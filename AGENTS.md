# pullkit

A Rust tool that lists the state of a configured set of Git repositories and syncs them together.
It provides a CLI (TUI) and a Tauri GUI in a single binary.

## Layout

| Path | Contents |
|---|---|
| `crates/pullkit-core` | Loading config, inspecting repository state, fetching commit dates, syncing. Independent of any UI |
| `src-tauri/src/main.rs` | CLI arguments, TUI (crossterm), table output, Tauri commands |
| `src-tauri/src/sync_screen.rs` | Full-screen display during sync. Shows the tail of the log in a pane per worker |
| `src-tauri/src/licenses.rs` | `--license` and the Third-Party Licenses menu item. Embeds LICENSE and THIRD-PARTY-NOTICES.txt |
| `ui/` | GUI frontend. Plain HTML / CSS / JS with no build step |

The config file is `~/.config/pullkit/config.yaml`. If it does not exist, it is created from `config.example.yaml`.

## Development commands

```bash
cargo run                    # TUI
cargo run -- --gui           # GUI
cargo run -- sync            # sync
cargo test --workspace       # unit tests
cargo clippy --workspace --all-targets
cargo fmt --all
node --check ui/main.js      # JS syntax check (stands in for a build step, since there is none)
scripts/generate-third-party-notices.sh  # regenerate THIRD-PARTY-NOTICES.txt (requires cargo-about)
```

`.j-menu.yaml` can launch the TUI / GUI / release build / release (version bump + push).

## License notices

- `LICENSE` (MIT, Cyberneura) and `THIRD-PARTY-NOTICES.txt` are embedded into the binary by
  `src-tauri/src/licenses.rs` with `include_str!`. The distributed artifact is a bare binary, so no
  LICENSE stays on the user's machine; therefore both `pullkit --license` and the GUI's app menu
  (Third-Party Licenses, right below About) show our own MIT text first. Both files are also
  bundled in the release tar.gz.
- **When adding or bumping dependencies, re-run `scripts/generate-third-party-notices.sh` and
  commit.** `cargo test` checks "are direct dependencies listed at the version in Cargo.lock" and
  "does each listed version exist in Cargo.lock", so it fails if you forget. Dependabot PRs are
  regenerated on the same branch too.
- Generation uses cargo-about (`cargo install cargo-about --locked --features cli`). The targets
  are `targets` in `src-tauri/about.toml` (only the distributed `aarch64-apple-darwin`). If you add
  a distribution target, add it to `targets` too. If it fails on a license not in `accepted`, do not
  widen the list on your own (a GPL-family license would change the distribution terms).
- To keep ourselves out of the list, both crates have `publish = false`, and about.toml has
  `[private] ignore`.
- The GUI menu exists only on macOS (Tauri creates a default menu only on macOS). `app_menu`
  inserts the item as the 2nd entry (after About) of the default menu's app menu. It is not wrapped
  in cfg so that type checking also passes on Linux; only the call site is limited to macOS.
- The Third-Party Licenses window (`ui/licenses.html`, label `licenses`) closes together with the
  main window when the main window is destroyed. If it remained, the app would not exit and the
  cleanup in `RunEvent::Exit` would not run either.

## Releases

Changing the version in the (workspace) `Cargo.toml` on `main` and pushing makes a release
(`.github/workflows/release.yml`). The `plan` job asks the releases API "is a Release for that
version already published", and only on 404 proceeds to test → build → release. The decision is
based on state, not on a diff, so the result does not change with squash / rebase / direct push,
and a failed run continues from where it stopped once you fix the cause and push.

- Numbering and pushing are done by `scripts/release.sh [patch|minor|major]` (default minor). It
  aligns `Cargo.toml`, `src-tauri/tauri.conf.json`, and `Cargo.lock` and pushes `Release vX.Y.Z`
  directly to main.
- build is macOS arm64 only. Tauri is not split out by feature, so there is no Linux leg (it would
  require webkit2gtk). The bare binary is signed with Developer ID and notarized with notarytool
  (`--options runtime` is required; stapling is not possible), a tar.gz containing
  `pullkit-vX.Y.Z-aarch64-apple-darwin/pullkit` is attached to a draft Release, and it is published
  after counting the assets.
- The signing Secrets (APPLE_*) are distributed from 1Password by adding `cyberneura/pullkit` to
  `repos` in `~/home-files/sh/github-secret/deploy-github-secret-apple-building.sh` and running it.
  If they are missing, build fails at the first step (it does not silently ship unsigned).
