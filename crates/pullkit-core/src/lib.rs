use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    env, fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, RecvTimeoutError, SyncSender},
        Arc, Mutex, MutexGuard, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

pub const EXAMPLE_CONFIG: &str = include_str!("../../../config.example.yaml");

/// How many repositories a sync works on at once when the configuration does
/// not say.
pub const DEFAULT_CONCURRENCY: usize = 4;
pub const MAX_CONCURRENCY: usize = 10;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// How many repositories a sync pulls and builds at the same time.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default)]
    pub repos: Vec<RepoConfig>,
}

fn default_concurrency() -> usize {
    DEFAULT_CONCURRENCY
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

/// Events a sync may have waiting for its caller before the workers wait.
pub const EVENT_QUEUE: usize = 4_096;

/// What a sync reports while it runs. `worker` numbers the slot that took the
/// repository, from zero, so a display can keep one place per slot.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncEvent {
    /// Sent once, first, with the number of slots this sync uses.
    Planned { workers: usize },
    /// A slot took a repository.
    Started { worker: usize, name: String },
    /// One line of log or command output from the repository the slot holds.
    Line {
        worker: usize,
        name: String,
        line: String,
    },
    /// The slot is done with the repository.
    Finished { worker: usize, result: SyncResult },
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
    validate_concurrency(config.concurrency)?;
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

pub fn validate_concurrency(concurrency: usize) -> Result<()> {
    if (1..=MAX_CONCURRENCY).contains(&concurrency) {
        return Ok(());
    }
    Err(anyhow!(
        "concurrency must be between 1 and {MAX_CONCURRENCY}, not {concurrency}"
    ))
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

/// Hands out one lock per git directory. Two things take it: the `git fetch`
/// of an inspection, which reads `FETCH_HEAD` straight back, and a sync of the
/// repository, from its status check through its pull, whose own fetch writes
/// that same file, to its build. The sync holds it that long because two
/// entries can share one repository, and with several syncs running at once
/// the pull of one must not change the tree the build of the other is reading.
/// Everything else git is asked to do here only reads, and runs unlocked. A
/// second copy of pullkit is not covered either: the lock spans this process
/// only.
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

/// Process ids of the commands running in a process group of their own right
/// now: every git command that only reads, the fetches of a list, and the pulls
/// and builds of a sync that a raw mode terminal or the GUI is watching. Only
/// the pulls and builds of a sync with no terminal screen stay off it, in this
/// process's own group. Quitting would otherwise leave them behind: nothing
/// signals a group of its own, and the thread that enforces a timeout dies with
/// this process. A group stays listed while any process in it lives, so a
/// process a build left behind is on it after the build itself has ended.
static RUNNING_COMMANDS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Set while a list or a sync is being abandoned, so that the workers stop
/// taking jobs. Killing only what is running would not be enough with more
/// repositories than workers: a worker whose command was killed would go on to
/// start the next one.
///
/// It is never cleared. Every abort here is followed by the end of the process,
/// and clearing it at the start of a run would let a worker thread that
/// happened to start late undo an abort issued a moment before. Should a run
/// ever need to follow an abort inside one process, clearing it is the caller's
/// job, on the thread that issues aborts and before the new run is started.
static ABANDONED: AtomicBool = AtomicBool::new(false);

/// Process groups whose command has ended, or was killed and given up on,
/// while something in the group lives on: a helper, or a process a build
/// started in the background. They are kept
/// apart from the running commands so that the end of a run can stop them
/// without touching a command another run may have started in the meantime.
static LEFTOVER_GROUPS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Commands being started right now, spawned but not yet on
/// `RUNNING_COMMANDS`. A stop that found the list empty would otherwise return,
/// and the process end, while such a command was about to be registered.
static SPAWNING: AtomicUsize = AtomicUsize::new(0);

/// Stops a list or a sync: no further repository is started, and the commands
/// running in a group of their own are killed. A command sharing this process's
/// group is not touched, because the terminal that put it there delivers Ctrl-C
/// to it directly. Call it on the way out, before this process exits.
pub fn terminate_running_commands() {
    ABANDONED.store(true, Ordering::SeqCst);
    signal_listed(Signal::Kill, &RUNNING_COMMANDS);
    signal_listed(Signal::Kill, &LEFTOVER_GROUPS);
}

/// Asks a list or a sync to stop: no further repository is started, and the
/// commands running in a group of their own get the signal Ctrl-C would have
/// given them. Unlike `terminate_running_commands`, this leaves them time to
/// clean up: a `git pull` told to stop removes its lock files, one killed
/// outright leaves them behind. The commands stay on the list, so a
/// `terminate_running_commands` afterwards still reaches one that ignored the
/// request.
///
/// Groups that have emptied since they were listed are dropped first, here and
/// in `terminate_running_commands`: once a group is gone its id can be given to
/// an unrelated process, and that process must not get the signal.
pub fn interrupt_running_commands() {
    ABANDONED.store(true, Ordering::SeqCst);
    signal_listed(Signal::Interrupt, &RUNNING_COMMANDS);
    signal_listed(Signal::Interrupt, &LEFTOVER_GROUPS);
}

/// Sends the signal to every group on the list that still has a member, and
/// drops the ones that have emptied. The list is not held while the signals
/// go out: on Windows a signal is a `taskkill` to wait for, and a thread
/// registering or releasing a command must not wait behind it.
fn signal_listed(signal: Signal, list: &Mutex<Vec<u32>>) {
    let alive = {
        let mut listed = lock_ignoring_poison(list);
        listed.retain(|pid| listed_group_alive(*pid));
        listed.clone()
    };
    signal_process_groups(&alive, signal);
}

/// Stops a list or a sync for good, on the way out of the process: asks the
/// running commands to stop, gives them `grace` to do so, and kills whatever is
/// still running after that. A request alone is not enough here, because a
/// build that ignores it would outlive the window or the screen that started
/// it, with nothing left to press Ctrl-C at.
///
/// A group stays on the list while any process in it lives, so a helper or a
/// background process a build left behind is stopped here too; only a group
/// that has emptied on its own is let go.
pub fn stop_running_commands(grace: Duration) {
    // A command spawned in the last moment is registered, seen as abandoned,
    // and killed by the thread that started it, all while it counts as
    // spawning; the waits inside allow that thread a moment to get there.
    stop_listed(grace);
}

/// Stops what a finished run left behind: a process a build started in the
/// background, or a helper of a hook, still alive in a group that the run is
/// done with. Nothing is abandoned by this, so the process can go on to
/// another run; it is for the end of a sync, where every command of the run has
/// returned and whatever is still listed is a leftover. A build that means to
/// leave a process behind has to put it in a session of its own, with `setsid`
/// or the like.
pub fn stop_leftover_commands(grace: Duration) {
    signal_listed(Signal::Interrupt, &LEFTOVER_GROUPS);
    if wait_for_list(&LEFTOVER_GROUPS, grace) {
        return;
    }
    signal_listed(Signal::Kill, &LEFTOVER_GROUPS);
    wait_for_list(&LEFTOVER_GROUPS, SPAWN_GRACE);
}

fn stop_listed(grace: Duration) {
    interrupt_running_commands();
    if wait_for_running_commands(grace) {
        return;
    }
    terminate_running_commands();
    wait_for_running_commands(SPAWN_GRACE);
}

/// Waits for the list to empty, dropping groups that have ended on their own.
/// Returns whether it emptied within `grace`.
fn wait_for_list(list: &Mutex<Vec<u32>>, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        let mut listed = lock_ignoring_poison(list);
        listed.retain(|pid| listed_group_alive(*pid));
        let empty = listed.is_empty();
        drop(listed);
        if empty {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Waits for the running list to empty, dropping groups that have ended on
/// their own, and for no command to be part way through starting. Returns
/// whether that came about within `grace`.
fn wait_for_running_commands(grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        let running = {
            let mut running = lock_ignoring_poison(&RUNNING_COMMANDS);
            running.retain(|pid| listed_group_alive(*pid));
            running.len()
        };
        let leftover = {
            let mut leftover = lock_ignoring_poison(&LEFTOVER_GROUPS);
            leftover.retain(|pid| listed_group_alive(*pid));
            leftover.len()
        };
        if running + leftover == 0 && SPAWNING.load(Ordering::SeqCst) == 0 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// How long a stop allows for a command that was starting as the stop came in.
const SPAWN_GRACE: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
enum Signal {
    Interrupt,
    Kill,
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
        Some(FETCH_TIMEOUT),
        Isolation::OwnGroup,
        &mut |_| {},
    )
    .with_context(|| format!("failed to fetch in {}", path.display()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(anyhow!(
        "git could not fetch origin HEAD: {}",
        output.text.trim()
    ))
}

/// Whether a command gets a process group of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    /// For a command no terminal delivers Ctrl-C to: one running behind a list,
    /// or under a raw mode screen, where Ctrl-C arrives as a key press and not
    /// as a signal. A timeout or `terminate_running_commands` can then take the
    /// whole group, so no helper of a killed command keeps working in the
    /// repository.
    OwnGroup,
    /// For a command a terminal is watching in cooked mode, so that Ctrl-C still
    /// reaches it.
    SharedGroup,
}

/// Runs a git command that talks to a remote, with no way for it to ask a
/// question. A prompt would go unanswered in the GUI, which has no terminal at
/// all, and in the terminal it would sit under a repainting list or hold up
/// every repository behind it, so failing and moving on is the better outcome.
fn git_offline(
    path: &Path,
    args: &[&str],
    timeout: Option<Duration>,
    isolation: Isolation,
    on_line: &mut dyn FnMut(String),
) -> Result<CommandOutput> {
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
    run_with_timeout(command, timeout, isolation, on_line)
}

fn has_configured_ssh_command(path: &Path) -> bool {
    git(path, &["config", "--get", "core.sshCommand"]).is_ok_and(|output| !output.stdout.is_empty())
}

const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Lines of a command's output kept for the result, beyond what was handed out
/// as it ran. A build has no timeout and can write without bound.
const RETAINED_LINES: usize = 2_000;
/// The bound on a git command that only reads. One of those can still hang, on
/// a hook or a file system monitor that does not answer, and a sync holds the
/// repository's lock while it waits.
const READ_TIMEOUT: Duration = Duration::from_secs(120);
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// What a pipe reader sends back: the lines as they come, then one `Eof`.
enum PipeMessage {
    Line(String),
    Eof,
}

/// Reads a pipe line by line on its own thread. Each pipe needs a thread of its
/// own: draining them one after the other lets the command fill the second pipe
/// and block, while this side is still waiting for the first to reach EOF.
fn stream(pipe: impl Read + Send + 'static, tx: SyncSender<PipeMessage>) -> Result<()> {
    // `thread::Builder` reports a thread that could not be started rather than
    // panicking, which would unwind past the child and leave it running.
    thread::Builder::new()
        .spawn(move || {
            let mut reader = BufReader::new(pipe);
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                match read_line_bounded(&mut reader, &mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if tx.send(PipeMessage::Line(output_line(&buffer))).is_err() {
                    return;
                }
            }
            let _ = tx.send(PipeMessage::Eof);
        })
        .context("failed to start a thread to read command output")?;
    Ok(())
}

/// The most of one line that is kept in memory. Output without a newline, a
/// progress bar that only ever redraws itself or a dump of binary data, would
/// otherwise be held whole until the command ends.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// Reads up to and including the next newline, or `MAX_LINE_BYTES` if no
/// newline comes first; the rest of such a line arrives as further lines.
/// Returns the number of bytes read, zero at the end of the pipe.
fn read_line_bounded(reader: &mut impl BufRead, buffer: &mut Vec<u8>) -> std::io::Result<usize> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(buffer.len());
        }
        let room = MAX_LINE_BYTES - buffer.len();
        let (taken, done) = match available.iter().position(|byte| *byte == b'\n') {
            Some(index) if index < room => (index + 1, true),
            _ => (available.len().min(room), false),
        };
        buffer.extend_from_slice(&available[..taken]);
        reader.consume(taken);
        if done || buffer.len() == MAX_LINE_BYTES {
            return Ok(buffer.len());
        }
    }
}

/// Turns raw bytes of one line into text. A progress bar redraws itself with
/// carriage returns rather than new lines, so only what came after the last one
/// is kept: that is what a terminal would have left on screen.
fn output_line(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let text = text.trim_end_matches(['\n', '\r']);
    text.rsplit('\r').next().unwrap_or_default().to_owned()
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
        signal_process_group(child.id(), Signal::Kill);
    }
    let _ = child.kill();
}

/// Signals the groups directly rather than through a `kill` binary: there is no
/// binary to go missing or hang, and nothing waits on a subprocess while the
/// running list is held.
#[cfg(unix)]
fn signal_process_groups(pids: &[u32], signal: Signal) {
    for pid in pids {
        signal_process_group(*pid, signal);
    }
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: Signal) {
    let signal = match signal {
        Signal::Interrupt => libc::SIGINT,
        Signal::Kill => libc::SIGKILL,
    };
    let Ok(group) = libc::pid_t::try_from(pid) else {
        return;
    };
    // SAFETY: `killpg` takes a group id and a signal number and touches no
    // memory of ours; a group that no longer exists is reported as an error,
    // which is ignored here.
    unsafe {
        libc::killpg(group, signal);
    }
}

/// Windows has no process group to signal, so the command's process tree is
/// taken down by `taskkill` instead, forcibly in either case: there is no
/// interruption a console-less process would receive.
#[cfg(windows)]
fn kill_command(child: &mut Child, _isolation: Isolation) {
    signal_process_group(child.id(), Signal::Kill);
    let _ = child.kill();
}

/// The `taskkill`s are started together and waited for together, so that the
/// pids are all acted on in the same moment and a stop takes one bound in all
/// rather than one per pid.
#[cfg(windows)]
fn signal_process_groups(pids: &[u32], _signal: Signal) {
    let mut children: Vec<Child> = pids
        .iter()
        .filter_map(|pid| {
            Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()
        })
        .collect();
    let deadline = Instant::now() + REAP_TIMEOUT;
    while Instant::now() < deadline {
        children.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
        if children.is_empty() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    for mut child in children {
        let _ = child.kill();
    }
}

#[cfg(windows)]
fn signal_process_group(pid: u32, signal: Signal) {
    signal_process_groups(&[pid], signal);
}

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

/// Lines a pipe reader may have waiting for the runner. Past that the reader
/// waits, the pipe fills, and the command waits on its write, as it would on a
/// slow terminal; memory does not grow with the speed of the command.
const PIPE_QUEUE: usize = 1_024;

/// Starts a command in a process group of its own and puts it on the running
/// list, so that a stop reaches it. The whole of that, from before the spawn to
/// after the registration, counts as spawning, and a stop waits for spawning to
/// finish: an abort that comes in during it is answered here, by killing the
/// command as soon as it is registered, before the count drops.
fn spawn_tracked(mut command: Command) -> Result<Child> {
    SPAWNING.fetch_add(1, Ordering::SeqCst);
    let spawned = spawn_tracked_counted(&mut command);
    SPAWNING.fetch_sub(1, Ordering::SeqCst);
    spawned
}

fn spawn_tracked_counted(command: &mut Command) -> Result<Child> {
    if ABANDONED.load(Ordering::SeqCst) {
        return Err(anyhow!("aborted before the command started"));
    }
    own_process_group(command);
    let mut child = command.spawn().context("failed to run command")?;
    lock_ignoring_poison(&RUNNING_COMMANDS).push(child.id());
    if ABANDONED.load(Ordering::SeqCst) {
        kill_command(&mut child, Isolation::OwnGroup);
        reap(&mut child);
        forget_running(&child, Isolation::OwnGroup);
        return Err(anyhow!("aborted before the command started"));
    }
    Ok(child)
}

/// What a command left behind: its exit status and every line it wrote to
/// either stream, in the order the lines arrived.
#[derive(Debug)]
struct CommandOutput {
    status: ExitStatus,
    text: String,
}

/// How often a running command is looked at, both for its exit and for the
/// timeout.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Runs the command, handing each line it writes to `on_line` as it arrives,
/// and kills it once `timeout` elapses, taking the helpers it started when, and
/// only when, it has a process group of its own. An unreachable remote would
/// otherwise leave `git fetch` waiting for a connection that never completes,
/// and a list or a sync would wait with it. A killed command yields no output
/// beyond the lines already handed out, and a `git fetch` killed part way
/// through can leave whatever it had already written to `FETCH_HEAD`.
fn run_with_timeout(
    mut command: Command,
    timeout: Option<Duration>,
    isolation: Isolation,
    on_line: &mut dyn FnMut(String),
) -> Result<CommandOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A command sharing this process's group is not tracked, but an abort
    // still means that nothing new may start: a stop is under way, and the
    // signal the terminal sent to the group has already gone by.
    if ABANDONED.load(Ordering::SeqCst) {
        return Err(anyhow!("aborted before the command started"));
    }
    let mut child = match isolation {
        Isolation::OwnGroup => spawn_tracked(command)?,
        Isolation::SharedGroup => command.spawn().context("failed to run command")?,
    };
    let (tx, rx) = mpsc::sync_channel(PIPE_QUEUE);
    let readers = stream(
        child.stdout.take().expect("stdout is piped above"),
        tx.clone(),
    )
    .and_then(|()| stream(child.stderr.take().expect("stderr is piped above"), tx));
    if let Err(error) = readers {
        kill_command(&mut child, isolation);
        reap(&mut child);
        forget_running(&child, isolation);
        return Err(error);
    }

    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    let mut lines = VecDeque::new();
    let mut open_pipes = 2;
    let mut exited: Option<(ExitStatus, Instant)> = None;
    loop {
        // A command that closed both its pipes and ran on, one that started a
        // daemon or redirected its output, has nothing more to say: the channel
        // is disconnected and would return at once, so the wait is slept
        // instead of spun.
        if open_pipes == 0 {
            thread::sleep(POLL_INTERVAL);
        }
        let message = if open_pipes == 0 {
            Err(RecvTimeoutError::Disconnected)
        } else {
            rx.recv_timeout(POLL_INTERVAL)
        };
        match message {
            Ok(PipeMessage::Line(line)) => {
                on_line(line.clone());
                if lines.len() == RETAINED_LINES {
                    lines.pop_front();
                }
                lines.push_back(line);
            }
            Ok(PipeMessage::Eof) => open_pipes -= 1,
            // A reader thread that died takes its sender with it; there is
            // nothing more to wait for from that pipe.
            Err(RecvTimeoutError::Disconnected) => open_pipes = 0,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if exited.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    forget_running(&child, isolation);
                    exited = Some((status, Instant::now() + DRAIN_TIMEOUT));
                }
                Ok(None) if deadline.is_some_and(|deadline| Instant::now() >= deadline) => {
                    kill_command(&mut child, isolation);
                    reap(&mut child);
                    forget_running(&child, isolation);
                    let seconds = timeout.unwrap_or_default().as_secs();
                    return Err(anyhow!("command timed out after {seconds} seconds"));
                }
                Ok(None) => {}
                Err(error) => {
                    kill_command(&mut child, isolation);
                    reap(&mut child);
                    forget_running(&child, isolation);
                    return Err(error).context("failed to wait for command");
                }
            }
        }
        // A helper the command left behind can hold the pipes open after the
        // command itself is gone, so the drain has a bound of its own.
        let Some((status, drain_deadline)) = exited else {
            continue;
        };
        if open_pipes == 0 || Instant::now() >= drain_deadline {
            return Ok(CommandOutput {
                status,
                text: Vec::from(lines).join("\n"),
            });
        }
    }
}

