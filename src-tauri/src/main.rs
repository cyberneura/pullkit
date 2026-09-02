use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use pullkit_core::{
    config_path, initialize_config, inspect_commits_parallel, load_config, sync_repos, RepoCommits,
    RepoConfig, RepoStatus, SyncOutcome, SyncResult, EXAMPLE_CONFIG,
};
use serde::Serialize;
use std::{
    collections::HashSet,
    io::{self, IsTerminal, Write},
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};
use tauri::Emitter;

#[derive(Parser)]
#[command(
    name = "pullkit",
    version,
    about = "Inspect and sync configured Git repositories"
)]
struct Args {
    /// Open the graphical interface.
    #[arg(long, global = true)]
    gui: bool,
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Sync all eligible repositories, or a named subset.
    Sync {
        /// Comma-separated repository names.
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
    },
}

fn main() {
    let args = Args::parse();
    let result = run(args);
    if let Err(error) = result {
        eprintln!("pullkit: {error:#}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<()> {
    let initialized_path = initialize_config()?;
    if let Some(path) = &initialized_path {
        print_config_help(
            "No configuration file was found.",
            &format!(
                "A sample configuration file was created at {}.",
                path.display()
            ),
        );
        if !args.gui {
            return Ok(());
        }
    }

    if args.gui {
        if initialized_path.is_none()
            && matches!(load_config(), Ok(config) if config.repos.is_empty())
        {
            print_empty_config_help()?;
        }
        return run_gui();
    }

    if load_config()?.repos.is_empty() {
        print_empty_config_help()?;
        return Ok(());
    }

    run_cli(args.command)
}

fn print_empty_config_help() -> Result<()> {
    let path = config_path()?;
    print_config_help(
        "No repositories are configured.",
        &format!("Add repository entries to {}.", path.display()),
    );
    Ok(())
}

fn print_config_help(message: &str, instruction: &str) {
    println!("{message}");
    println!("{instruction}");
    println!();
    print!("{EXAMPLE_CONFIG}");
    println!();
    println!("Edit the repository names, paths, and build commands, then run pullkit again.");
}

fn run_cli(command: Option<CliCommand>) -> Result<()> {
    let config = load_config()?;
    match command {
        None => {
            if io::stdin().is_terminal() && io::stdout().is_terminal() {
                if let Some(repos) = run_tui(&config.repos)? {
                    run_sync(repos)?;
                }
            } else {
                print_repo_list(&config.repos);
            }
            Ok(())
        }
        Some(CliCommand::Sync { only }) => {
            let repos = select_repos(&config.repos, &only)?;
            run_sync(repos)
        }
    }
}

fn print_repo_list(repos: &[RepoConfig]) {
    let statuses: Vec<_> = repos.iter().map(pullkit_core::inspect).collect();
    let mut commits: Vec<Option<RepoCommits>> = vec![None; repos.len()];
    inspect_commits_parallel(repos, |index, item| commits[index] = Some(item));
    println!(
        "{:<20} {:<8} {:<12} {:<8} {:<DATE_WIDTH$} {:<DATE_WIDTH$} {:<DIFFERENCE_WIDTH$} PATH",
        "REPOSITORY", "TREE", "BRANCH", "ON MAIN", "LOCAL COMMIT", "REMOTE COMMIT", "DIFFERENCE"
    );
    for (status, commits) in statuses.iter().zip(&commits) {
        print_status(status, commits.as_ref());
    }
}

/// Runs the commit inspection on a background thread so a list can be shown
/// before the remote fetches finish; results arrive on the returned channel.
fn spawn_commit_inspection(repos: &[RepoConfig]) -> Receiver<(usize, RepoCommits)> {
    let (tx, rx) = mpsc::channel();
    let repos = repos.to_vec();
    thread::spawn(move || {
        inspect_commits_parallel(&repos, |index, commits| {
            let _ = tx.send((index, commits));
        });
    });
    rx
}

/// Wide enough for the longest label, `diverged by 12 months`.
const DIFFERENCE_WIDTH: usize = 21;
const DATE_WIDTH: usize = 16;

struct CommitCells {
    local: String,
    remote: String,
    difference: String,
}

fn commit_cells(commits: Option<&RepoCommits>) -> CommitCells {
    let Some(commits) = commits else {
        return CommitCells {
            local: "-".into(),
            remote: "-".into(),
            difference: "fetching...".into(),
        };
    };
    let date = |commit: &Option<pullkit_core::CommitInfo>| {
        commit
            .as_ref()
            .map_or("-".into(), |commit| commit.date.clone())
    };
    CommitCells {
        local: date(&commits.local),
        remote: date(&commits.remote),
        difference: commits.difference.clone().unwrap_or_else(|| {
            if commits.error.is_some() {
                "unavailable".into()
            } else {
                "-".into()
            }
        }),
    }
}

fn run_sync(repos: Vec<RepoConfig>) -> Result<()> {
    println!(
        "pullkit run: {} repositor{}",
        repos.len(),
        if repos.len() == 1 { "y" } else { "ies" }
    );
    println!();
    let results = sync_repos(&repos, |line| println!("{line}"));
    print_summary(&results);
    if results.iter().any(|result| {
        matches!(
            result.outcome,
            SyncOutcome::PullFailed | SyncOutcome::BuildFailed | SyncOutcome::StatusFailed
        )
    }) {
        std::process::exit(1);
    }
    Ok(())
}

struct TuiState {
    cursor: usize,
    selected: Vec<bool>,
}

impl TuiState {
    fn new(repo_count: usize) -> Self {
        Self {
            cursor: 0,
            selected: vec![false; repo_count],
        }
    }

    fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_down(&mut self) {
        if self.cursor + 1 < self.selected.len() {
            self.cursor += 1;
        }
    }

    fn toggle_current(&mut self, statuses: &[RepoStatus]) {
        if statuses[self.cursor].path_exists {
            self.selected[self.cursor] = !self.selected[self.cursor];
        }
    }

    fn toggle_all(&mut self, statuses: &[RepoStatus]) {
        let all_selected = statuses
            .iter()
            .enumerate()
            .filter(|(_, status)| status.path_exists)
            .all(|(index, _)| self.selected[index]);
        for (index, status) in statuses.iter().enumerate() {
            if status.path_exists {
                self.selected[index] = !all_selected;
            }
        }
    }

    fn selected_count(&self) -> usize {
        self.selected.iter().filter(|selected| **selected).count()
    }
}

struct TerminalSession {
    stdout: io::Stdout,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self { stdout })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(self.stdout, Show, LeaveAlternateScreen, ResetColor);
    }
}

