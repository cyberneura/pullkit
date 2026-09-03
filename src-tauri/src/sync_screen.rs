//! The full screen view of a running sync: one pane per worker, each showing
//! the tail of what its repositories wrote, redrawn as the lines arrive.

use crate::{display_width, fit_to_width, truncate_to_width, TerminalSession};
use anyhow::Result;
use crossterm::{
    cursor::MoveTo,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use pullkit_core::{sync_repos, Isolation, RepoConfig, SyncEvent, SyncOutcome, SyncResult};
use std::{
    collections::VecDeque,
    io::Write,
    sync::mpsc::{self, TryRecvError},
    thread,
    time::{Duration, Instant},
};

/// Lines kept per pane. A pane shows its tail, and a build that writes more
/// than this has said what it needed to say long before the end.
const PANE_HISTORY: usize = 500;
const MERGED_HISTORY: usize = 2_000;
/// A pane with fewer log rows than this shows too little of a build to follow;
/// below it, the panes give way to one merged log.
const MIN_LOG_ROWS: usize = 3;
/// Frames are drawn no more often than this. A build writes lines faster than a
/// terminal draws frames, and redrawing on every line would only add flicker.
const FRAME_INTERVAL: Duration = Duration::from_millis(50);
/// Events taken in before the keyboard is looked at again. Several builds
/// writing flat out can keep the channel from ever running empty, and a Ctrl-C
/// must not wait for that.
const EVENTS_PER_FRAME: usize = 2_000;
/// How long a stop waits for the commands to end on their own before killing
/// them, when the screen leaves with something still running.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// What the user asked for while the screen was up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Abort {
    None,
    /// Ctrl-C once: the commands were asked to stop and can clean up.
    Requested,
    /// Ctrl-C twice: the commands were killed.
    Forced,
}

struct Pane {
    /// The repository the worker holds or last held.
    name: Option<String>,
    running: bool,
    lines: VecDeque<String>,
}

pub struct SyncScreen {
    panes: Vec<Pane>,
    merged: VecDeque<String>,
    total: usize,
    finished: Vec<SyncResult>,
    abort: Abort,
    all_done: bool,
}

/// One row of a frame, as text and the colour it is drawn in.
#[derive(Debug, PartialEq, Eq)]
pub struct Row {
    text: String,
    color: Option<Color>,
}

impl SyncScreen {
    pub fn new(total: usize) -> Self {
        Self {
            panes: Vec::new(),
            merged: VecDeque::new(),
            total,
            finished: Vec::new(),
            abort: Abort::None,
            all_done: false,
        }
    }

    pub fn apply(&mut self, event: SyncEvent) {
        match event {
            SyncEvent::Planned { workers } => {
                self.panes = (0..workers)
                    .map(|_| Pane {
                        name: None,
                        running: false,
                        lines: VecDeque::new(),
                    })
                    .collect();
            }
            SyncEvent::Started { worker, name } => {
                let Some(pane) = self.panes.get_mut(worker) else {
                    return;
                };
                // The pane keeps the log of the repositories before this one,
                // so the new one is set off from them.
                if !pane.lines.is_empty() {
                    push_line(&mut pane.lines, PANE_HISTORY, String::new());
                    push_line(&mut pane.lines, PANE_HISTORY, repo_separator(&name));
                }
                pane.name = Some(name);
                pane.running = true;
            }
            SyncEvent::Line { worker, name, line } => {
                push_line(&mut self.merged, MERGED_HISTORY, format!("[{name}] {line}"));
                let Some(pane) = self.panes.get_mut(worker) else {
                    return;
                };
                push_line(&mut pane.lines, PANE_HISTORY, line);
            }
            SyncEvent::Finished { worker, result } => {
                self.finished.push(result);
                if let Some(pane) = self.panes.get_mut(worker) {
                    pane.running = false;
                }
            }
        }
    }