/// Takes the command off the running list once it has ended. A process group
/// that still has members, a helper the command started or a background
/// process a build left behind, moves to the leftover list, so that a stop
/// still reaches them; it is let go when it has emptied. A pid cannot be
/// reused while a group with that id exists, so a kept entry names the right
/// processes.
#[cfg(unix)]
fn forget_running(child: &Child, isolation: Isolation) {
    if matches!(isolation, Isolation::SharedGroup) {
        return;
    }
    let pid = child.id();
    // Onto the leftover list before it leaves the running list, so that at no
    // moment is a live group on neither: a stop that looked between the two
    // would find nothing left to wait for and return with the group alive.
    // Leftover groups that have since emptied are let go at the same time,
    // which keeps the time an emptied group's id spends on the list short;
    // the id may be handed to an unrelated group once it is free.
    {
        let mut leftover = lock_ignoring_poison(&LEFTOVER_GROUPS);
        leftover.retain(|other| group_alive(*other));
        if group_alive(pid) {
            leftover.push(pid);
        }
    }
    let mut running = lock_ignoring_poison(&RUNNING_COMMANDS);
    if let Some(index) = running.iter().position(|other| *other == pid) {
        running.swap_remove(index);
    }
}

/// Whether a listed group is still worth signalling. On Windows there is no
/// group to ask, so an entry is trusted for as long as its command runs and
/// dropped the moment the command has ended; the group check on Unix below
/// would drop every entry there before anything was signalled.
#[cfg(unix)]
fn listed_group_alive(pid: u32) -> bool {
    group_alive(pid)
}

