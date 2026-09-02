use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
        Arc, Mutex, MutexGuard, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

pub const EXAMPLE_CONFIG: &str = include_str!("../../../config.example.yaml");

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub repos: Vec<RepoConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RepoConfig {
    pub name: String,
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_command: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoStatus {
    pub name: String,
    pub path: PathBuf,
    pub path_exists: bool,
    pub branch: Option<String>,
    pub clean: bool,
    pub on_main: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitInfo {
    pub sha: String,
    pub timestamp: i64,
    pub date: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoCommits {
    pub name: String,
    pub path: PathBuf,
    pub local: Option<CommitInfo>,
    pub remote: Option<CommitInfo>,
    pub difference: Option<String>,
    pub error: Option<String>,
}

const MAX_COMMIT_WORKERS: usize = 8;
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
const PULL_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncOutcome {
    Succeeded,
    SkippedDirty,
    SkippedBranch,
    PullFailed,
    BuildFailed,
    StatusFailed,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncResult {
    pub name: String,
    pub path: PathBuf,
    pub outcome: SyncOutcome,
    pub branch: Option<String>,
    pub pull_output: Option<String>,
    pub build_output: Option<String>,
    pub message: String,
}

pub fn config_path() -> Result<PathBuf> {
    let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".config/pullkit/config.yaml"))
}

pub fn initialize_config() -> Result<Option<PathBuf>> {
    let path = config_path()?;
    let created = initialize_config_at(&path)?;
    Ok(created.then_some(path))
}

fn initialize_config_at(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }

    let directory = path
        .parent()
        .ok_or_else(|| anyhow!("config path has no parent: {}", path.display()))?;
    fs::create_dir_all(directory).with_context(|| {
        format!(
            "could not create config directory at {}",
            directory.display()
        )
    })?;
    fs::write(path, EXAMPLE_CONFIG)
        .with_context(|| format!("could not create sample config at {}", path.display()))?;
    Ok(true)
}

pub fn load_config() -> Result<Config> {
    load_config_from(&config_path()?)
}

pub fn load_config_from(path: &Path) -> Result<Config> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("could not read config at {}", path.display()))?;
    let mut config: Config = serde_yaml::from_str(&contents)
        .with_context(|| format!("invalid YAML in {}", path.display()))?;
    expand_repo_paths(&mut config)?;
    validate_config(&config)?;
    Ok(config)
}

fn expand_repo_paths(config: &mut Config) -> Result<()> {
    if !config.repos.iter().any(|repo| repo.path.starts_with("~")) {
        return Ok(());
    }

    let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    expand_repo_paths_from(config, Path::new(&home));
    Ok(())
}

fn expand_repo_paths_from(config: &mut Config, home: &Path) {
    for repo in &mut config.repos {
        let Ok(relative) = repo.path.strip_prefix("~") else {
            continue;
        };
        repo.path = home.join(relative);
    }
}

fn validate_config(config: &Config) -> Result<()> {
    for (index, repo) in config.repos.iter().enumerate() {
        if repo.name.trim().is_empty() {
            return Err(anyhow!("repo {} has an empty name", index + 1));
        }
        if repo.path.as_os_str().is_empty() {
            return Err(anyhow!("repo '{}' has an empty path", repo.name));
        }
        if config.repos[..index]
            .iter()
            .any(|other| other.name == repo.name)
        {
            return Err(anyhow!("duplicate repo name '{}'", repo.name));
        }
        // One directory listed twice would be inspected, pulled, and built twice.
        if let Some(other) = config.repos[..index]
            .iter()
            .find(|other| same_directory(&other.path, &repo.path))
        {
            return Err(anyhow!(
                "repos '{}' and '{}' point at the same directory",
                other.name,
                repo.name
            ));
        }
    }
    Ok(())
}