- Homebrew is `Casks/pullkit.rb` in `cyberneura/homebrew-tap`. The tap's `scripts/update.py` checks
  the latest release hourly and rewrites version / url / sha256, so do not push to the tap from this
  repository. A new version shows up in `brew upgrade` delayed by up to the tap's cron interval.
- `test.yml` also runs on PRs (fmt / clippy -D warnings / test / `node --check`). release calls the
  same definition via `workflow_call`.

## How to verify

**Always run `cargo build` before a Rust E2E.** `cargo test` / `cargo clippy` do not relink
`target/debug/pullkit`, so if you touch the real binary after a fix without building, you see the
old behavior.

**The `pullkit` on PATH is the Homebrew cask, separate from your local build.** It only contains the
published version, so do not use the result of typing `pullkit` to check local changes. Verify by
specifying the path, e.g. `./target/debug/pullkit`.

**The compiler is pinned by `rust-toolchain.toml`.** rustup-managed cargo reads it, but cargo from
Homebrew's `rust` formula ignores it and builds with its own version. If `cargo --version` differs
from the version written there, clippy passing locally does not mean it will pass in CI.

### TUI

The Tauri GUI window cannot be checked in a browser, but the TUI's real screen can be captured via
a pty. Launch it with `pty.fork()` and assemble the lines by interpreting escape sequences, and you
can verify asynchronously changing frames frame by frame.

### GUI

It is a native window, so browser automation does not work. With `orca computer` (the
`/computer-use` skill) you can click checkboxes / buttons via the accessibility tree, and it also
returns screenshots. Clicks do not work unless you first bring it to the front with
`get-app-state --restore-window`. Launching with a replaced `HOME` lets you try fixtures without
polluting the user's settings. Screenshots cannot be taken while the screen is locked; in that case,
state explicitly that they could not be taken.

**After editing `ui/`, run `cargo build` again.** Tauri embeds the frontend into the binary at build
time, so even a debug build launches with the old JS / CSS.

For rendering logic alone, you can verify by loading `ui/main.js` onto DOM stubs with Node's `vm`
(stub `window.__TAURI__.core.invoke` and `event.listen`). With jsdom you can run it against a real
DOM, including `click()`. **A `let` loaded via `window.eval` does not become a page global**, so read
values such as `inspectionToken` back from the arguments passed to `invoke("inspect_all_commits")`.

**Layout such as column alignment cannot be measured in jsdom** (it has no layout engine).
Dump the DOM assembled in jsdom to static HTML, start `http.server` on the LAN IP rather than
`127.0.0.1`, have the remote Chrome (agent-browser) load it, and measure the left edges of the
columns with `getBoundingClientRect()`. **Always shut the server down when you are done checking.**

### Paths involving remotes

Timeouts, cleanup at exit, and queues exceeding the worker count can be reproduced by preparing
several temporary repositories with an unreachable remote.

```bash
git remote add origin "ssh://git@10.255.255.1:22/nope.git"
```

## Design commitments

- **ahead / behind is determined by ancestry.** Commit dates can be set arbitrarily, so judging by
  which timestamp is larger gets reversed by rebase, amend, or clock skew. Use
  `git merge-base --is-ancestor`, and use dates only for the magnitude of the difference.
- **Never run git concurrently against the same `.git`.** `repo_lock` hands out locks keyed by the
  git directory (`git rev-parse --absolute-git-dir`). The list's `git fetch` and the fetch included
  in sync's `git pull` share `FETCH_HEAD`, and the inspection side reads it back right afterwards.
  The exclusion is taken where git is executed, not in the UI. Waiting on the UI side is UX so that
  "the user is not kept waiting silently." Linked worktrees have a `FETCH_HEAD` per worktree, so
  keep them as separate keys.
- **git that touches remotes must not prompt.** The GUI has no terminal, and in the TUI the prompt
  gets buried under redraws. Pass `GIT_TERMINAL_PROMPT=0`, and add `ssh -o BatchMode=yes` only when
  none of `GIT_SSH_COMMAND`, `GIT_SSH`, or `core.sshCommand` is set.