#[cfg(windows)]
fn listed_group_alive(_pid: u32) -> bool {
    true
}

/// Whether any process in the group still exists. Signal zero checks without
/// delivering anything; a group of another user's processes, which answers
/// with a permission error, is taken as alive.
#[cfg(unix)]
fn group_alive(pid: u32) -> bool {
    let Ok(group) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: as in `signal_process_group`; signal zero delivers nothing.
    let answer = unsafe { libc::killpg(group, 0) };
    answer == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Windows has no process group to ask about; see `listed_group_alive`.
#[cfg(windows)]
fn forget_running(child: &Child, isolation: Isolation) {
    if matches!(isolation, Isolation::SharedGroup) {
        return;
    }
    let pid = child.id();
    let mut running = lock_ignoring_poison(&RUNNING_COMMANDS);
    if let Some(index) = running.iter().position(|other| *other == pid) {
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

/// Syncs one repository. Every line goes to `log` without the repository's
/// name on it: the caller knows which repository it asked about, and puts the
/// name where its display wants it.
///
/// `isolation` says whether a terminal in cooked mode is watching, which is
/// the only case in which Ctrl-C reaches the pull and the build by itself.
pub fn sync_repo<F>(repo: &RepoConfig, isolation: Isolation, mut log: F) -> SyncResult
where
    F: FnMut(String),
{
    // The lock is held from the status check to the end of the build. The pull
    // must not run while a background inspection, or another worker of this
    // sync, is fetching into the same `.git`; and where two entries share a
    // repository, the pull of one must not change the tree while the other is
    // checking or building it. The pull carries a timeout so that a pull that
    // cannot finish does not hold the lock for the rest of the session; a
    // build holds it for as long as it runs.
    let lock = repo_lock(&repo.path);
    let _held = match lock.try_lock() {
        Ok(held) => held,
        Err(std::sync::TryLockError::WouldBlock) => {
            log("waiting for another git command in this repository".into());
            lock_ignoring_poison(&lock)
        }
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
    };
    // The wait may have outlasted an abort, whose signal reached only the
    // commands running at the time. Anything started from here on would be
    // killed as it starts, so the repository is given up before that.
    if ABANDONED.load(Ordering::SeqCst) {
        let message = "aborted before the sync started".to_owned();
        log(format!("ERROR {message}"));
        return result(repo, SyncOutcome::StatusFailed, None, message);
    }
    log("checking repository".into());
    let status = inspect(repo);
    if let Some(error) = status.error {
        let message = format!("status check failed: {error}");
        log(format!("ERROR {message}"));
        return result(repo, SyncOutcome::StatusFailed, None, message);
    }
    if !status.clean {
        let message = "skipped: working tree has uncommitted changes".to_owned();
        log(format!("WARN {message}"));
        return result(repo, SyncOutcome::SkippedDirty, status.branch, message);
    }
    if !status.on_main {
        let branch = status.branch.unwrap_or_else(|| "unknown".into());
        let message = format!("skipped: current branch is '{branch}', not main/master");
        log(format!("WARN {message}"));
        return result(repo, SyncOutcome::SkippedBranch, Some(branch), message);
    }

    let branch = status.branch;
    log("pulling from origin".into());
    let pull = match git_offline(
        &repo.path,
        &["pull", "origin", "HEAD"],
        Some(PULL_TIMEOUT),
        isolation,
        &mut |line| log(line),
    ) {
        Ok(output) => output,
        Err(error) => {
            let message = format!("git pull did not run to completion: {error:#}");
            log(format!("ERROR {message}"));
            return result(repo, SyncOutcome::PullFailed, branch, message);
        }
    };
    if !pull.status.success() {
        let message = format!("git pull failed ({})", exit_label(pull.status));
        log(format!("ERROR {message}"));
        let mut item = result(repo, SyncOutcome::PullFailed, branch, message);
        item.pull_output = Some(pull.text);
        return item;
    }

    if let Some(command) = repo
        .build_command
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        log(format!("building: {command}"));
        let build = match run_with_timeout(
            shell_command(command, &repo.path),
            None,
            isolation,
            &mut |line| log(line),
        ) {
            Ok(output) => output,
            Err(error) => {
                let message = format!("could not start build: {error:#}");
                log(format!("ERROR {message}"));
                let mut item = result(repo, SyncOutcome::BuildFailed, branch, message);
                item.pull_output = Some(pull.text);
                return item;
            }
        };
        if !build.status.success() {
            let message = format!("build failed ({})", exit_label(build.status));
            log(format!("ERROR {message}"));
            let mut item = result(repo, SyncOutcome::BuildFailed, branch, message);
            item.pull_output = Some(pull.text);
            item.build_output = Some(build.text);
            return item;
        }
        let message = "sync completed".to_owned();
        log(format!("OK {message}"));
        let mut item = result(repo, SyncOutcome::Succeeded, branch, message);
        item.pull_output = Some(pull.text);
        item.build_output = Some(build.text);
        return item;
    }

    let message = "sync completed".to_owned();
    log(format!("OK {message}"));
    let mut item = result(repo, SyncOutcome::Succeeded, branch, message);
    item.pull_output = Some(pull.text);
    item
}

/// Syncs the repositories `concurrency` at a time and calls `on_event` on the
/// calling thread as the workers report, so a display needs no locking of its
/// own. The results come back in the order of `repos`. A sync abandoned with
/// `terminate_running_commands` returns only the repositories that were
/// started; the rest were never touched.
pub fn sync_repos<F>(
    repos: &[RepoConfig],
    concurrency: usize,
    isolation: Isolation,
    mut on_event: F,
) -> Vec<SyncResult>
where
    F: FnMut(SyncEvent),
{
    let workers = repos.len().min(concurrency.max(1));
    on_event(SyncEvent::Planned { workers });
    let (job_tx, job_rx) = mpsc::channel::<(usize, &RepoConfig)>();
    // Bounded, so that workers wait for a slow display rather than pile lines
    // up in memory; the display is drained on this thread below, for as long
    // as any worker runs.
    let (event_tx, event_rx) = mpsc::sync_channel(EVENT_QUEUE);
    for job in repos.iter().enumerate() {
        let _ = job_tx.send(job);
    }
    drop(job_tx);
    let job_rx = Mutex::new(job_rx);

    let mut results: Vec<Option<SyncResult>> = vec![None; repos.len()];
    thread::scope(|scope| {
        for worker in 0..workers {
            let event_tx = event_tx.clone();
            let job_rx = &job_rx;
            scope.spawn(move || loop {
                if ABANDONED.load(Ordering::SeqCst) {
                    return;
                }
                let Ok((index, repo)) = lock_ignoring_poison(job_rx).recv() else {
                    return;
                };
                let name = repo.name.clone();
                if event_tx
                    .send((index, SyncEvent::Started { worker, name }))
                    .is_err()
                {
                    return;
                }
                let result = sync_repo(repo, isolation, |line| {
                    let name = repo.name.clone();
                    let _ = event_tx.send((index, SyncEvent::Line { worker, name, line }));
                });
                if event_tx
                    .send((index, SyncEvent::Finished { worker, result }))
                    .is_err()
                {
                    return;
                }
            });
        }
        drop(event_tx);
        for (index, event) in event_rx {
            if let SyncEvent::Finished { result, .. } = &event {
                results[index] = Some(result.clone());
            }
            on_event(event);
        }
    });
    results.into_iter().flatten().collect()
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
    let mut command = Command::new("git");
    command.args(args).current_dir(path);
    output_within(command, READ_TIMEOUT)
        .with_context(|| format!("failed to run git in {}", path.display()))
}

/// Runs a command that only reads and collects its output, killing it once
/// `timeout` elapses. It gets a group of its own and goes on the running list
/// like every other command, so that a stop reaches it too, hook and all. The
/// bound is there so that nothing waits on it without end, a lock least of all.
///
/// Most of these commands end within milliseconds, so the wait for them starts
/// short and lengthens, rather than costing a whole poll interval each.
fn output_within(mut command: Command, timeout: Duration) -> Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_tracked(command)?;
    let stdout = collect(child.stdout.take().expect("stdout is piped above"));
    let stderr = collect(child.stderr.take().expect("stderr is piped above"));
    let deadline = Instant::now() + timeout;
    let mut pause = Duration::from_millis(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(pause);
                pause = (pause * 2).min(POLL_INTERVAL);
            }
            Ok(None) => {
                kill_command(&mut child, Isolation::OwnGroup);
                reap(&mut child);
                forget_running(&child, Isolation::OwnGroup);
                return Err(anyhow!("timed out after {} seconds", timeout.as_secs()));
            }
            Err(error) => {
                kill_command(&mut child, Isolation::OwnGroup);
                reap(&mut child);
                forget_running(&child, Isolation::OwnGroup);
                return Err(error.into());
            }
        }
    };
    forget_running(&child, Isolation::OwnGroup);
    Ok(Output {
        status,
        stdout: stdout.recv_timeout(DRAIN_TIMEOUT).unwrap_or_default(),
        stderr: stderr.recv_timeout(DRAIN_TIMEOUT).unwrap_or_default(),
    })
}