fn run_tui(repos: &[RepoConfig]) -> Result<Option<Vec<RepoConfig>>> {
    let statuses: Vec<_> = repos.iter().map(pullkit_core::inspect).collect();
    let mut commits: Vec<Option<RepoCommits>> = vec![None; repos.len()];
    let commit_rx = spawn_commit_inspection(repos);
    let mut state = TuiState::new(repos.len());
    let mut terminal = TerminalSession::enter()?;
    let mut needs_redraw = true;

    loop {
        if needs_redraw {
            let (width, height) = terminal::size()?;
            draw_tui(
                &mut terminal.stdout,
                &statuses,
                &commits,
                &state,
                usize::from(width),
                height,
            )?;
            needs_redraw = false;
        }
        while let Ok((index, item)) = commit_rx.try_recv() {
            commits[index] = Some(item);
            needs_redraw = true;
        }
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(key) => key,
            Event::Resize(..) => {
                needs_redraw = true;
                continue;
            }
            _ => continue,
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }

        needs_redraw = true;
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(leave_tui());
            }
            KeyCode::Up | KeyCode::Char('k') => state.move_up(),
            KeyCode::Down | KeyCode::Char('j') => state.move_down(),
            KeyCode::Char(' ') => state.toggle_current(&statuses),
            KeyCode::Char('a') => state.toggle_all(&statuses),
            KeyCode::Enter if state.selected_count() > 0 => break,
            KeyCode::Esc | KeyCode::Char('q') => return Ok(leave_tui()),
            _ => {}
        }
    }

    drop(terminal);
    wait_for_commits(&commits, &commit_rx);
    Ok(Some(
        repos
            .iter()
            .enumerate()
            .filter(|(index, _)| state.selected[*index])
            .map(|(_, repo)| repo.clone())
            .collect(),
    ))
}