- **git / build that the terminal cannot deliver Ctrl-C to run in their own process group.** This
  applies to the list's fetch, the GUI's sync, and the sync below the TUI's pane screen. In a
  raw-mode terminal Ctrl-C becomes a key input rather than a signal, so the screen sends
  `interrupt_running_commands` (SIGINT), and on the second press `terminate_running_commands`
  (SIGKILL). When the screen exits for its own reasons, when the GUI quits (`RunEvent::Exit`;
  closing the window and Cmd+Q both go through here), and when leaving the list,
  `stop_running_commands` (SIGINT → grace period → SIGKILL) is used. Only `pullkit sync` with stdin
  or stdout not being a terminal uses the shared group, so a Ctrl-C typed in the terminal at the end
  of the pipe reaches it directly (with a sender that signals only pullkit's pid, as cron does, the
  children do not follow). The caller specifies this with `Isolation`. Windows has no process
  groups, so `signal_process_group` brings down the process tree with `taskkill /T /F` regardless of
  SIGINT / SIGKILL, and it does not reach processes left behind after build has finished
  (unverified).
- **When the terminal disappears (SIGHUP) and on SIGTERM, catch them ourselves, run
  `stop_running_commands`, and then exit.** The pull / build below the screen and the list's fetch
  are in their own process group, so the terminal's hangup does not reach them, and if only pullkit
  dies first by the default action, they are all left running. `stop_commands_on_hangup` (a
  signal-hook thread) is started at the top of `run` for both CLI and GUI. SIGINT is caught too,
  because there are windows where our own group's commands run in cooked mode, such as "Waiting for
  the remote inspection" after Enter (it is not generated on raw-mode screens). After stopping, kill
  ourselves with the same signal rather than `exit()` (`emulate_default_handler`). bash decides
  whether to stop a script based on whether the child died of SIGINT, so reverting to `exit(130)`
  would make scripts that call the pipe path not stop on Ctrl-C. The pull / build on the pipe path
  are in the shared group and not registered, so they are not targets of stop. A Ctrl-C / hangup that
  the terminal sends to the foreground group reaches the children too, but with a signal sent only to
  pullkit's pid the children remain (as before).
- **Keep interruption (`stop_running_commands`) and cleanup (`stop_leftover_commands`) separate.**
  The former sets `ABANDONED`, so that process can no longer start a run. Stopping processes that
  build left behind after a sync finishes is the latter, and the GUI can do the next sync with the
  window still open. The pull / build on the pipe path are in the shared group and not registered,
  so anything left over there cannot be stopped (the README says so too).
- **Once `ABANDONED` is set, never reset it.** It assumes interruption always ends in process exit;
  resetting it at the start of a run would let a worker thread that starts late cancel out the
  interruption just before. If it ever becomes necessary to start a run after an interruption in the
  same process, reset it on the thread that issues the interruption, before starting the run.
- **Keep a process group in `RUNNING_COMMANDS` while it still has members, and remove groups that
  have become empty before sending a signal.** If the group still has members after the command
  finishes, move it to `LEFTOVER_GROUPS` and stop it with `stop_leftover_commands` when the sync
  finishes normally (what pullkit started ends with pullkit). Running commands and leftovers are kept
  in separate lists so that commands started by another run, e.g. when the GUI page is reloaded, are
  not mistaken for leftovers and killed. The id of an emptied group can be reused by another process,
  so check with `killpg(pgid, 0)` before sending, and remove emptied leftovers from the list each
  time a command finishes to shorten the window. The window between the check and the send remains
  as long as it is pid-based (an accepted trade-off on the assumption that pidfd / kqueue are not
  available). Signals are sent via libc, not the `kill` binary, and the list's Mutex is released
  before sending.
- **Register read-only git with `spawn_tracked` too, and put an upper bound (`READ_TIMEOUT`) on it.**
  If a `git status` stuck on a hook or fsmonitor keeps holding `repo_lock`, the whole sync never
  returns. From spawn to registration it is counted in `SPAWNING`, and stop waits until that reaches 0.
- **Pipe line length (`MAX_LINE_BYTES`) and channels (`PIPE_QUEUE` / `EVENT_QUEUE`) are bounded.** So
  that memory does not grow with output that has no newlines or a UI whose rendering cannot keep up,
  when they overflow the child process's write is made to wait.
- **sync holds `repo_lock` from the status check until the end of build.** When 2 entries inside the
  same work tree run in parallel, this keeps one's pull from rewriting the tree while the other is
  building.
- **sync logs flow through `SyncEvent`, and lines do not include the repository name.**
  `sync_repos` runs on as many threads as workers, and events are handed to `on_event` on the
  calling thread (so both TUI rendering and Tauri emit need only 1 thread). The output of pull and
  build is streamed line by line. Emitting it all at once would leave the pane empty for the whole
  `cargo build`.