fn same_directory(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

pub fn inspect(repo: &RepoConfig) -> RepoStatus {
    let path_exists = repo.path.exists();
    match inspect_inner(repo) {
        Ok(status) => status,
        Err(error) => RepoStatus {
            name: repo.name.clone(),
            path: repo.path.clone(),
            path_exists,
            branch: None,
            clean: false,
            on_main: false,
            error: Some(format!("{error:#}")),
        },
    }
}

fn inspect_inner(repo: &RepoConfig) -> Result<RepoStatus> {
    if !repo.path.is_dir() {
        return Err(anyhow!("directory does not exist: {}", repo.path.display()));
    }

    let branch_output = git(&repo.path, &["branch", "--show-current"])?;
    ensure_success(&branch_output, "read current branch")?;
    let branch = String::from_utf8_lossy(&branch_output.stdout)
        .trim()
        .to_owned();
    if branch.is_empty() {
        return Err(anyhow!("repository is in detached HEAD state"));
    }

    let status_output = git(&repo.path, &["status", "--porcelain"])?;
    ensure_success(&status_output, "read working tree status")?;
    let clean = status_output.stdout.is_empty();
    let on_main = branch == "main" || branch == "master";

    Ok(RepoStatus {
        name: repo.name.clone(),
        path: repo.path.clone(),
        path_exists: true,
        branch: Some(branch),
        clean,
        on_main,
        error: None,
    })
}

/// Hands out one lock per git directory. Exactly two commands take it: the
/// `git fetch` of an inspection, which reads `FETCH_HEAD` straight back, and
/// the `git pull` of a sync, whose own fetch writes that same file. Everything
/// else git is asked to do here only reads, and runs unlocked. A second copy of
/// pullkit is not covered either: the lock spans this process only.
///
/// The key is the per-worktree git directory rather than the shared one a
/// linked worktree points at, because each worktree keeps its own `FETCH_HEAD`.
/// Entries can still land on one key without naming the same directory, when
/// one of them sits inside the other's work tree.
#[derive(Default)]
struct RepoLocks {
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

static REPO_LOCKS: OnceLock<RepoLocks> = OnceLock::new();

/// Process ids of the background fetches that are running right now. Quitting
/// during a list would otherwise leave them behind: each one has a process
/// group of its own, so nothing signals it, and the thread that enforces its
/// timeout dies with this process.
static RUNNING_FETCHES: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Set while a list is being abandoned, so that the workers stop taking jobs.
/// Killing only what is running would not be enough with more repositories than
/// workers: a worker whose fetch was killed would go on to start the next one.
static ABANDONED: AtomicBool = AtomicBool::new(false);

/// Stops the inspection of a list: no further fetch is started, and the ones
/// already running are killed. Call it on the way out, before this process
/// exits. A later call to `inspect_commits_parallel` starts afresh.
pub fn terminate_running_fetches() {
    ABANDONED.store(true, Ordering::SeqCst);
    let running = std::mem::take(&mut *lock_ignoring_poison(&RUNNING_FETCHES));
    for pid in running {
        kill_process_group(pid);
    }
}

/// Poisoning carries no meaning here: these locks guard `()`, so a panic under
/// one leaves nothing half-written for the next holder to find.
fn lock_ignoring_poison<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn repo_lock(path: &Path) -> Arc<Mutex<()>> {
    let key = git_directory(path).unwrap_or_else(|| path.to_path_buf());
    let locks = REPO_LOCKS.get_or_init(RepoLocks::default);
    let mut locks = lock_ignoring_poison(&locks.locks);
    Arc::clone(locks.entry(key).or_default())
}

fn git_directory(path: &Path) -> Option<PathBuf> {
    let output = git(path, &["rev-parse", "--absolute-git-dir"]).ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then(|| PathBuf::from(text))
}

pub fn inspect_commits(repo: &RepoConfig) -> RepoCommits {
    let mut commits = RepoCommits {
        name: repo.name.clone(),
        path: repo.path.clone(),
        local: None,
        remote: None,
        difference: None,
        error: None,
    };
    if !repo.path.is_dir() {
        commits.error = Some(format!("directory does not exist: {}", repo.path.display()));
        return commits;
    }

    let local = match read_commit(&repo.path, "HEAD") {
        Ok(local) => local,
        Err(error) => {
            commits.error = Some(format!("{error:#}"));
            return commits;
        }
    };
    let remote = {
        let lock = repo_lock(&repo.path);
        let _held = lock_ignoring_poison(&lock);
        fetch_remote_head(&repo.path).and_then(|()| read_commit(&repo.path, "FETCH_HEAD"))
    };
    match remote {
        Ok(remote) => {
            match commit_relation(&repo.path, &local, &remote) {
                Ok(relation) => {
                    commits.difference = Some(describe_difference(&local, &remote, relation))
                }
                Err(error) => commits.error = Some(format!("{error:#}")),
            }
            commits.remote = Some(remote);
        }
        Err(error) => commits.error = Some(format!("{error:#}")),
    }
    commits.local = Some(local);
    commits
}

/// Inspects every repository on a bounded worker pool and calls `on_result`
/// on the calling thread as each result arrives, in completion order.
pub fn inspect_commits_parallel<F>(repos: &[RepoConfig], mut on_result: F)
where
    F: FnMut(usize, RepoCommits),
{
    let (job_tx, job_rx) = mpsc::channel::<(usize, &RepoConfig)>();
    let (result_tx, result_rx) = mpsc::channel();
    for job in repos.iter().enumerate() {
        let _ = job_tx.send(job);
    }
    drop(job_tx);
    let job_rx = Mutex::new(job_rx);
    ABANDONED.store(false, Ordering::SeqCst);

    thread::scope(|scope| {
        for _ in 0..repos.len().min(MAX_COMMIT_WORKERS) {
            let result_tx = result_tx.clone();
            let job_rx = &job_rx;
            scope.spawn(move || loop {
                if ABANDONED.load(Ordering::SeqCst) {
                    return;
                }
                let Ok((index, repo)) = lock_ignoring_poison(job_rx).recv() else {
                    return;
                };
                if result_tx.send((index, inspect_commits(repo))).is_err() {
                    return;
                }
            });
        }
        drop(result_tx);
        for (index, commits) in result_rx {
            on_result(index, commits);
        }
    });
}

fn read_commit(path: &Path, reference: &str) -> Result<CommitInfo> {
    let output = git(
        path,
        &[
            "log",
            "-1",
            "--format=%H%n%ct%n%cd",
            "--date=format-local:%Y-%m-%d %H:%M",
            reference,
        ],
    )?;
    ensure_success(&output, &format!("read commit {reference}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let (Some(sha), Some(timestamp), Some(date)) = (lines.next(), lines.next(), lines.next())
    else {
        return Err(anyhow!("unexpected git log output for {reference}: {text}"));
    };
    Ok(CommitInfo {
        sha: sha.to_owned(),
        timestamp: timestamp
            .parse()
            .with_context(|| format!("invalid commit timestamp '{timestamp}'"))?,
        date: date.to_owned(),
    })
}

fn fetch_remote_head(path: &Path) -> Result<()> {
    let output = git_offline(
        path,
        &["fetch", "--quiet", "origin", "HEAD"],
        FETCH_TIMEOUT,
        Isolation::OwnGroup,
    )
    .with_context(|| format!("failed to fetch in {}", path.display()))?;
    ensure_success(&output, "fetch origin HEAD")
}

/// Whether the command gets a process group of its own.
#[derive(Clone, Copy)]
enum Isolation {
    /// For a command running out of sight: the timeout can then take the whole
    /// group, so no helper of a killed command keeps working in the repository.
    OwnGroup,
    /// For a command a terminal is watching, so that Ctrl-C still reaches it.
    SharedGroup,
}

/// Runs a git command that talks to a remote, with no way for it to ask a
/// question. A prompt would go unanswered in the GUI, which has no terminal at
/// all, and in the terminal it would sit under a repainting list or hold up
/// every repository behind it, so failing and moving on is the better outcome.
fn git_offline(
    path: &Path,
    args: &[&str],
    timeout: Duration,
    isolation: Isolation,
) -> Result<Output> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(path)
        .env("GIT_TERMINAL_PROMPT", "0");
    // Setting GIT_SSH_COMMAND takes precedence over GIT_SSH and over
    // core.sshCommand, which would break a repository that configures its own
    // key, port, or wrapper, so it is only set when nothing else chose one.
    if env::var_os("GIT_SSH_COMMAND").is_none()
        && env::var_os("GIT_SSH").is_none()
        && !has_configured_ssh_command(path)
    {
        command.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    output_with_timeout(command, timeout, isolation)
}

fn has_configured_ssh_command(path: &Path) -> bool {
    git(path, &["config", "--get", "core.sshCommand"]).is_ok_and(|output| !output.stdout.is_empty())
}

const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// Reads a pipe to the end on its own thread. Each pipe needs a thread of its
/// own: draining them one after the other lets the command fill the second pipe
/// and block, while this side is still waiting for the first to reach EOF.
fn drain(mut pipe: impl Read + Send + 'static) -> Result<Receiver<Vec<u8>>> {
    let (tx, rx) = mpsc::channel();
    // `thread::Builder` reports a thread that could not be started rather than
    // panicking, which would unwind past the child and leave it running.
    thread::Builder::new()
        .spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            let _ = tx.send(buffer);
        })
        .context("failed to start a thread to read git output")?;
    Ok(rx)
}

/// Puts the command in a process group of its own, so that killing the group
/// takes the helpers it started, such as `ssh`, with it. A surviving helper
/// would keep working inside the repository a sync is about to pull into.
#[cfg(unix)]
fn own_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn own_process_group(_command: &mut Command) {}

/// Kills the command. One given a group of its own is killed by group, so the
/// helpers it started go with it. One sharing this process's group is killed on
/// its own: its pid names no group, and signalling the group it is really in
/// would take pullkit down with it. Its helpers are then left to notice that
/// git is gone, which is the price of letting Ctrl-C reach it.
#[cfg(unix)]
fn kill_command(child: &mut Child, isolation: Isolation) {
    if matches!(isolation, Isolation::OwnGroup) {
        kill_process_group(child.id());
    }
    let _ = child.kill();
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Windows has no process group to signal here, so only the command itself is
/// stopped and a helper it started can outlive it.
#[cfg(windows)]
fn kill_command(child: &mut Child, _isolation: Isolation) {
    let _ = child.kill();
}

/// Windows has no process group here, so a fetch left behind by a quit cannot
/// be reached once its `Child` is gone.
#[cfg(windows)]
fn kill_process_group(_pid: u32) {}

/// Collects the child, giving up rather than waiting without a bound. A child
/// that outlasts the wait is left for the operating system to reap when this
/// process exits.
fn reap(child: &mut Child) {
    let deadline = Instant::now() + REAP_TIMEOUT;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            _ => return,
        }
    }
}

/// Runs the command and kills it once `timeout` elapses, taking the helpers it
/// started when, and only when, it has a process group of its own. An unreachable remote would
/// otherwise leave `git fetch` waiting for a connection that never completes,
/// and a list or a sync would wait with it. A killed command yields no output,
/// and a `git fetch` killed part way through can leave whatever it had already
/// written to `FETCH_HEAD`.
fn output_with_timeout(
    mut command: Command,
    timeout: Duration,
    isolation: Isolation,
) -> Result<Output> {
    if matches!(isolation, Isolation::OwnGroup) {
        own_process_group(&mut command);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run git")?;
    let pipes = drain(child.stdout.take().expect("stdout is piped above")).and_then(|stdout| {
        Ok((
            stdout,
            drain(child.stderr.take().expect("stderr is piped"))?,
        ))
    });
    let (stdout, stderr) = match pipes {
        Ok(pipes) => pipes,
        Err(error) => {
            kill_command(&mut child, isolation);
            reap(&mut child);
            return Err(error);
        }
    };

    if matches!(isolation, Isolation::OwnGroup) {
        lock_ignoring_poison(&RUNNING_FETCHES).push(child.id());
    }
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                kill_command(&mut child, isolation);
                reap(&mut child);
                forget_running(&child, isolation);
                return Err(anyhow!("git timed out after {} seconds", timeout.as_secs()));
            }
            Err(error) => {
                kill_command(&mut child, isolation);
                reap(&mut child);
                forget_running(&child, isolation);
                return Err(error).context("failed to wait for git");
            }
        }
    };
    forget_running(&child, isolation);
    Ok(Output {
        status,
        stdout: stdout.recv_timeout(DRAIN_TIMEOUT).unwrap_or_default(),
        stderr: stderr.recv_timeout(DRAIN_TIMEOUT).unwrap_or_default(),
    })
}