/// Quitting would otherwise leave the fetches of the list running: each one has
/// a process group of its own, so nothing reaches it once this process is gone.
fn leave_tui() -> Option<Vec<RepoConfig>> {
    pullkit_core::terminate_running_fetches();
    None
}

/// The core takes a per-repository lock, so a sync would block rather than race
/// with a background fetch. Waiting here instead keeps that from happening
/// silently, with the terminal already restored and no sign of why nothing is
/// happening. Every row is waited for, not only the selected ones: a row whose
/// directory sits inside another row's work tree shares that repository.
fn wait_for_commits(commits: &[Option<RepoCommits>], commit_rx: &Receiver<(usize, RepoCommits)>) {
    let mut pending = commits.iter().filter(|item| item.is_none()).count();
    if pending == 0 {
        return;
    }
    println!("Waiting for the remote inspection to finish...");
    while pending > 0 && commit_rx.recv().is_ok() {
        pending -= 1;
    }
}

fn draw_tui(
    stdout: &mut impl Write,
    statuses: &[RepoStatus],
    commits: &[Option<RepoCommits>],
    state: &TuiState,
    width: usize,
    height: u16,
) -> Result<()> {
    let visible_rows = usize::from(height.saturating_sub(4).max(1));
    let first_row = state.cursor.saturating_sub(visible_rows - 1);

    let header = tui_row(
        &format!("    {:<20} {:<14} ", "REPOSITORY", "STATUS"),
        "PATH",
        &CommitCells {
            local: "LOCAL COMMIT".into(),
            remote: "REMOTE COMMIT".into(),
            difference: "DIFFERENCE".into(),
        },
        width,
    );
    queue!(
        stdout,
        Clear(ClearType::All),
        MoveTo(0, 0),
        SetForegroundColor(Color::Cyan),
        SetAttribute(Attribute::Bold),
        Print("pullkit repositories"),
        SetAttribute(Attribute::Reset),
        ResetColor,
        MoveTo(0, 1),
        SetForegroundColor(Color::DarkGrey),
        Print(header),
        ResetColor
    )?;

    for (screen_row, (index, status)) in statuses
        .iter()
        .enumerate()
        .skip(first_row)
        .take(visible_rows)
        .enumerate()
    {
        queue!(
            stdout,
            MoveTo(0, u16::try_from(screen_row + 2).unwrap_or(u16::MAX)),
            Clear(ClearType::CurrentLine)
        )?;
        if index == state.cursor {
            queue!(stdout, SetAttribute(Attribute::Reverse))?;
        }
        if state.selected[index] {
            queue!(stdout, SetAttribute(Attribute::Bold))?;
        }
        queue!(stdout, SetForegroundColor(tui_status_color(status)))?;

        let checkbox = if state.selected[index] { "[x]" } else { "[ ]" };
        let line = tui_row(
            &format!("{checkbox} {:<20} {:<14} ", status.name, tui_status(status)),
            &status.path.display().to_string(),
            &commit_cells(commits[index].as_ref()),
            width,
        );
        queue!(
            stdout,
            Print(line),
            SetAttribute(Attribute::Reset),
            ResetColor
        )?;
    }

    let footer = format!(
        "Space select  a all  Enter sync  q quit    {} selected",
        state.selected_count()
    );
    queue!(
        stdout,
        MoveTo(0, height.saturating_sub(1)),
        Clear(ClearType::CurrentLine),
        SetForegroundColor(Color::DarkCyan),
        Print(truncate_line(&footer, width)),
        ResetColor
    )?;
    stdout.flush()?;
    Ok(())
}