    /// Lays the screen out as `height` rows of at most `width` columns each.
    /// Each row is padded to the full width, so drawing a frame over the last
    /// one needs no clearing, except the last row, which is left unpadded: a
    /// character in the bottom right corner makes some terminals scroll.
    pub fn frame(&self, width: usize, height: usize) -> Vec<Row> {
        let mut rows = Vec::with_capacity(height);
        let Some(body_rows) = height.checked_sub(1) else {
            return rows;
        };
        let pane_rows = body_rows.checked_div(self.panes.len()).unwrap_or(0);
        let log_rows = pane_rows.saturating_sub(1);
        if log_rows >= MIN_LOG_ROWS {
            for (index, pane) in self.panes.iter().enumerate() {
                rows.push(title_row(&pane_title(index, pane), width));
                rows.extend(tail_rows(&pane.lines, log_rows, width));
            }
        } else if body_rows > 0 {
            let title = if self.panes.is_empty() {
                "pullkit sync  starting".to_owned()
            } else {
                format!(
                    "pullkit sync  {} workers, one log: the terminal is too small for panes",
                    self.panes.len()
                )
            };
            rows.push(title_row(&title, width));
            rows.extend(tail_rows(&self.merged, body_rows - 1, width));
        }
        while rows.len() < body_rows {
            rows.push(Row {
                text: " ".repeat(width),
                color: None,
            });
        }
        rows.push(Row {
            text: truncate_to_width(&self.footer(), width.saturating_sub(1)),
            color: Some(Color::DarkCyan),
        });
        rows
    }

    fn footer(&self) -> String {
        let done = self.finished.len();
        let total = self.total;
        if self.all_done {
            let failed = self.finished.iter().filter(|r| failed(r)).count();
            let skipped = self
                .finished
                .iter()
                .filter(|r| {
                    matches!(
                        r.outcome,
                        SyncOutcome::SkippedDirty | SyncOutcome::SkippedBranch
                    )
                })
                .count();
            let succeeded = done - failed - skipped;
            let missing = total - done;
            let aborted = if missing > 0 {
                format!(", {missing} not started")
            } else {
                String::new()
            };
            return format!(
                "Done: {succeeded} ok, {failed} failed, {skipped} skipped{aborted}    press any key"
            );
        }
        match self.abort {
            Abort::None => format!("{done}/{total} done    Ctrl-C abort"),
            Abort::Requested => {
                format!("{done}/{total} done    aborting, waiting for commands to stop    Ctrl-C again to kill them")
            }
            Abort::Forced => {
                format!("{done}/{total} done    killed, waiting for the workers to stop")
            }
        }
    }
}

fn failed(result: &SyncResult) -> bool {
    matches!(
        result.outcome,
        SyncOutcome::PullFailed | SyncOutcome::BuildFailed | SyncOutcome::StatusFailed
    )
}

fn push_line(lines: &mut VecDeque<String>, capacity: usize, line: String) {
    if lines.len() == capacity {
        lines.pop_front();
    }
    lines.push_back(line);
}

/// The line that opens a repository's part of a pane. It starts with a rule
/// so that it can be told from output: no command starts a line with one.
fn repo_separator(name: &str) -> String {
    format!("\u{2500}\u{2500} {name}")
}

fn pane_title(index: usize, pane: &Pane) -> String {
    let worker = index + 1;
    match (&pane.name, pane.running) {
        (None, _) => format!("worker {worker}  waiting"),
        (Some(name), true) => format!("worker {worker}  {name}"),
        (Some(name), false) => format!("worker {worker}  {name}  done"),
    }
}

/// A rule with the title set into it, the way a pane border is drawn.
fn title_row(title: &str, width: usize) -> Row {
    let label = truncate_to_width(&format!(" {title} "), width.saturating_sub(2));
    let rule = "\u{2500}".repeat(width.saturating_sub(display_width(&label) + 2));
    Row {
        text: fit_to_width(&format!("\u{2500}\u{2500}{label}{rule}"), width),
        color: Some(Color::DarkCyan),
    }
}

/// The last `count` lines, each cut to the width rather than wrapped: a
/// wrapped line would take a row from the lines above it, and the tail of a
/// log is read for what happened last, not for every column of it.
fn tail_rows(lines: &VecDeque<String>, count: usize, width: usize) -> Vec<Row> {
    let skip = lines.len().saturating_sub(count);
    let mut rows: Vec<Row> = lines
        .iter()
        .skip(skip)
        .map(|line| Row {
            text: fit_to_width(line, width),
            color: line_color(line),
        })
        .collect();
    while rows.len() < count {
        rows.push(Row {
            text: " ".repeat(width),
            color: None,
        });
    }
    rows
}

