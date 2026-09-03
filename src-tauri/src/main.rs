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
    config_path, initialize_config, inspect_commits_parallel, load_config, sync_repos,
    validate_concurrency, Isolation, RepoCommits, RepoConfig, RepoStatus, SyncEvent, SyncOutcome,
    SyncResult, EXAMPLE_CONFIG,
};
use serde::Serialize;
use std::{
    collections::HashSet,
    io::{self, IsTerminal, Write},
    sync::{
        atomic::Ordering,
        mpsc::{self, Receiver},
    },
    thread,
    time::Duration,
};
use tauri::Emitter;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

mod sync_screen;

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
        /// How many repositories to pull and build at once, 1 to 10. Overrides
        /// `concurrency` in the configuration file.
        #[arg(short, long)]
        jobs: Option<usize>,
    },
}

fn main() {
    let args = Args::parse();
    let result = run(args);
    if let Err(error) = result {
        // A signal on its way out is what ends the process then: this thread
        // must not get to `exit` first, or the shell would see an exit code in
        // place of the signal.
        if EXITING.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(10));
        }
        // An error on any path may leave the fetches of a list or the commands
        // of a sync running in groups of their own, with this process about to
        // go; the list screen, for one, returns its errors straight up. The
        // stop comes first, and the message is not allowed to panic on a
        // closed stderr, so that nothing stands between the error and the stop.
        pullkit_core::stop_running_commands(Duration::from_secs(2));
        let _ = writeln!(io::stderr(), "pullkit: {error:#}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<()> {
    stop_commands_on_hangup()?;
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

/// Stops the commands and leaves when the terminal hangs up or the process is
/// told to end. The git commands that only read, the list's fetches, and the
/// pulls and builds of the screen and the GUI run in process groups of their
/// own, so the hangup that ends pullkit does not reach them, and the default
/// action of the signal would end pullkit before any of its ways out could
/// stop them. The pulls and builds of the plain output share this process's
/// group and are not on the list: a signal a terminal sends to the group, a
/// Ctrl-C or a hangup, reaches them as it always has, and one sent to
/// pullkit's pid alone leaves them to end on their own, as it always has.
#[cfg(unix)]
fn stop_commands_on_hangup() -> Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    // SIGINT is on the list for the moments a terminal is in cooked mode with
    // commands of their own group running: the wait for the list's fetches
    // before the screen, for one. The raw mode screens never generate it, and
    // the plain output's pulls and builds get it themselves.
    let mut signals = signal_hook::iterator::Signals::new([SIGHUP, SIGINT, SIGTERM])?;
    thread::Builder::new().spawn(move || {
        let Some(signal) = signals.forever().next() else {
            return;
        };
        pullkit_core::stop_running_commands(Duration::from_secs(3));
        // A screen may be up: its session is not dropped on this way out, and
        // the terminal would be left in raw mode on the alternate screen. No
        // new screen may start from here on either, or it would be left the
        // same way a moment later.
        EXITING.store(true, Ordering::SeqCst);
        // The gate is held to the end, so that no screen starts between the
        // look at the terminal and the death of the process.
        let _gate = terminal_gate();
        if terminal::is_raw_mode_enabled().unwrap_or(false) {
            let _ = execute!(io::stdout(), Show, LeaveAlternateScreen, ResetColor);
            let _ = terminal::disable_raw_mode();
        }
        // Die of the signal itself rather than exit with a code: a shell script
        // decides whether to stop on Ctrl-C by whether its child died of
        // SIGINT, not by the code it returned. The call does not return: it
        // raises the signal with its default action restored, and aborts if
        // it cannot.
        let _ = signal_hook::low_level::emulate_default_handler(signal);
        std::process::exit(128 + signal);
    })?;
    Ok(())
}

#[cfg(windows)]
fn stop_commands_on_hangup() -> Result<()> {
    Ok(())
}

fn run_cli(command: Option<CliCommand>) -> Result<()> {
    let config = load_config()?;
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    match command {
        None => {
            if interactive {
                if let Some(repos) = run_tui(&config.repos)? {
                    run_sync(repos, config.concurrency)?;
                }
            } else {
                print_repo_list(&config.repos);
            }
            Ok(())
        }
        Some(CliCommand::Sync { only, jobs }) => {
            let repos = select_repos(&config.repos, &only)?;
            let concurrency = match jobs {
                Some(jobs) => {
                    validate_concurrency(jobs)?;
                    jobs
                }
                None => config.concurrency,
            };
            run_sync(repos, concurrency)
        }
    }
}

