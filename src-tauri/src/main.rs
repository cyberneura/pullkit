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
    config_path, initialize_config, load_config, sync_repos, RepoConfig, RepoStatus, SyncOutcome,
    SyncResult, EXAMPLE_CONFIG,
};
use std::{
    collections::HashSet,
    io::{self, IsTerminal, Write},
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
    println!(
        "{:<20} {:<8} {:<12} {:<8} PATH",
        "REPOSITORY", "TREE", "BRANCH", "ON MAIN"
    );
    for repo in repos {
        print_status(&pullkit_core::inspect(repo));
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
    let mut state = TuiState::new(repos.len());
    let mut terminal = TerminalSession::enter()?;

    loop {
        draw_tui(&mut terminal.stdout, &statuses, &state)?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
            KeyCode::Up | KeyCode::Char('k') => state.move_up(),
            KeyCode::Down | KeyCode::Char('j') => state.move_down(),
            KeyCode::Char(' ') => state.toggle_current(&statuses),
            KeyCode::Char('a') => state.toggle_all(&statuses),
            KeyCode::Enter if state.selected_count() > 0 => break,
            KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
            _ => {}
        }
    }

    drop(terminal);
    Ok(Some(
        repos
            .iter()
            .enumerate()
            .filter(|(index, _)| state.selected[*index])
            .map(|(_, repo)| repo.clone())
            .collect(),
    ))
}

fn draw_tui(stdout: &mut io::Stdout, statuses: &[RepoStatus], state: &TuiState) -> Result<()> {
    let (width, height) = terminal::size()?;
    let visible_rows = usize::from(height.saturating_sub(4).max(1));
    let first_row = state.cursor.saturating_sub(visible_rows - 1);

    queue!(
        stdout,
        Clear(ClearType::All),
        MoveTo(0, 0),
        SetForegroundColor(Color::Cyan),
        SetAttribute(Attribute::Bold),
        Print("pullkit repositories"),
        SetAttribute(Attribute::Reset),
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
        let line = format!(
            "{checkbox} {:<20} {:<14} {}",
            status.name,
            tui_status(status),
            status.path.display()
        );
        queue!(
            stdout,
            Print(truncate_line(&line, usize::from(width))),
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
        Print(truncate_line(&footer, usize::from(width))),
        ResetColor
    )?;
    stdout.flush()?;
    Ok(())
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

fn print_status(status: &RepoStatus) {
    if let Some(error) = &status.error {
        println!(
            "{:<20} {:<8} {:<12} {:<8} {} ({error})",
            status.name,
            "error",
            "-",
            "-",
            status.path.display()
        );
    } else {
        println!(
            "{:<20} {:<8} {:<12} {:<8} {}",
            status.name,
            if status.clean { "clean" } else { "dirty" },
            status.branch.as_deref().unwrap_or("-"),
            if status.on_main { "yes" } else { "no" },
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

#[tauri::command]
async fn sync_selected(
    app: tauri::AppHandle,
    names: Vec<String>,
) -> Result<Vec<SyncResult>, String> {
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
        .invoke_handler(tauri::generate_handler![list_repos, sync_selected])
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