fn line_color(line: &str) -> Option<Color> {
    if line.starts_with("ERROR ") {
        Some(Color::Red)
    } else if line.starts_with("WARN ") {
        Some(Color::Yellow)
    } else if line.starts_with("OK ") {
        Some(Color::Green)
    } else if line.starts_with('\u{2500}') {
        Some(Color::DarkGrey)
    } else {
        None
    }
}

fn draw(stdout: &mut impl Write, screen: &SyncScreen, width: usize, height: usize) -> Result<()> {
    let rows = screen.frame(width, height);
    let last = rows.len().saturating_sub(1);
    for (index, row) in rows.iter().enumerate() {
        queue!(stdout, MoveTo(0, u16::try_from(index).unwrap_or(u16::MAX)))?;
        if index == last {
            queue!(stdout, Clear(ClearType::CurrentLine))?;
        }
        if let Some(color) = row.color {
            queue!(stdout, SetForegroundColor(color))?;
        }
        queue!(stdout, Print(&row.text), ResetColor)?;
    }
    stdout.flush()?;
    Ok(())
}

/// What the screen ends with: the results of the repositories that ran, and
/// whether the user aborted before all of them had.
pub struct Outcome {
    pub results: Vec<SyncResult>,
    pub aborted: bool,
}

/// Runs the sync on a background thread and shows it until it ends and a key
/// is pressed. The commands run in process groups of their own: the terminal is
/// in raw mode, so Ctrl-C arrives here as a key press, and the screen has to
/// pass it on itself. Should the screen itself fail, because the terminal went
/// away, the commands are stopped before the error is returned; nothing else
/// would be left to stop them.
pub fn run(repos: Vec<RepoConfig>, concurrency: usize) -> Result<Outcome> {
    let mut terminal = TerminalSession::enter()?;
    let mut screen = SyncScreen::new(repos.len());
    // Bounded like the channel inside `sync_repos`, and for the same reason.
    let (tx, rx) = mpsc::sync_channel(pullkit_core::EVENT_QUEUE);
    let worker = thread::spawn(move || {
        sync_repos(&repos, concurrency, Isolation::OwnGroup, |event| {
            let _ = tx.send(event);
        })
    });
    let watched = watch(&mut terminal, &mut screen, &rx);
    // Nothing drains the channel from here on. A sender blocked on it would
    // keep the sync thread, and the join below, waiting for good.
    drop(rx);
    match &watched {
        // The run is over; what is still listed is something a build left
        // behind, which would otherwise outlive pullkit.
        Ok(()) => pullkit_core::stop_leftover_commands(STOP_GRACE),
        // The screen is gone with commands still running and nothing left to
        // press Ctrl-C at.
        Err(_) => pullkit_core::stop_running_commands(STOP_GRACE),
    }
    drop(terminal);
    let results = worker
        .join()
        .map_err(|_| anyhow::anyhow!("the sync thread panicked"))?;
    watched?;
    let aborted = screen.abort != Abort::None;
    Ok(Outcome { results, aborted })
}