fn forget_running(child: &Child, isolation: Isolation) {
    if matches!(isolation, Isolation::SharedGroup) {
        return;
    }
    let mut running = lock_ignoring_poison(&RUNNING_FETCHES);
    if let Some(index) = running.iter().position(|pid| *pid == child.id()) {
        running.swap_remove(index);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitRelation {
    Same,
    Behind,
    Ahead,
    Diverged,
}

/// Commit dates are arbitrary, so the ancestry decides which side is ahead and
/// the dates only supply the size of the gap.
fn commit_relation(path: &Path, local: &CommitInfo, remote: &CommitInfo) -> Result<CommitRelation> {
    if local.sha == remote.sha {
        return Ok(CommitRelation::Same);
    }
    if is_ancestor(path, &local.sha, &remote.sha)? {
        return Ok(CommitRelation::Behind);
    }
    if is_ancestor(path, &remote.sha, &local.sha)? {
        return Ok(CommitRelation::Ahead);
    }
    Ok(CommitRelation::Diverged)
}

/// `merge-base --is-ancestor` answers "no" with exit code 1, so only that code
/// means the commit is not an ancestor; any other failure must not read as a
/// divergence.
fn is_ancestor(path: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = git(path, &["merge-base", "--is-ancestor", ancestor, descendant])?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(anyhow!(
            "git could not compare {ancestor} with {descendant}: {}",
            combined_output(&output)
        )),
    }
}

fn describe_difference(
    local: &CommitInfo,
    remote: &CommitInfo,
    relation: CommitRelation,
) -> String {
    let gap = humanize_seconds((remote.timestamp - local.timestamp).unsigned_abs());
    match relation {
        CommitRelation::Same => "up to date".into(),
        CommitRelation::Behind => format!("{gap} behind"),
        CommitRelation::Ahead => format!("{gap} ahead"),
        CommitRelation::Diverged => format!("diverged by {gap}"),
    }
}

fn humanize_seconds(seconds: u64) -> String {
    const UNITS: [(u64, &str); 6] = [
        (365 * 86_400, "year"),
        (30 * 86_400, "month"),
        (7 * 86_400, "week"),
        (86_400, "day"),
        (3_600, "hour"),
        (60, "min"),
    ];
    for (length, unit) in UNITS {
        if seconds < length {
            continue;
        }
        let count = seconds / length;
        let plural = if count == 1 || unit == "min" { "" } else { "s" };
        return format!("{count} {unit}{plural}");
    }
    "<1 min".into()
}

pub fn sync_repo<F>(repo: &RepoConfig, mut log: F) -> SyncResult
where
    F: FnMut(String),
{
    log(format!("[{}] checking repository", repo.name));
    let status = inspect(repo);
    if let Some(error) = status.error {
        let message = format!("status check failed: {error}");
        log(format!("[{}] ERROR {message}", repo.name));
        return result(repo, SyncOutcome::StatusFailed, None, message);
    }
    if !status.clean {
        let message = "skipped: working tree has uncommitted changes".to_owned();
        log(format!("[{}] WARN {message}", repo.name));
        return result(repo, SyncOutcome::SkippedDirty, status.branch, message);
    }
    if !status.on_main {
        let branch = status.branch.unwrap_or_else(|| "unknown".into());
        let message = format!("skipped: current branch is '{branch}', not main/master");
        log(format!("[{}] WARN {message}", repo.name));
        return result(repo, SyncOutcome::SkippedBranch, Some(branch), message);
    }

    let branch = status.branch;
    log(format!("[{}] pulling from origin", repo.name));
    // The pull holds the repository's lock so that it cannot run while a
    // background inspection is fetching into the same `.git`, and it carries a
    // timeout so that a pull nothing can finish does not hold that lock for the
    // rest of the session.
    let lock = repo_lock(&repo.path);
    let held = lock_ignoring_poison(&lock);
    let pull = match git_offline(
        &repo.path,
        &["pull", "origin", "HEAD"],
        PULL_TIMEOUT,
        Isolation::SharedGroup,
    ) {
        Ok(output) => output,
        Err(error) => {
            let message = format!("git pull did not run to completion: {error:#}");
            log(format!("[{}] ERROR {message}", repo.name));
            return result(repo, SyncOutcome::PullFailed, branch, message);
        }
    };
    drop(held);
    let pull_text = combined_output(&pull);
    if !pull.status.success() {
        let message = format!("git pull failed ({})", exit_label(&pull));
        log(format!(
            "[{}] ERROR {message}\n{}",
            repo.name,
            indent(&pull_text)
        ));
        let mut item = result(repo, SyncOutcome::PullFailed, branch, message);
        item.pull_output = Some(pull_text);
        return item;
    }
    if !pull_text.is_empty() {
        log(format!(
            "[{}] {}",
            repo.name,
            pull_text.replace('\n', "\n    ")
        ));
    }

    if let Some(command) = repo
        .build_command
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        log(format!("[{}] building: {command}", repo.name));
        let build = match shell_command(command, &repo.path) {
            Ok(output) => output,
            Err(error) => {
                let message = format!("could not start build: {error:#}");
                log(format!("[{}] ERROR {message}", repo.name));
                let mut item = result(repo, SyncOutcome::BuildFailed, branch, message);
                item.pull_output = Some(pull_text);
                return item;
            }
        };
        let build_text = combined_output(&build);
        if !build.status.success() {
            let message = format!("build failed ({})", exit_label(&build));
            log(format!(
                "[{}] ERROR {message}\n{}",
                repo.name,
                indent(&build_text)
            ));
            let mut item = result(repo, SyncOutcome::BuildFailed, branch, message);
            item.pull_output = Some(pull_text);
            item.build_output = Some(build_text);
            return item;
        }
        if !build_text.is_empty() {
            log(format!(
                "[{}] {}",
                repo.name,
                build_text.replace('\n', "\n    ")
            ));
        }
    }

    let message = "sync completed".to_owned();
    log(format!("[{}] OK {message}", repo.name));
    let mut item = result(repo, SyncOutcome::Succeeded, branch, message);
    item.pull_output = Some(pull_text);
    item
}