/// Lays out `left`, a path that absorbs the remaining width, and the commit
/// columns against the right edge.
///
/// A narrow terminal cannot hold all of it, so things are given up in order of
/// how little they are missed: the path first, then the remote date, then the
/// local date. The difference is never given up, because it is the answer the
/// list exists to give; cutting it off would leave `up t` where `up to date`
/// belongs.
fn tui_row(left: &str, path: &str, cells: &CommitCells, width: usize) -> String {
    const MIN_PATH_WIDTH: usize = 8;
    let difference = format!("{:<DIFFERENCE_WIDTH$}", cells.difference);
    let candidates = [
        format!(
            "{:<DATE_WIDTH$}  {:<DATE_WIDTH$}  {difference}",
            cells.local, cells.remote
        ),
        format!("{:<DATE_WIDTH$}  {difference}", cells.local),
        difference.clone(),
    ];

    for columns in &candidates {
        let fixed = left.chars().count() + columns.chars().count() + 2;
        if width >= fixed + MIN_PATH_WIDTH {
            let path_width = width - fixed;
            let path = truncate_line(path, path_width);
            return format!("{left}{path:<path_width$}  {columns}");
        }
        if width >= left.chars().count() + columns.chars().count() {
            return format!("{left}{columns}");
        }
    }
    // Nothing on the right is left to give up, so the left side yields instead.
    // Truncating the row would cut the difference off the end, which is the one
    // thing this layout exists to keep. The room set aside is the same on every
    // row rather than the length of this row's own text, so the column stays put
    // down the list instead of shifting with each label.
    let Some(left_width) = width.checked_sub(DIFFERENCE_WIDTH + 1) else {
        return truncate_line(&cells.difference, width);
    };
    let left = truncate_line(left, left_width);
    format!("{left:<left_width$} {}", cells.difference)
}

fn tui_status(status: &RepoStatus) -> String {
    if !status.path_exists {
        return "Missing".into();
    }
    if status.error.is_some() {
        return "Error".into();
    }
    if !status.clean {
        return "Dirty".into();
    }
    if !status.on_main {
        return status
            .branch
            .clone()
            .unwrap_or_else(|| "Other branch".into());
    }
    "Ready".into()
}

fn tui_status_color(status: &RepoStatus) -> Color {
    if !status.path_exists {
        return Color::DarkGrey;
    }
    if status.error.is_some() {
        return Color::Red;
    }
    if !status.clean {
        return Color::Yellow;
    }
    if !status.on_main {
        return Color::Magenta;
    }
    Color::Green
}

fn truncate_line(line: &str, width: usize) -> String {
    line.chars().take(width).collect()
}

fn select_repos(repos: &[RepoConfig], only: &[String]) -> Result<Vec<RepoConfig>> {
    if only.is_empty() {
        return Ok(repos.to_vec());
    }
    let wanted: HashSet<&str> = only.iter().map(String::as_str).collect();
    let known: HashSet<&str> = repos.iter().map(|repo| repo.name.as_str()).collect();
    let mut missing: Vec<_> = wanted.difference(&known).copied().collect();
    missing.sort_unstable();
    if !missing.is_empty() {
        bail!("unknown repo name(s): {}", missing.join(", "));
    }
    Ok(repos
        .iter()
        .filter(|repo| wanted.contains(repo.name.as_str()))
        .cloned()
        .collect())
}

