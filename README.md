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
3. `git pull origin HEAD`
4. If `build_command` is configured, run it
5. Continue to next repo regardless of individual failures