- **Set the Dock icon ourselves at launch.** The distributed artifact is a bare binary, not a
  `.app`, so macOS has no Info.plist to read, and if left alone it gets the generic executable icon
  (`bundleIdentifier` of `NSRunningApplication` is `nil`). `src-tauri/icons/icon.icns` is embedded
  with `include_bytes!` and passed to `NSApplication.setApplicationIconImage` in setup.
  `bundle.icon` only takes effect when building a `.app`, so writing it there is pointless.
  This cannot be checked with `NSRunningApplication.icon` (it returns the bundle-derived value).
- **Build the GUI list with `<table>`.** In the old implementation that put `min-width` on flex
  rows, the wide `Missing` status and long labels such as `diverged by 12 months` pushed the
  following cells out, and columns misaligned row by row (CYBERNEURA-DEV-687; up to 32px measured).
  Columns align because of `<table>`, not because of the `table-layout` value. **Leave column widths
  at `table-layout: auto` and give only upper and lower bounds in CSS** (changed from `fixed` in the
  same task). **Do not hard-code fixed values** — expanding with the content is the intended
  behavior, and the lower bounds exist only to stop fluctuation.
  - Status / Local / Remote / Difference have `min-width`. The list's fetch returns row by row, so
    without a lower bound the column widths are recalculated every time a result arrives and the
    table jitters. The value is the width of the longest label that column can take.
  - The Repository column has `width: 100%` + `max-width: 0` + `min-width`. The first 2 make it a
    column that "takes the remaining width but does not widen the table with a long path"
    (overflowing paths get an ellipsis), and below `min-width` the column does not shrink and
    `.repo-list` scrolls horizontally instead.
  - Only the Status column also has `max-width`. Branch names have no length limit, and without an
    upper bound a single repository would make the table wider than the window.
  - `.status` / `.difference` are `display: block` + `white-space: nowrap`. With
    `display: inline-block; width: 100%`, a percentage is not a width the auto layout can measure,
    and a wrappable label reports only the width of its longest word, so the column never learns the
    width of its own content (in the `fixed` days, branch names wrapped onto 4 lines inside a 116px
    column).
  The behavior where clicking anywhere on a row toggles its checkbox is kept by delegating clicks
  on `#repos`, since `<label>` can no longer be used.
- **The GUI's sync re-reads the selection after any wait.** Before `sync_selected` there is a window
  of "wait until the list's fetch finishes," during which the selection can grow via
  `select_outdated_by_default` or manual actions. Running with names read before the wait would
  leave repositories that the screen shows as checked unsynced. The TUI side is the opposite: it
  runs with the selection at the moment Enter is pressed (the screen is already collapsed, and
  selections added after that are not visible to the user).
  Both are aligned on "run with the selection the user last saw."
- **Do not read state back from the `difference` wording.** `RepoCommits.relation`
  (`same` / `behind` / `ahead` / `diverged`) is the source of truth, and `difference` is the
  human-facing text. Both color coding and "Select all outdated" look at relation.
- **When counting columns, use terminal cells, not character counts.** Use `display_width` /
  `truncate_to_width` / `pad_to_width`, and do not handle widths with `chars().count()` or `{:<20}`.
  Japanese text and emoji are rendered in 2 cells, so counting characters makes lines wrap and break
  the next line. Widths are counted per grapheme cluster. Emoji composed with skin tones or ZWJ are
  2 cells even though they are multiple scalars, so counting per scalar does not match. Truncate at
  cluster boundaries and never split an emoji in the middle.

## Review by AI agents

Changes where async processing, subprocesses, and multiple UIs intersect do not converge in one
round of review. Keep iterating until both Codex and code-reviewer return zero Critical findings.
They are strong in different areas: Codex in concurrent execution order and process lifetimes,
code-reviewer in backing claims with actual measurements.

## macOS app icon

Run `bash scripts/generate-macos-icons.sh` with Xcode on macOS. Fixed artwork,
layout measurements, size-specific sources and the verification command are in
`resources/app-icons/README.md`. Small @1x backgrounds reach the canvas edges;
the glyph fits inside the central 80%. Larger backgrounds have 10% outer margins.
Do not replace the generated ICNS with `tauri icon` / `iconutil`: preserve the
ARGB small entries and the matching Assets.car / CFBundleIconName registration.
Native rendering is verified by `.github/workflows/macos-icons.yml`.

The distributed CLI is a bare executable: its GUI embeds ICNS and sets
NSApplication’s icon at startup. Bundle registration applies only to optional
.app builds; it cannot give a bare binary a System Settings bundle icon.