pub fn sync_repos<F>(repos: &[RepoConfig], mut log: F) -> Vec<SyncResult>
where
    F: FnMut(String),
{
    repos.iter().map(|repo| sync_repo(repo, &mut log)).collect()
}

fn result(
    repo: &RepoConfig,
    outcome: SyncOutcome,
    branch: Option<String>,
    message: String,
) -> SyncResult {
    SyncResult {
        name: repo.name.clone(),
        path: repo.path.clone(),
        outcome,
        branch,
        pull_output: None,
        build_output: None,
        message,
    }
}

fn git(path: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .with_context(|| format!("failed to run git in {}", path.display()))
}

#[cfg(unix)]
fn shell_command(command: &str, path: &Path) -> Result<Output> {
    Command::new("sh")
        .args(["-lc", command])
        .current_dir(path)
        .output()
        .context("failed to run shell")
}

#[cfg(windows)]
fn shell_command(command: &str, path: &Path) -> Result<Output> {
    Command::new("cmd")
        .args(["/C", command])
        .current_dir(path)
        .output()
        .context("failed to run shell")
}

fn ensure_success(output: &Output, action: &str) -> Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "git could not {action}: {}",
            combined_output(output)
        ))
    }
}

fn combined_output(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("{}{}", stdout, stderr).trim().to_owned()
}

