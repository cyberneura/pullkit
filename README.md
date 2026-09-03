# pullkit

Inspect and sync configured Git repositories.

- **CLI** (default): list repo status, sync eligible repos
- **GUI** (`--gui`): Tauri-based graphical interface

## Install

macOS on Apple Silicon, through Homebrew:

```bash
brew install --cask cyberneura/tap/pullkit
```

The cask installs the `pullkit` binary, which is both the terminal interface and, with
`--gui`, the window. The binary is signed with a Developer ID and notarized, so the first
run needs no workaround. Elsewhere, build from source with `cargo build --release`.

## Usage

```bash
# Open the interactive terminal interface
pullkit

# Sync all eligible repos
pullkit sync

# Sync specific repos
pullkit sync --only myapp,another-app

# Launch GUI
pullkit --gui
```

In the terminal interface, use Up/Down or `j`/`k` to move, Space to select repositories,
`a` to select all available repositories, Enter to sync, and `q` or Esc to quit.
When standard input or output is redirected, `pullkit` prints the repository status as a table.

Every list shows the date of the latest local commit, the date of the latest commit on
`origin` (`HEAD` of the remote, fetched with `git fetch origin HEAD`), and the difference
between them in words. Which side is ahead comes from the ancestry of the two commits, and
the dates supply the size of the gap: `up to date`, `2 days behind`, `3 hours ahead`, or
`diverged by 5 days` when neither commit contains the other.

The terminal interface and the GUI show the list immediately with `fetching...` placeholders
and update each row as its fetch completes; the table printed to a pipe waits for every
repository instead. A terminal too narrow to hold everything gives up the path first, then the
remote date, then the local date; the difference is never cut off. Columns are measured in
terminal cells, so a repository named with an emoji or with Japanese lines up with the rest,
and a name cut short at the column edge never leaves half an emoji behind. Eight repositories are fetched at a time, and a `git fetch` that has not
finished within 60 seconds is killed, on Unix together with the helpers it started. Within one
pullkit, the `git fetch` of a list and the `git pull` of a sync never run at the same time in
one repository, even where two entries share a repository because a directory sits inside
another entry's work tree. A repository whose remote cannot be read shows `unavailable`, and
the GUI puts the git error in the row's tooltip.

`GIT_TERMINAL_PROMPT=0` stops git from asking for credentials. Unless `GIT_SSH_COMMAND`,
`GIT_SSH`, or the repository's `core.sshCommand` is already set, ssh is run with
`BatchMode=yes` so that it fails instead of asking as well. A repository that chooses its own ssh command keeps it, but a
prompt from that command still never reaches you: the fetch has no terminal it may read, so it
waits out the 60 second timeout and the row ends up showing `unavailable`.

## Configuration

`~/.config/pullkit/config.yaml`:

```yaml
concurrency: 4
repos:
  - name: myapp
    path: ~/projects/myapp
    build_command: cargo build
  - name: web-frontend
    path: ~/projects/web-frontend
    build_command: npm run build
```

`concurrency` is how many repositories are pulled and built at the same time, from 1 to 10;
it defaults to 4 when left out. `pullkit sync --jobs N` overrides it for one run.

If the configuration file does not exist, pullkit creates it from the sample above and prints
setup help. The same help is printed when the configuration contains no repositories.
Repository paths may use `~` to refer to the current user's home directory.

## Per-repo behavior

A sync works on `concurrency` repositories at once. For each repo:
1. Check for uncommitted changes → skip with warning
2. Check current branch is main/master → skip if not
3. `git pull origin HEAD`, with the same non-interactive settings as the fetch above, and
   killed if it has not finished within 10 minutes
4. If `build_command` is configured, run it
5. Failures in one repository never stop the others

## Watching a sync

On a terminal, the sync takes over the screen with one pane per worker. Each pane shows the
repository its worker is on and the tail of what the pull and the build wrote, line by line as
it arrives. When a pane would have fewer than three log rows, because the terminal is short or
the concurrency high, the panes give way to one merged log with the repository's name in front of
each line. Ctrl-C asks the running commands to stop, so a `git pull` can remove its lock files;
a second Ctrl-C kills them. Once every repository is done, a key press leaves the screen and the
summary is printed to the terminal.

When stdin or stdout is not a terminal, a pipe or a cron job, the lines of every repository
are printed as they arrive, each with the repository's name in front. The commands then stay in
pullkit's own process group, so a Ctrl-C typed at a terminal that pullkit's output is piped
through still reaches them; a job runner that signals only pullkit's pid ends pullkit alone.

The GUI shows the same panes in a grid under the repository list. Quitting the GUI, by closing
the window or with Cmd+Q, asks the running commands to stop, waits three seconds, and kills what
is still running.

If the terminal goes away while the screen is up, or pullkit is told to terminate, the commands
are stopped the same way before pullkit ends.

On the screen and in the GUI, anything pullkit started is stopped when the sync ends, the git
commands that only read included, and also a process a build left running in the background: it
shares the build's process group, and that group is stopped whether the sync was aborted or ran
to the end. A build that means to leave a process behind has to start it in a session of its
own, with `setsid` or the like. When stdin or stdout is not a terminal the pulls and builds share
pullkit's own group, and a process a build leaves behind there is left alone, as any other
command's would be.

Two entries that share one repository, because one directory sits inside the other's work
tree, are never worked on at the same time: a sync holds the repository from its status check
to the end of its build.

On Windows there are no process groups to signal; a stop takes the command's process tree
down with `taskkill` instead, forcibly, and a process a build left behind after it ended is out
of reach. Windows is not tested.

## Releasing

A release follows the version in `Cargo.toml` on `main`: change it there and push, and
`.github/workflows/release.yml` builds, signs, notarizes, and publishes it. Nothing else starts
one, and a version that is already released is left alone however often `main` is pushed.

```bash
scripts/release.sh          # minor bump
scripts/release.sh patch
scripts/release.sh major
```

The script bumps `Cargo.toml`, `src-tauri/tauri.conf.json`, and the lockfile together, commits
`Release vX.Y.Z`, and pushes `main`. The workflow runs the tests, builds `pullkit` for
`aarch64-apple-darwin`, signs and notarizes it, and publishes `pullkit-vX.Y.Z-aarch64-apple-darwin.tar.gz`
on a GitHub Release with its checksum in the notes. If a run fails, fix the cause and push: the
version is not released yet, so the next run carries on. The signing secrets come from
`deploy-github-secret-apple-building.sh` in home-files.

The Homebrew cask in [cyberneura/homebrew-tap](https://github.com/cyberneura/homebrew-tap)
(`Casks/pullkit.rb`) is updated by the tap itself, which looks at the latest release every hour.
This repository never pushes to the tap.
