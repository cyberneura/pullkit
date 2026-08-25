# pullkit

Inspect and sync configured Git repositories.

- **CLI** (default): list repo status, sync eligible repos
- **GUI** (`--gui`): Tauri-based graphical interface

## Usage

```bash
# List repo status
pullkit

# Sync all eligible repos
pullkit sync

# Sync specific repos
pullkit sync --only myapp,another-app

# Launch GUI
pullkit --gui
```

## Configuration

`~/.config/pullkit/config.yaml`:

```yaml
repos:
  - name: myapp
    path: /Users/you/projects/myapp
    build_command: cargo build
  - name: web-frontend
    path: /Users/you/projects/web-frontend
    build_command: npm run build
```

## Per-repo behavior

For each repo during sync:
1. Check for uncommitted changes → skip with warning
2. Check current branch is main/master → skip if not
3. `git pull origin HEAD`
4. If `build_command` is configured, run it
5. Continue to next repo regardless of individual failures