/// Nothing bounds the width of a pipe, so the columns here are padded and never
/// cut: two repositories that share their first twenty columns must not come
/// out looking like the same one.
fn print_repo_list(repos: &[RepoConfig]) {
    let statuses: Vec<_> = repos.iter().map(pullkit_core::inspect).collect();
    let mut commits: Vec<Option<RepoCommits>> = vec![None; repos.len()];
    inspect_commits_parallel(repos, |index, item| commits[index] = Some(item));
    print_line(&format!(
        "{} {:<8} {} {:<8} {:<DATE_WIDTH$} {:<DATE_WIDTH$} {:<DIFFERENCE_WIDTH$} PATH",
        pad_to_width("REPOSITORY", NAME_WIDTH),
        "TREE",
        pad_to_width("BRANCH", BRANCH_WIDTH),
        "ON MAIN",
        "LOCAL COMMIT",
        "REMOTE COMMIT",
        "DIFFERENCE"
    ));
    for (status, commits) in statuses.iter().zip(&commits) {
        print_status(status, commits.as_ref());
    }
    // For a helper that a hook of one of the git commands left behind. The
    // lines above go through `print_line` so that a closed pipe ends in a stop
    // rather than in a panic this call would never be reached past.
    pullkit_core::stop_leftover_commands(Duration::from_secs(3));
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
const NAME_WIDTH: usize = 20;
const STATUS_WIDTH: usize = 14;
const BRANCH_WIDTH: usize = 12;

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

/// Writes one line of the plain output. A reader that has gone away, `head`
/// say, closes the pipe, and going on would only leave the sync running for
/// nobody; `println!` would panic instead, and the panic would wait for every
/// worker, and so for a build with no timeout, before it got anywhere.
fn print_line(line: &str) {
    let mut stdout = io::stdout().lock();
    if writeln!(stdout, "{line}")
        .and_then(|()| stdout.flush())
        .is_err()
    {
        // The other workers may be in the middle of a git command with a
        // group of its own; the pulls and builds of this path share this
        // process's group and are left to end on their own, as before.
        pullkit_core::stop_running_commands(Duration::from_secs(1));
        std::process::exit(1);
    }
}

/// On a terminal the sync gets a screen with one pane per worker; anywhere
/// else, a pipe or a cron job, the lines of every repository are written one
/// after another with the repository's name in front. The screen's raw mode
/// turns Ctrl-C into a key press, so the screen passes it on to the commands
/// itself; the plain output leaves them in the terminal's process group, where
/// Ctrl-C reaches them as it always has.
fn run_sync(repos: Vec<RepoConfig>, concurrency: usize) -> Result<()> {
    let count = repos.len();
    let header = |workers: usize| {
        print_line(&format!(
            "pullkit run: {count} repositor{}, {workers} at a time",
            if count == 1 { "y" } else { "ies" }
        ));
    };
    let (results, aborted) = if io::stdin().is_terminal() && io::stdout().is_terminal() {
        let outcome = sync_screen::run(repos, concurrency)?;
        header(concurrency.min(count));
        (outcome.results, outcome.aborted)
    } else {
        let results = sync_repos(
            &repos,
            concurrency,
            Isolation::SharedGroup,
            |event| match event {
                SyncEvent::Planned { workers } => {
                    header(workers);
                    print_line("");
                }
                SyncEvent::Line { name, line, .. } => print_line(&format!("[{name}] {line}")),
                SyncEvent::Started { .. } | SyncEvent::Finished { .. } => {}
            },
        );
        // The pulls and builds of this path shared this process's group and
        // are out of reach here, as is anything they left behind; this is for
        // the git commands that only read, which have groups of their own.
        pullkit_core::stop_leftover_commands(Duration::from_secs(3));
        (results, false)
    };
    print_summary(&results);
    let not_started = count - results.len();
    if aborted && not_started > 0 {
        print_line(&format!(
            "\naborted: {not_started} repositor{} never started",
            if not_started == 1 { "y" } else { "ies" }
        ));
    } else if aborted {
        print_line("\naborted");
    }
    let failed = results.iter().any(|result| {
        matches!(
            result.outcome,
            SyncOutcome::PullFailed | SyncOutcome::BuildFailed | SyncOutcome::StatusFailed
        )
    });
    if failed || aborted {
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

/// Set once a signal has started the way out, so that no screen is entered
/// after the terminal was put back.
static EXITING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Held while the terminal is being put into raw mode or taken out of it on
/// the way out, so that the two cannot interleave: a screen entered just after
/// the signal thread had looked would be left raw when the process dies.
static TERMINAL_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn terminal_gate() -> std::sync::MutexGuard<'static, ()> {
    TERMINAL_GATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct TerminalSession {
    stdout: io::Stdout,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        let _gate = terminal_gate();
        if EXITING.load(Ordering::SeqCst) {
            bail!("pullkit is exiting");
        }
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
/// They are asked to stop and given a moment, so that a fetch can remove its
/// lock files, then killed if still there.
fn leave_tui() -> Option<Vec<RepoConfig>> {
    pullkit_core::stop_running_commands(Duration::from_secs(2));
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
        &format!(
            "    {} {} ",
            fit_to_width("REPOSITORY", NAME_WIDTH),
            fit_to_width("STATUS", STATUS_WIDTH)
        ),
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
            &format!(
                "{checkbox} {} {} ",
                fit_to_width(&status.name, NAME_WIDTH),
                fit_to_width(&tui_status(status), STATUS_WIDTH)
            ),
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
        Print(truncate_to_width(&footer, width)),
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
    let difference = pad_to_width(&cells.difference, DIFFERENCE_WIDTH);
    let candidates = [
        format!(
            "{}  {}  {difference}",
            fit_to_width(&cells.local, DATE_WIDTH),
            fit_to_width(&cells.remote, DATE_WIDTH)
        ),
        format!("{}  {difference}", fit_to_width(&cells.local, DATE_WIDTH)),
        difference.clone(),
    ];

    for columns in &candidates {
        let fixed = display_width(left) + display_width(columns) + 2;
        if width >= fixed + MIN_PATH_WIDTH {
            let path_width = width - fixed;
            return format!("{left}{}  {columns}", fit_to_width(path, path_width));
        }
        if width >= display_width(left) + display_width(columns) {
            return format!("{left}{columns}");
        }
    }
    // Nothing on the right is left to give up, so the left side yields instead.
    // Truncating the row would cut the difference off the end, which is the one
    // thing this layout exists to keep. The room set aside is the same on every
    // row rather than the length of this row's own text, so the column stays put
    // down the list instead of shifting with each label. A label longer than the
    // usual reservation still gets the room it needs, or the row would run past
    // the edge and wrap onto the next one.
    let reserved = DIFFERENCE_WIDTH.max(display_width(&cells.difference));
    let Some(left_width) = width.checked_sub(reserved + 1) else {
        return truncate_to_width(&cells.difference, width);
    };
    format!("{} {}", fit_to_width(left, left_width), cells.difference)
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

/// The number of columns a terminal draws the text in. A Japanese character or
/// an emoji takes two, so counting scalars would understate it and let a row
/// run past the edge and wrap onto the next one.
///
/// The count is taken per grapheme cluster, not per scalar. An emoji assembled
/// from several scalars, a skin tone or a family joined with zero width
/// joiners, is drawn in two columns however many scalars went into it.
fn display_width(text: &str) -> usize {
    text.graphemes(true).map(UnicodeWidthStr::width).sum()
}

/// Cuts the text down to `width` columns, never inside a grapheme cluster: half
/// of an emoji is not a character the terminal can draw. A cluster that would
/// straddle the limit is dropped, so a row can come out one column short of the
/// width rather than one past it.
fn truncate_to_width(text: &str, width: usize) -> String {
    let mut kept = String::new();
    let mut used = 0;
    for cluster in text.graphemes(true) {
        let cluster_width = UnicodeWidthStr::width(cluster);
        if used + cluster_width > width {
            break;
        }
        kept.push_str(cluster);
        used += cluster_width;
    }
    kept
}

/// Fills the text out to `width` columns, and leaves it alone when it already
/// runs past them. For the last column on a row, where there is nothing to keep
/// the text clear of and cutting it would lose what the row is there to say.
fn pad_to_width(text: &str, width: usize) -> String {
    let padding = width.saturating_sub(display_width(text));
    format!("{text}{}", " ".repeat(padding))
}

/// Cuts the text to `width` columns and fills what is left with spaces, so the
/// next column starts where it does on every other row. `{:<width$}` cannot do
/// this, because it counts scalars.
fn fit_to_width(text: &str, width: usize) -> String {
    pad_to_width(&truncate_to_width(text, width), width)
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
        print_line(&format!(
            "{} {:<8} {} {:<8} {:<DATE_WIDTH$} {:<DATE_WIDTH$} {:<DIFFERENCE_WIDTH$} {} ({error})",
            pad_to_width(&status.name, NAME_WIDTH),
            "error",
            pad_to_width("-", BRANCH_WIDTH),
            "-",
            cells.local,
            cells.remote,
            cells.difference,
            status.path.display()
        ));
    } else {
        print_line(&format!(
            "{} {:<8} {} {:<8} {:<DATE_WIDTH$} {:<DATE_WIDTH$} {:<DIFFERENCE_WIDTH$} {}",
            pad_to_width(&status.name, NAME_WIDTH),
            if status.clean { "clean" } else { "dirty" },
            pad_to_width(status.branch.as_deref().unwrap_or("-"), BRANCH_WIDTH),
            if status.on_main { "yes" } else { "no" },
            cells.local,
            cells.remote,
            cells.difference,
            status.path.display()
        ));
    }
}

fn print_summary(results: &[SyncResult]) {
    print_line("\nSummary");
    for result in results {
        print_line(&format!(
            "  {} {:<16} {}",
            pad_to_width(&result.name, NAME_WIDTH),
            format!("{:?}", result.outcome).to_lowercase(),
            result.message
        ));
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

#[derive(Clone, Serialize)]
struct SyncEventEnvelope {
    token: InspectionToken,
    event: SyncEvent,
}

/// Syncs the named repositories `concurrency` at a time and emits one
/// `sync-event` per event, so the window can keep one pane per worker. The
/// commands get process groups of their own: the window has no terminal to
/// deliver Ctrl-C, so closing it is what stops them, through the window event
/// below. `token` comes back on every event, so that a page reloaded during a
/// sync can tell that sync's events from those of one it starts itself.
#[tauri::command]
async fn sync_selected(
    app: tauri::AppHandle,
    names: Vec<String>,
    token: InspectionToken,
) -> Result<Vec<SyncResult>, String> {
    if names.is_empty() {
        // `select_repos` reads an empty list as "every repository", which is the
        // CLI's contract for `--only`, never what an empty selection means here.
        return Err("no repositories were selected".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let config = load_config().map_err(|error| format!("{error:#}"))?;
        let repos = select_repos(&config.repos, &names).map_err(|error| format!("{error:#}"))?;
        let results = sync_repos(&repos, config.concurrency, Isolation::OwnGroup, |event| {
            let _ = app.emit("sync-event", SyncEventEnvelope { token, event });
        });
        // The window stays open for another sync, so what a build left behind
        // is stopped now rather than at the window's close, and without
        // abandoning anything.
        pullkit_core::stop_leftover_commands(Duration::from_secs(3));
        Ok(results)
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Quitting the GUI asks the running commands to stop, waits a little, and
/// kills what is still running: a `git pull` or a build that gets the request
/// cleans up after itself, where one killed outright can leave a lock file in
/// the repository, but one that ignores the request would otherwise outlive
/// the window with nothing left to stop it. A fetch of the list, and a process
/// a build left running in the background, stop the same way. The stop hangs
/// on the exit of the event loop rather than on the window: Cmd+Q on macOS
/// ends the application without closing the window first, and the window's
/// own close reaches the exit as well.
fn run_gui() -> Result<()> {
    tauri::Builder::default()
        .setup(|_app| {
            set_dock_icon();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_repos,
            inspect_all_commits,
            sync_selected
        ])
        .build(tauri::generate_context!())?
        .run(|_app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                pullkit_core::stop_running_commands(Duration::from_secs(3));
            }
        });
    Ok(())
}

/// Puts the application icon in the Dock.
///
/// A bundled app carries its icon in the .app and needs none of this, but what
/// ships here is a bare binary: macOS finds no Info.plist, and the Dock falls
/// back to the generic executable tile. The icon is the same file a bundle
/// would use, compiled in so that it travels with the binary.
///
/// Anything short of setting it is ignored rather than reported: an icon is
/// decoration, and a window that opens without one beats no window at all.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::AnyThread;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{MainThreadMarker, NSData};

    // setup runs on the main thread, which is where AppKit insists this happen.
    let Some(marker) = MainThreadMarker::new() else {
        return;
    };
    let data = NSData::with_bytes(include_bytes!("../icons/icon.png"));
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        return;
    };
    // Unsafe only in that AppKit is: the marker already proves the thread, and
    // the image is one this function just built.
    unsafe {
        NSApplication::sharedApplication(marker).setApplicationIconImage(Some(&image));
    }
}

#[cfg(not(target_os = "macos"))]
fn set_dock_icon() {}

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
    fn display_width_counts_an_emoji_sequence_as_one_pair_of_columns() {
        // Arrange & Act & Assert: the scalar count is wrong for every one of
        // these; the number of columns a terminal draws is what matters.
        assert_eq!(display_width("repo"), 4);
        assert_eq!(display_width("日本語"), 6);
        assert_eq!(display_width("👍"), 2);
        assert_eq!(display_width("👍🏽"), 2, "an emoji with a skin tone");
        assert_eq!(
            display_width("👨‍👩‍👧‍👦"),
            2,
            "a family joined with zero width joiners"
        );
        assert_eq!(
            display_width("❤️"),
            2,
            "an emoji written with a variation selector"
        );
        assert_eq!(
            display_width("🇯🇵"),
            2,
            "a flag built from regional indicators"
        );
        assert_eq!(display_width("🚀 deploy 日本"), 14);
    }

    #[test]
    fn truncation_never_splits_an_emoji() {
        // Arrange
        let family = "👨‍👩‍👧‍👦";
        let text = format!("{family}{family}");

        // Act: three columns hold one two column emoji and no half of another.
        let kept = truncate_to_width(&text, 3);

        // Assert
        assert_eq!(kept, family);
        assert_eq!(display_width(&kept), 2);
        // A single column cannot hold it at all, and half of it is not a thing
        // the terminal can draw.
        assert_eq!(truncate_to_width(&text, 1), "");
    }

    #[test]
    fn padding_fills_the_columns_a_wide_name_leaves() {
        // Arrange & Act
        let wide = fit_to_width("日本語", 10);
        let narrow = fit_to_width("repo", 10);
        let overlong = fit_to_width("日本語リポジトリ", 7);
        let unbounded = pad_to_width("日本語リポジトリ", 7);

        // Assert: every field ends at the same column whatever it holds.
        assert_eq!(display_width(&wide), 10);
        assert_eq!(display_width(&narrow), 10);
        assert_eq!(
            display_width(&overlong),
            7,
            "an odd width leaves one column"
        );
        assert_eq!(overlong, "日本語 ");
        // Padding alone leaves a value that outgrows its column whole, for the
        // outputs that have no width to keep it inside.
        assert_eq!(unbounded, "日本語リポジトリ");
    }

    #[test]
    fn tui_row_fits_the_terminal_when_the_name_is_wide() {
        // Arrange
        let left = format!(
            "[ ] {} {} ",
            fit_to_width("🚀日本語リポジトリ", NAME_WIDTH),
            fit_to_width("Ready", STATUS_WIDTH)
        );
        let ascii = format!(
            "[ ] {} {} ",
            fit_to_width("ascii-repo", NAME_WIDTH),
            fit_to_width("Ready", STATUS_WIDTH)
        );

        // Act
        let rows: Vec<_> = [80, 100, 120]
            .map(|width| {
                (
                    width,
                    tui_row(&left, "/w/repo", &cells(), width),
                    tui_row(&ascii, "/w/repo", &cells(), width),
                )
            })
            .into_iter()
            .collect();

        // Assert
        for (width, wide_row, ascii_row) in rows {
            assert!(display_width(&wide_row) <= width, "{width}: {wide_row}");
            assert!(wide_row.contains("3 weeks behind"), "{width}: {wide_row}");
            // Both rows put the difference in the same column, so the list
            // reads down the page.
            assert_eq!(
                display_width(&wide_row),
                display_width(&ascii_row),
                "{width}: a wide name shifts the row"
            );
        }
    }

    #[test]
    fn tui_row_makes_room_for_a_difference_past_the_usual_width() {
        // Arrange: commit dates are arbitrary, so the gap has no upper bound,
        // and this label is 22 columns against a 21 column reservation.
        let cells = CommitCells {
            local: "0999-01-01 00:00".into(),
            remote: "2026-01-01 00:00".into(),
            difference: "diverged by 1027 years".into(),
        };
        let left = "[ ] repository       Ready          ";

        // Act: at every width, not only the one that reaches the last resort.
        let rows: Vec<_> = [130, 100, 80, 50]
            .map(|width| (width, tui_row(left, "/w/repository", &cells, width)))
            .into_iter()
            .collect();

        // Assert
        for (width, row) in rows {
            assert!(row.ends_with("diverged by 1027 years"), "{width}: {row}");
            assert!(display_width(&row) <= width, "{width}: {row}");
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