/// The most output of a git command that only reads that is kept. Past it the
/// rest is read and dropped, so that the command is not left blocked on a full
/// pipe.
const MAX_READ_OUTPUT: usize = 8 * 1024 * 1024;

/// Reads a pipe to the end on its own thread, for the same reason `stream`
/// does. A thread that cannot be started leaves an empty result rather than a
/// panic, because the child has already been spawned.
fn collect(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    let _ = thread::Builder::new().spawn(move || {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(read) = pipe.read(&mut chunk) {
            if read == 0 {
                break;
            }
            let room = MAX_READ_OUTPUT.saturating_sub(buffer.len());
            buffer.extend_from_slice(&chunk[..read.min(room)]);
        }
        let _ = tx.send(buffer);
    });
    rx
}

#[cfg(unix)]
fn shell_command(command: &str, path: &Path) -> Command {
    let mut shell = Command::new("sh");
    shell.args(["-lc", command]).current_dir(path);
    shell
}

#[cfg(windows)]
fn shell_command(command: &str, path: &Path) -> Command {
    let mut shell = Command::new("cmd");
    shell.args(["/C", command]).current_dir(path);
    shell
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

fn exit_label(status: ExitStatus) -> String {
    status.code().map_or_else(
        || "terminated by signal".into(),
        |code| format!("exit {code}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// `ABANDONED` and `RUNNING_COMMANDS` are shared by the whole process, and
    /// every git command reads the one and joins the other. A test that sets
    /// the flag or stops the running commands takes this exclusively; a test
    /// that merely runs commands takes it shared, so those still run together.
    static ABANDON_FLAG: std::sync::RwLock<()> = std::sync::RwLock::new(());

    fn shared() -> std::sync::RwLockReadGuard<'static, ()> {
        ABANDON_FLAG
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn exclusive() -> std::sync::RwLockWriteGuard<'static, ()> {
        let guard = ABANDON_FLAG
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ABANDONED.store(false, Ordering::SeqCst);
        guard
    }

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
        let _serial = shared();
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
        let _serial = shared();
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
        let _serial = shared();
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
        let _serial = shared();
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
        let _serial = shared();
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
        // Arrange: the command is on the running list while it runs, so a stop
        // issued by another test would end it early.
        let _serial = shared();
        let mut command = Command::new("sleep");
        command.arg("30");
        let started = Instant::now();

        // Act
        let error = run_with_timeout(
            command,
            Some(Duration::from_millis(300)),
            Isolation::OwnGroup,
            &mut |_| {},
        )
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

        let (mut stdout, mut stderr) = (0, 0);

        // Act
        let output = run_with_timeout(
            command,
            Some(Duration::from_secs(20)),
            Isolation::SharedGroup,
            &mut |line| match line.as_str() {
                "stdout" | "stdou" => stdout += 1,
                _ => stderr += 1,
            },
        )
        .unwrap();

        // Assert: `yes` writes one word per line and `head` cuts the last one
        // short, so every line is handed out, newlines taken off, while the
        // result keeps only the tail.
        assert!(output.status.success());
        assert_eq!(stdout, 400_000_usize.div_ceil("stdout\n".len()));
        assert_eq!(stderr, 400_000_usize.div_ceil("stderr\n".len()));
        assert_eq!(output.text.lines().count(), RETAINED_LINES);
    }

    #[cfg(unix)]
    #[test]
    fn hands_out_each_line_as_it_arrives_and_keeps_the_last_progress_frame() {
        // Arrange: a line, a progress bar redrawn with carriage returns, a line
        // on the other stream, and a last line without a newline.
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "echo first; printf '10%%\\r50%%\\r100%%\\n'; echo warning >&2; sleep 0.2; printf last",
        ]);
        let mut seen = Vec::new();

        // Act
        let output = run_with_timeout(
            command,
            Some(Duration::from_secs(20)),
            Isolation::SharedGroup,
            &mut |line| seen.push(line),
        )
        .unwrap();

        // Assert: the two streams are read by two threads, so only the order
        // within one stream is promised.
        assert!(output.status.success());
        let position = |wanted: &str| seen.iter().position(|line| line == wanted);
        assert!(position("first") < position("100%"), "{seen:?}");
        assert!(position("100%") < position("last"), "{seen:?}");
        assert!(!seen.iter().any(|line| line.contains('\r')), "{seen:?}");
        assert!(seen.contains(&"warning".to_owned()), "{seen:?}");
        assert_eq!(seen.last().map(String::as_str), Some("last"));
        assert_eq!(output.text, seen.join("\n"));
    }

    #[test]
    fn runs_a_build_without_a_timeout() {
        // Arrange
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 0.3; echo built"]);

        // Act
        let output = run_with_timeout(command, None, Isolation::SharedGroup, &mut |_| {}).unwrap();

        // Assert
        assert!(output.status.success());
        assert_eq!(output.text, "built");
    }

    #[cfg(unix)]
    #[test]
    fn kills_a_command_that_starts_after_an_abort() {
        // Arrange: the abort came in before this command could be registered,
        // so its signal found nothing of it to hit.
        let _serial = exclusive();
        ABANDONED.store(true, Ordering::SeqCst);
        let mut command = Command::new("sleep");
        command.arg("30");
        let started = Instant::now();

        // Act
        let error = run_with_timeout(command, None, Isolation::OwnGroup, &mut |_| {})
            .expect_err("the command is not allowed to run on");
        ABANDONED.store(false, Ordering::SeqCst);

        // Assert
        assert!(error.to_string().contains("aborted"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(lock_ignoring_poison(&RUNNING_COMMANDS).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_stop_waits_for_the_commands_it_asked_to_stop_and_kills_the_rest() {
        // Arrange: one command that stops when asked, one that ignores it.
        let _serial = exclusive();
        ABANDONED.store(false, Ordering::SeqCst);
        let mut polite = Command::new("sleep");
        polite.arg("30");
        let mut stubborn = Command::new("sh");
        stubborn.args(["-c", "trap '' INT; sleep 30"]);
        let outcomes = Mutex::new(Vec::new());
        let started = Instant::now();

        // Act
        thread::scope(|scope| {
            for command in [polite, stubborn] {
                let outcomes = &outcomes;
                scope.spawn(move || {
                    let output = run_with_timeout(command, None, Isolation::OwnGroup, &mut |_| {});
                    lock_ignoring_poison(outcomes)
                        .push(output.map(|output| output.status.success()));
                });
            }
            thread::sleep(Duration::from_millis(500));
            stop_running_commands(Duration::from_secs(2));
        });
        // The stop leaves the flag set, as it would on the way out of the
        // process; the next test starts from a clean one.
        ABANDONED.store(false, Ordering::SeqCst);

        // Assert: both ended, neither with success, and the stop did not wait
        // out the 30 seconds.
        let outcomes = lock_ignoring_poison(&outcomes);
        assert_eq!(outcomes.len(), 2);
        assert!(
            outcomes.iter().all(|outcome| matches!(outcome, Ok(false))),
            "{outcomes:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(lock_ignoring_poison(&RUNNING_COMMANDS).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn keeps_a_group_listed_while_a_background_process_of_the_build_lives() {
        // Arrange: the build itself ends at once and leaves a process behind.
        let _serial = exclusive();
        ABANDONED.store(false, Ordering::SeqCst);
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 >/dev/null 2>&1 &"]);

        // Act
        let output = run_with_timeout(command, None, Isolation::OwnGroup, &mut |_| {}).unwrap();
        let running = lock_ignoring_poison(&RUNNING_COMMANDS).clone();
        let leftover = lock_ignoring_poison(&LEFTOVER_GROUPS).clone();
        stop_leftover_commands(Duration::from_secs(2));
        thread::sleep(Duration::from_millis(200));

        // Assert: the group outlived the build, moved to the leftovers, and the
        // stop of those took it down without abandoning anything.
        assert!(output.status.success());
        assert!(running.is_empty(), "{running:?}");
        assert_eq!(leftover.len(), 1, "{leftover:?}");
        assert!(!group_alive(leftover[0]));
        assert!(lock_ignoring_poison(&LEFTOVER_GROUPS).is_empty());
        assert!(!ABANDONED.load(Ordering::SeqCst));
    }

    #[test]
    fn splits_a_line_that_never_ends_instead_of_holding_it_whole() {
        // Arrange: three times the bound, with no newline anywhere.
        let data = vec![b'x'; MAX_LINE_BYTES * 3];
        let mut reader = BufReader::new(data.as_slice());
        let mut pieces = Vec::new();

        // Act
        loop {
            let mut buffer = Vec::new();
            let read = read_line_bounded(&mut reader, &mut buffer).unwrap();
            if read == 0 {
                break;
            }
            pieces.push(buffer.len());
        }

        // Assert
        assert_eq!(pieces, vec![MAX_LINE_BYTES; 3]);

        // And an ordinary line comes back whole, newline included.
        let mut reader = BufReader::new(&b"one\ntwo"[..]);
        let mut buffer = Vec::new();
        assert_eq!(read_line_bounded(&mut reader, &mut buffer).unwrap(), 4);
        assert_eq!(buffer, b"one\n");
        buffer.clear();
        assert_eq!(read_line_bounded(&mut reader, &mut buffer).unwrap(), 3);
        assert_eq!(buffer, b"two");
    }

    #[test]
    fn keeps_only_the_first_part_of_an_oversized_read_only_output() {
        // Arrange: more than the bound, on one pipe.
        let _serial = shared();
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!("yes | head -c {}", MAX_READ_OUTPUT + 100_000),
        ]);

        // Act
        let output = output_within(command, Duration::from_secs(20)).unwrap();

        // Assert: the command ran to its end rather than blocking on the pipe,
        // and the result stops at the bound.
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), MAX_READ_OUTPUT);
    }

    #[test]
    fn bounds_a_git_command_that_only_reads() {
        // Arrange: the command is tracked while it runs, like every other.
        let _serial = shared();
        let mut command = Command::new("sleep");
        command.arg("30");
        let started = Instant::now();

        // Act
        let error = output_within(command, Duration::from_millis(300))
            .expect_err("the command outlives the bound");

        // Assert
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn rejects_a_concurrency_outside_the_supported_range() {
        // Arrange & Act & Assert
        assert!(validate_concurrency(0).is_err());
        assert!(validate_concurrency(1).is_ok());
        assert!(validate_concurrency(MAX_CONCURRENCY).is_ok());
        assert!(validate_concurrency(MAX_CONCURRENCY + 1).is_err());
    }

    #[test]
    fn config_defaults_the_concurrency_and_reads_it_when_given() {
        // Arrange & Act
        let defaulted: Config = serde_yaml::from_str("repos: []\n").unwrap();
        let given: Config = serde_yaml::from_str("concurrency: 2\nrepos: []\n").unwrap();
        let too_many: Config = serde_yaml::from_str("concurrency: 11\nrepos: []\n").unwrap();

        // Assert
        assert_eq!(defaulted.concurrency, DEFAULT_CONCURRENCY);
        assert_eq!(given.concurrency, 2);
        assert!(validate_config(&too_many)
            .unwrap_err()
            .to_string()
            .contains("concurrency must be between 1 and 10, not 11"));
    }

    #[test]
    fn parallel_sync_reports_every_repository_once_on_a_bounded_set_of_workers() {
        // Arrange: directories that do not exist fail the status check at once,
        // so this exercises the pool and the events, not git.
        let _serial = shared();
        let repos: Vec<_> = (0..7)
            .map(|index| RepoConfig {
                name: format!("absent-{index}"),
                path: env::temp_dir().join(format!("pullkit-absent-{index}")),
                build_command: None,
            })
            .collect();
        let mut planned = None;
        let mut started = vec![0; repos.len()];
        let mut finished = vec![0; repos.len()];
        let mut lines = 0;

        // Act
        let results = sync_repos(&repos, 3, Isolation::SharedGroup, |event| match event {
            SyncEvent::Planned { workers } => planned = Some(workers),
            SyncEvent::Started { worker, name } => {
                assert!(worker < 3);
                let index = repos.iter().position(|repo| repo.name == name).unwrap();
                started[index] += 1;
            }
            SyncEvent::Line { worker, line, .. } => {
                assert!(worker < 3);
                assert!(!line.starts_with('['), "lines carry no name: {line}");
                lines += 1;
            }
            SyncEvent::Finished { worker, result } => {
                assert!(worker < 3);
                let index = repos
                    .iter()
                    .position(|repo| repo.name == result.name)
                    .unwrap();
                finished[index] += 1;
            }
        });

        // Assert
        assert_eq!(planned, Some(3));
        assert!(started.iter().all(|count| *count == 1));
        assert!(finished.iter().all(|count| *count == 1));
        assert!(lines >= repos.len());
        let names: Vec<_> = results.iter().map(|result| result.name.as_str()).collect();
        let expected: Vec<_> = repos.iter().map(|repo| repo.name.as_str()).collect();
        assert_eq!(names, expected, "results keep the configured order");
        assert!(results
            .iter()
            .all(|result| matches!(result.outcome, SyncOutcome::StatusFailed)));
    }

    #[test]
    fn sync_uses_no_more_workers_than_repositories() {
        // Arrange
        let _serial = shared();
        let repos = vec![RepoConfig {
            name: "only".into(),
            path: env::temp_dir().join("pullkit-absent-only"),
            build_command: None,
        }];
        let mut planned = None;

        // Act
        sync_repos(&repos, 10, Isolation::SharedGroup, |event| {
            if let SyncEvent::Planned { workers } = event {
                planned = Some(workers);
            }
        });

        // Assert
        assert_eq!(planned, Some(1));
    }

    #[test]
    fn parallel_inspection_reports_every_repository_once() {
        // Arrange
        let _serial = shared();
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
        let _serial = shared();
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