fn print_status(status: &RepoStatus, commits: Option<&RepoCommits>) {
    let cells = commit_cells(commits);
    if let Some(error) = &status.error {
        println!(
            "{:<20} {:<8} {:<12} {:<8} {:<DATE_WIDTH$} {:<DATE_WIDTH$} {:<DIFFERENCE_WIDTH$} {} ({error})",
            status.name,
            "error",
            "-",
            "-",
            cells.local,
            cells.remote,
            cells.difference,
            status.path.display()
        );
    } else {
        println!(
            "{:<20} {:<8} {:<12} {:<8} {:<DATE_WIDTH$} {:<DATE_WIDTH$} {:<DIFFERENCE_WIDTH$} {}",
            status.name,
            if status.clean { "clean" } else { "dirty" },
            status.branch.as_deref().unwrap_or("-"),
            if status.on_main { "yes" } else { "no" },
            cells.local,
            cells.remote,
            cells.difference,
            status.path.display()
        );
    }
}

fn print_summary(results: &[SyncResult]) {
    println!("\nSummary");
    for result in results {
        println!(
            "  {:<20} {:<16} {}",
            result.name,
            format!("{:?}", result.outcome).to_lowercase(),
            result.message
        );
    }
}

#[tauri::command]
fn list_repos() -> Result<Vec<RepoStatus>, String> {
    let config = load_config().map_err(|error| format!("{error:#}"))?;
    Ok(config.repos.iter().map(pullkit_core::inspect).collect())
}

/// The page seeds this from `Date.now()`, so it has to hold milliseconds since
/// 1970, not just a counter.
type InspectionToken = u64;

#[derive(Clone, Serialize)]
struct CommitEvent {
    token: InspectionToken,
    commits: RepoCommits,
}

/// Inspects every repository on the core worker pool and emits one event per
/// result, so the window fills its rows in as the fetches finish. `token`
/// comes back on every event so the page can discard a superseded run.
#[tauri::command]
async fn inspect_all_commits(app: tauri::AppHandle, token: InspectionToken) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let config = load_config().map_err(|error| format!("{error:#}"))?;
        inspect_commits_parallel(&config.repos, |_, commits| {
            let _ = app.emit("commit-inspected", CommitEvent { token, commits });
        });
        Ok(())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn sync_selected(
    app: tauri::AppHandle,
    names: Vec<String>,
) -> Result<Vec<SyncResult>, String> {
    if names.is_empty() {
        // `select_repos` reads an empty list as "every repository", which is the
        // CLI's contract for `--only`, never what an empty selection means here.
        return Err("no repositories were selected".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let config = load_config().map_err(|error| format!("{error:#}"))?;
        let repos = select_repos(&config.repos, &names).map_err(|error| format!("{error:#}"))?;
        let results = sync_repos(&repos, |line| {
            let _ = app.emit("sync-log", line);
        });
        Ok(results)
    })
    .await
    .map_err(|error| error.to_string())?
}

