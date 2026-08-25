use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
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
    }
    Ok(())
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
    let pull = match git(&repo.path, &["pull", "origin", "HEAD"]) {
        Ok(output) => output,
        Err(error) => {
            let message = format!("could not start git pull: {error:#}");
            log(format!("[{}] ERROR {message}", repo.name));
            return result(repo, SyncOutcome::PullFailed, branch, message);
        }
    };
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