fn exit_label(output: &Output) -> String {
    output.status.code().map_or_else(
        || "terminated by signal".into(),
        |code| format!("exit {code}"),
    )
}

fn indent(text: &str) -> String {
    if text.is_empty() {
        "    (no output)".into()
    } else {
        format!("    {}", text.replace('\n', "\n    "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parses_example_config() {
        let config: Config = serde_yaml::from_str(
            "repos:\n  - name: myapp\n    path: /tmp/myapp\n    build_command: cargo build\n",
        )
        .unwrap();
        assert_eq!(config.repos[0].name, "myapp");
        assert_eq!(
            config.repos[0].build_command.as_deref(),
            Some("cargo build")
        );
    }

    #[test]
    fn initializes_missing_config_without_overwriting_it() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = env::temp_dir().join(format!("pullkit-{unique}"));
        let path = directory.join("nested/config.yaml");

        assert!(initialize_config_at(&path).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), EXAMPLE_CONFIG);

        fs::write(&path, "repos: []\n").unwrap();
        assert!(!initialize_config_at(&path).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "repos: []\n");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn expands_current_user_tilde_in_repo_paths() {
        let mut config: Config = serde_yaml::from_str(
            "repos:\n  - name: home\n    path: ~\n  - name: nested\n    path: ~/workspace/app\n  - name: absolute\n    path: /tmp/app\n  - name: another-user\n    path: ~someone/app\n",
        )
        .unwrap();

        expand_repo_paths_from(&mut config, Path::new("/Users/example"));

        assert_eq!(config.repos[0].path, Path::new("/Users/example"));
        assert_eq!(
            config.repos[1].path,
            Path::new("/Users/example/workspace/app")
        );
        assert_eq!(config.repos[2].path, Path::new("/tmp/app"));
        assert_eq!(config.repos[3].path, Path::new("~someone/app"));
    }

    fn commit(sha: &str, timestamp: i64) -> CommitInfo {
        CommitInfo {
            sha: sha.into(),
            timestamp,
            date: String::new(),
        }
    }

    #[test]
    fn humanizes_durations_with_the_largest_whole_unit() {
        assert_eq!(humanize_seconds(0), "<1 min");
        assert_eq!(humanize_seconds(59), "<1 min");
        assert_eq!(humanize_seconds(60), "1 min");
        assert_eq!(humanize_seconds(5 * 60), "5 min");
        assert_eq!(humanize_seconds(3_600), "1 hour");
        assert_eq!(humanize_seconds(5 * 3_600 + 59 * 60), "5 hours");
        assert_eq!(humanize_seconds(86_400), "1 day");
        assert_eq!(humanize_seconds(6 * 86_400), "6 days");
        assert_eq!(humanize_seconds(7 * 86_400), "1 week");
        assert_eq!(humanize_seconds(29 * 86_400), "4 weeks");
        assert_eq!(humanize_seconds(30 * 86_400), "1 month");
        assert_eq!(humanize_seconds(364 * 86_400), "12 months");
        assert_eq!(humanize_seconds(365 * 86_400), "1 year");
        assert_eq!(humanize_seconds(2 * 365 * 86_400), "2 years");
    }

    #[test]
    fn describes_difference_from_the_ancestry_and_the_date_gap() {
        // Arrange
        let local = commit("aaa", 1_000_000);
        let same = commit("aaa", 1_000_000);
        let newer = commit("bbb", 1_000_000 + 2 * 86_400);
        let older = commit("bbb", 1_000_000 - 3 * 3_600);

        // Act & Assert
        assert_eq!(
            describe_difference(&local, &same, CommitRelation::Same),
            "up to date"
        );
        assert_eq!(
            describe_difference(&local, &newer, CommitRelation::Behind),
            "2 days behind"
        );
        assert_eq!(
            describe_difference(&local, &older, CommitRelation::Ahead),
            "3 hours ahead"
        );
        assert_eq!(
            describe_difference(&local, &newer, CommitRelation::Diverged),
            "diverged by 2 days"
        );
    }

    #[test]
    fn reads_the_local_head_commit_of_this_repository() {
        // Arrange
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");

        // Act
        let local = read_commit(&path, "HEAD").expect("HEAD is readable");

        // Assert
        assert_eq!(local.sha.len(), 40);
        assert!(local.timestamp > 0);
        assert_eq!(local.date.len(), "YYYY-MM-DD HH:MM".len());
    }

    #[test]
    fn rejects_two_repos_that_point_at_the_same_directory() {
        // Arrange
        let directory = env::temp_dir().display().to_string();
        let config: Config = serde_yaml::from_str(&format!(
            "repos:\n  - name: first\n    path: {directory}\n  - name: second\n    path: {directory}/.\n"
        ))
        .unwrap();

        // Act
        let error = validate_config(&config).expect_err("the duplicate path is rejected");

        // Assert
        assert!(error.to_string().contains("same directory"));
    }

    #[test]
    fn reports_an_unreadable_ancestry_instead_of_calling_it_diverged() {
        // Arrange
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let head = read_commit(&path, "HEAD").expect("HEAD is readable");
        let unknown = CommitInfo {
            sha: "0".repeat(40),
            timestamp: head.timestamp,
            date: head.date.clone(),
        };

        // Act
        let relation = commit_relation(&path, &head, &unknown);

        // Assert
        assert!(relation.is_err());
    }

    #[test]
    fn shares_one_lock_between_a_repository_and_a_directory_inside_it() {
        // Arrange
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let inside = root.join("crates");

        // Act
        let root_lock = repo_lock(&root);
        let inside_lock = repo_lock(&inside);

        // Assert
        assert_eq!(git_directory(&root), git_directory(&inside));
        assert!(Arc::ptr_eq(&root_lock, &inside_lock));
    }

    #[test]
    fn keeps_using_a_repository_lock_after_a_holder_panicked() {
        // Arrange
        let lock = repo_lock(&env::temp_dir());
        let poisoning = Arc::clone(&lock);
        let _ = thread::spawn(move || {
            let _held = poisoning.lock().unwrap();
            panic!("the holder panics while the lock is held");
        })
        .join();

        // Act
        let held = lock_ignoring_poison(&lock);

        // Assert
        drop(held);
        assert!(lock.is_poisoned());
    }

    #[test]
    fn relates_a_commit_to_itself_without_touching_the_remote() {
        // Arrange
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let head = read_commit(&path, "HEAD").expect("HEAD is readable");

        // Act
        let relation = commit_relation(&path, &head, &head).expect("the ancestry is readable");

        // Assert
        assert_eq!(relation, CommitRelation::Same);
    }

    #[cfg(unix)]
    #[test]
    fn kills_a_command_that_outlives_its_timeout() {
        // Arrange
        let mut command = Command::new("sleep");
        command.arg("30");
        let started = Instant::now();

        // Act
        let error = output_with_timeout(command, Duration::from_millis(300), Isolation::OwnGroup)
            .expect_err("the command outlives the timeout");

        // Assert
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn kills_the_helpers_that_the_command_started() {
        // Arrange
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 60 & sleep 60"])
            .stdout(Stdio::piped());
        own_process_group(&mut command);
        let mut child = command.spawn().unwrap();
        let group = child.id().to_string();
        thread::sleep(Duration::from_millis(300));

        // Act
        kill_command(&mut child, Isolation::OwnGroup);
        reap(&mut child);
        thread::sleep(Duration::from_millis(300));

        // Assert
        let survivors = Command::new("pgrep").args(["-g", &group]).output().unwrap();
        assert!(String::from_utf8_lossy(&survivors.stdout).trim().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn collects_output_larger_than_a_pipe_buffer_from_both_streams() {
        // Arrange
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "yes stderr | head -c 400000 >&2; yes stdout | head -c 400000",
        ]);

        // Act
        let output =
            output_with_timeout(command, Duration::from_secs(20), Isolation::SharedGroup).unwrap();

        // Assert
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 400_000);
        assert_eq!(output.stderr.len(), 400_000);
    }

    #[test]
    fn parallel_inspection_reports_every_repository_once() {
        // Arrange
        let repos: Vec<_> = (0..MAX_COMMIT_WORKERS + 3)
            .map(|index| RepoConfig {
                name: format!("missing-{index}"),
                path: env::temp_dir().join(format!("pullkit-missing-{index}")),
                build_command: None,
            })
            .collect();
        let mut seen = vec![0; repos.len()];

        // Act
        inspect_commits_parallel(&repos, |index, commits| {
            assert_eq!(commits.name, repos[index].name);
            assert!(commits.error.is_some());
            seen[index] += 1;
        });

        // Assert
        assert!(seen.iter().all(|count| *count == 1));
    }

    #[test]
    fn reports_whether_repo_path_exists() {
        let existing = inspect(&RepoConfig {
            name: "existing".into(),
            path: env::temp_dir(),
            build_command: None,
        });
        let missing = inspect(&RepoConfig {
            name: "missing".into(),
            path: env::temp_dir().join("pullkit-path-that-does-not-exist"),
            build_command: None,
        });

        assert!(existing.path_exists);
        assert!(!missing.path_exists);
    }
}