fn run_gui() -> Result<()> {
    tauri::Builder::default()
        .on_window_event(|_window, event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                pullkit_core::terminate_running_fetches();
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_repos,
            inspect_all_commits,
            sync_selected
        ])
        .run(tauri::generate_context!())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn status(name: &str, path_exists: bool) -> RepoStatus {
        RepoStatus {
            name: name.into(),
            path: PathBuf::from(name),
            path_exists,
            branch: Some("main".into()),
            clean: true,
            on_main: true,
            error: None,
        }
    }

    fn rendered_tui(commits: &[Option<RepoCommits>]) -> String {
        let statuses = vec![status("pullkit", true)];
        let state = TuiState::new(statuses.len());
        let mut buffer = Vec::new();
        draw_tui(&mut buffer, &statuses, commits, &state, 120, 10).unwrap();
        String::from_utf8(buffer).unwrap()
    }

    #[test]
    fn tui_shows_placeholders_until_commit_inspection_arrives() {
        // Arrange
        let commits = RepoCommits {
            name: "pullkit".into(),
            path: PathBuf::from("pullkit"),
            local: Some(pullkit_core::CommitInfo {
                sha: "a".repeat(40),
                timestamp: 1_000_000,
                date: "2026-09-01 09:30".into(),
            }),
            remote: Some(pullkit_core::CommitInfo {
                sha: "b".repeat(40),
                timestamp: 1_000_000 + 2 * 86_400,
                date: "2026-09-03 09:30".into(),
            }),
            difference: Some("2 days behind".into()),
            error: None,
        };

        // Act
        let pending = rendered_tui(&[None]);
        let resolved = rendered_tui(&[Some(commits)]);

        // Assert
        assert!(pending.contains("LOCAL COMMIT"));
        assert!(pending.contains("fetching..."));
        assert!(resolved.contains("2026-09-01 09:30  2026-09-03 09:30  2 days behind"));
        assert!(!resolved.contains("fetching..."));
    }

    #[test]
    fn inspection_token_holds_a_javascript_timestamp() {
        // Arrange: what `Date.now()` returns, which the page uses as its seed.
        let milliseconds_since_1970: i64 = 1_788_323_990_678;

        // Act
        let token = InspectionToken::try_from(milliseconds_since_1970);

        // Assert: a narrower type makes every invoke fail before the command runs.
        assert!(token.is_ok());
    }

    fn cells() -> CommitCells {
        CommitCells {
            local: "2026-08-10 11:19".into(),
            remote: "2026-09-01 22:04".into(),
            difference: "3 weeks behind".into(),
        }
    }

    #[test]
    fn tui_row_keeps_the_difference_whatever_the_width_is() {
        // Arrange
        let left = "[ ] repository       Ready          ";
        let path = "/Users/example/workspace/repository";

        // Act
        let wide = tui_row(left, path, &cells(), 130);
        let medium = tui_row(left, path, &cells(), 100);
        let narrow = tui_row(left, path, &cells(), 80);
        let tiny = tui_row(left, path, &cells(), 60);
        let cramped = tui_row(left, path, &cells(), 50);
        let absurd = tui_row(left, path, &cells(), 10);

        // Assert: the difference reads in full at every usable width.
        for row in [&wide, &medium, &narrow, &tiny, &cramped] {
            assert!(row.contains("3 weeks behind"), "{row}");
        }
        // Narrower than the difference itself, there is nothing left to protect.
        assert_eq!(absurd, "3 weeks be");
        // The name survives as far as it fits; the status is what goes.
        assert!(cramped.starts_with("[ ] repository"));
        // The path goes first, then the remote date, then the local date.
        assert!(wide.contains("/Users/example"));
        assert!(wide.contains("2026-08-10 11:19") && wide.contains("2026-09-01 22:04"));
        assert!(!medium.contains("/Users/example"));
        assert!(medium.contains("2026-08-10 11:19") && medium.contains("2026-09-01 22:04"));
        assert!(narrow.contains("2026-08-10 11:19") && !narrow.contains("2026-09-01 22:04"));
        assert!(!tiny.contains("2026-08-10 11:19"));
        // Nothing overflows the terminal.
        for (row, width) in [
            (&wide, 130),
            (&medium, 100),
            (&narrow, 80),
            (&tiny, 60),
            (&cramped, 50),
            (&absurd, 10),
        ] {
            assert!(row.chars().count() <= width, "{width}: {row}");
        }
    }

    #[test]
    fn tui_selection_excludes_missing_paths() {
        let statuses = vec![status("available", true), status("missing", false)];
        let mut state = TuiState::new(statuses.len());

        state.toggle_current(&statuses);
        state.move_down();
        state.toggle_current(&statuses);

        assert_eq!(state.selected, vec![true, false]);

        state.toggle_all(&statuses);
        assert_eq!(state.selected, vec![false, false]);
    }
}
