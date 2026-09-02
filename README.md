# pullkit

Inspect and sync configured Git repositories.

- **CLI** (default): list repo status, sync eligible repos
- **GUI** (`--gui`): Tauri-based graphical interface

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
repos:
  - name: myapp
    path: ~/projects/myapp
    build_command: cargo build
  - name: web-frontend
    path: ~/projects/web-frontend
    build_command: npm run build
```

If the configuration file does not exist, pullkit creates it from the sample above and prints
setup help. The same help is printed when the configuration contains no repositories.
Repository paths may use `~` to refer to the current user's home directory.

## Per-repo behavior

For each repo during sync:
1. Check for uncommitted changes → skip with warning
2. Check current branch is main/master → skip if not
3. `git pull origin HEAD`, with the same non-interactive settings as the fetch above, and
   killed if it has not finished within 10 minutes
4. If `build_command` is configured, run it
5. Continue to next repo regardless of individual failures