/// Draws the screen and answers the keyboard until every worker has reported
/// and a key has been pressed.
fn watch(
    terminal: &mut TerminalSession,
    screen: &mut SyncScreen,
    rx: &mpsc::Receiver<SyncEvent>,
) -> Result<()> {
    let mut dirty = true;
    let mut last_frame = Instant::now() - FRAME_INTERVAL;
    loop {
        for _ in 0..EVENTS_PER_FRAME {
            match rx.try_recv() {
                Ok(event) => {
                    screen.apply(event);
                    dirty = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !screen.all_done {
                        screen.all_done = true;
                        dirty = true;
                    }
                    break;
                }
            }
        }
        if dirty && (screen.all_done || last_frame.elapsed() >= FRAME_INTERVAL) {
            let (width, height) = terminal::size()?;
            draw(
                &mut terminal.stdout,
                screen,
                usize::from(width),
                usize::from(height),
            )?;
            dirty = false;
            last_frame = Instant::now();
        }
        if !event::poll(FRAME_INTERVAL)? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(key) => key,
            Event::Resize(..) => {
                dirty = true;
                continue;
            }
            _ => continue,
        };
        // A held key repeats, and two Ctrl-C from one press would kill the
        // commands outright when the user meant to ask them to stop once.
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if screen.all_done {
            return Ok(());
        }
        let is_ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if !is_ctrl_c {
            continue;
        }
        match screen.abort {
            Abort::None => {
                pullkit_core::interrupt_running_commands();
                screen.abort = Abort::Requested;
            }
            // A command that started in the moment between the kill and its
            // registration was not on the list then; a further Ctrl-C reaches
            // it, rather than being ignored.
            Abort::Requested | Abort::Forced => {
                pullkit_core::terminate_running_commands();
                screen.abort = Abort::Forced;
            }
        }
        dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn planned(workers: usize, total: usize) -> SyncScreen {
        let mut screen = SyncScreen::new(total);
        screen.apply(SyncEvent::Planned { workers });
        screen
    }

    fn result(name: &str, outcome: SyncOutcome) -> SyncResult {
        SyncResult {
            name: name.into(),
            path: PathBuf::from(name),
            outcome,
            branch: Some("main".into()),
            pull_output: None,
            build_output: None,
            message: String::new(),
        }
    }

    fn texts(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|row| row.text.as_str()).collect()
    }

    #[test]
    fn splits_the_screen_into_one_pane_per_worker() {
        // Arrange
        let mut screen = planned(4, 6);
        screen.apply(SyncEvent::Started {
            worker: 2,
            name: "pullkit".into(),
        });
        screen.apply(SyncEvent::Line {
            worker: 2,
            name: "pullkit".into(),
            line: "pulling from origin".into(),
        });

        // Act
        let rows = screen.frame(100, 41);

        // Assert: 40 body rows make four panes of ten, then the footer.
        assert_eq!(rows.len(), 41);
        let texts = texts(&rows);
        assert!(texts[0].contains("worker 1  waiting"), "{}", texts[0]);
        assert!(texts[10].contains("worker 2  waiting"), "{}", texts[10]);
        assert!(texts[20].contains("worker 3  pullkit"), "{}", texts[20]);
        assert!(
            texts[21].starts_with("pulling from origin"),
            "{}",
            texts[21]
        );
        assert!(texts[30].contains("worker 4  waiting"), "{}", texts[30]);
        assert!(texts[40].contains("0/6 done"), "{}", texts[40]);
        for row in &rows[..40] {
            assert_eq!(display_width(&row.text), 100, "{}", row.text);
        }
    }

    #[test]
    fn falls_back_to_one_merged_log_when_the_panes_would_be_too_short() {
        // Arrange: ten workers on 24 rows leave two rows per pane.
        let mut screen = planned(10, 12);
        for worker in 0..10 {
            screen.apply(SyncEvent::Line {
                worker,
                name: format!("repo-{worker}"),
                line: "checking repository".into(),
            });
        }

        // Act
        let rows = screen.frame(80, 24);

        // Assert
        assert_eq!(rows.len(), 24);
        let texts = texts(&rows);
        assert!(texts[0].contains("too small for panes"), "{}", texts[0]);
        assert!(texts[1].starts_with("[repo-0] checking repository"));
        assert!(texts[10].starts_with("[repo-9] checking repository"));
        assert!(!texts.iter().any(|text| text.contains("worker 1")));
    }

    #[test]
    fn shows_the_tail_of_a_pane_and_cuts_wide_lines_at_the_edge() {
        // Arrange: one worker, a 12 row screen, so the pane has 10 log rows.
        let mut screen = planned(1, 1);
        screen.apply(SyncEvent::Started {
            worker: 0,
            name: "repo".into(),
        });
        for index in 0..30 {
            screen.apply(SyncEvent::Line {
                worker: 0,
                name: "repo".into(),
                line: format!("line {index} 日本語の長い出力がここに続きます"),
            });
        }

        // Act
        let rows = screen.frame(30, 12);

        // Assert
        let texts = texts(&rows);
        assert!(texts[1].starts_with("line 20 "), "{}", texts[1]);
        assert!(texts[10].starts_with("line 29 "), "{}", texts[10]);
        for row in &rows {
            assert!(display_width(&row.text) <= 30, "{}", row.text);
        }
    }

    #[test]
    fn colours_the_status_lines_and_the_titles() {
        // Arrange
        let mut screen = planned(1, 1);
        for line in [
            "ERROR git pull failed (exit 1)",
            "WARN skipped",
            "OK sync completed",
            "plain",
        ] {
            screen.apply(SyncEvent::Line {
                worker: 0,
                name: "repo".into(),
                line: line.into(),
            });
        }

        // Act
        let rows = screen.frame(60, 6);

        // Assert
        assert_eq!(rows[0].color, Some(Color::DarkCyan));
        assert_eq!(rows[1].color, Some(Color::Red));
        assert_eq!(rows[2].color, Some(Color::Yellow));
        assert_eq!(rows[3].color, Some(Color::Green));
        assert_eq!(rows[4].color, None);
    }

    #[test]
    fn footer_counts_the_outcomes_once_everything_has_run() {
        // Arrange
        let mut screen = planned(2, 4);
        screen.apply(SyncEvent::Finished {
            worker: 0,
            result: result("a", SyncOutcome::Succeeded),
        });
        screen.apply(SyncEvent::Finished {
            worker: 1,
            result: result("b", SyncOutcome::BuildFailed),
        });
        screen.apply(SyncEvent::Finished {
            worker: 0,
            result: result("c", SyncOutcome::SkippedDirty),
        });
        let running = screen.footer();
        screen.all_done = true;

        // Act
        let done = screen.footer();

        // Assert: the fourth repository never started, as after an abort.
        assert_eq!(running, "3/4 done    Ctrl-C abort");
        assert_eq!(
            done,
            "Done: 1 ok, 1 failed, 1 skipped, 1 not started    press any key"
        );
    }

    #[test]
    fn a_pane_marks_its_repository_done_and_keeps_the_log() {
        // Arrange
        let mut screen = planned(1, 2);
        screen.apply(SyncEvent::Started {
            worker: 0,
            name: "first".into(),
        });
        screen.apply(SyncEvent::Line {
            worker: 0,
            name: "first".into(),
            line: "OK sync completed".into(),
        });
        screen.apply(SyncEvent::Finished {
            worker: 0,
            result: result("first", SyncOutcome::Succeeded),
        });

        // Act
        let rows = screen.frame(60, 6);

        // Assert
        assert!(
            rows[0].text.contains("worker 1  first  done"),
            "{}",
            rows[0].text
        );
        assert!(rows[1].text.starts_with("OK sync completed"));
    }

    #[test]
    fn a_tiny_terminal_still_gets_a_footer_and_never_overflows() {
        // Arrange
        let screen = planned(4, 4);

        // Act
        let rows = screen.frame(10, 2);

        // Assert
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert!(display_width(&row.text) <= 10, "{}", row.text);
        }
    }

    #[test]
    fn a_pane_sets_its_next_repository_off_from_the_last_one() {
        // Arrange
        let mut screen = planned(1, 2);
        screen.apply(SyncEvent::Started {
            worker: 0,
            name: "first".into(),
        });
        screen.apply(SyncEvent::Line {
            worker: 0,
            name: "first".into(),
            line: "ERROR git pull failed (exit 1)".into(),
        });
        screen.apply(SyncEvent::Finished {
            worker: 0,
            result: result("first", SyncOutcome::PullFailed),
        });
        screen.apply(SyncEvent::Started {
            worker: 0,
            name: "second".into(),
        });
        screen.apply(SyncEvent::Line {
            worker: 0,
            name: "second".into(),
            line: "checking repository".into(),
        });

        // Act
        let rows = screen.frame(60, 8);

        // Assert: the earlier error stays, and a rule names the repository
        // whose lines follow.
        assert!(
            rows[0].text.contains("worker 1  second"),
            "{}",
            rows[0].text
        );
        assert!(rows[1].text.starts_with("ERROR git pull failed"));
        assert_eq!(rows[2].text.trim(), "");
        assert!(
            rows[3].text.starts_with("\u{2500}\u{2500} second"),
            "{}",
            rows[3].text
        );
        assert_eq!(rows[3].color, Some(Color::DarkGrey));
        assert!(rows[4].text.starts_with("checking repository"));
    }
}
