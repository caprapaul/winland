# CLI

The CLI binary is named `winland`. During local development from the workspace root, use:

```powershell
cargo run -p winland-cli -- <command>
```

Most desktop commands require Windows because they call `winland-win32`.

## Commands

| Command | IPC | Purpose |
| --- | --- | --- |
| `state [--json]` | Yes | Query the running daemon. |
| `reload-config [--json]` | Yes | Ask the running daemon to reload config from disk. |
| `windows [--manageable-only]` | No | Enumerate top-level windows and explain the conservative filter. |
| `monitors` | No | List monitor IDs, rectangles, and work areas. |
| `diagnose-window [--hwnd <HWND>]` | No | Explain manageability, fullscreen detection, and game mode for one window. |
| `tile-once` | No | Tile manageable windows on the primary monitor once. |
| `config validate [--path <FILE>]` | No | Parse and validate config. |
| `shell ...` | No IPC | Experimental VM-only shell replacement helpers. |

If the daemon is not running, IPC commands print:

```text
Winland daemon is not running. Start winland-daemon before using IPC commands.
```

and exit with code `2`.

## State

```powershell
cargo run -p winland-cli -- state
cargo run -p winland-cli -- state --json
```

Human output includes config version/path, total/manageable/floating counts, active workspace, foreground HWND, lightweight performance counters, monitor workspace state, and window participation.

JSON output matches the current IPC snapshot shape:

```json
{
  "config_path": "F:\\Projects\\winland\\winland.toml",
  "config_version": 1,
  "config_loaded_at_unix_ms": 1234567890,
  "total_windows": 3,
  "manageable_windows": 2,
  "floating_windows": 1,
  "temporary_floating_windows": 0,
  "active_workspace": 1,
  "foreground_window": 51966,
  "performance": {
    "relayout_count": 12,
    "skipped_relayout_count": 7,
    "last_relayout_duration_ms": 2,
    "last_relayout_move_count": 3,
    "managed_window_count": 2,
    "border_window_count": 2,
    "game_mode_active": false,
    "config_reload_count": 1
  },
  "monitors": [
    { "monitor_id": 1, "workspace_id": 1, "focused": true }
  ],
  "windows": [
    {
      "handle": 51966,
      "title": "Editor",
      "monitor_id": 1,
      "workspace_id": 1,
      "focused": true,
      "participation": "tiled",
      "constrained": false,
      "visible_on_active_workspace": true
    }
  ]
}
```

`participation` is normally `tiled`, `floating`, or `temporary-floating`. The IPC schema still accepts `overflow-floating` for compatibility, but overflow promotion now reports as normal `floating`.

## Reload Config

```powershell
cargo run -p winland-cli -- reload-config
cargo run -p winland-cli -- reload-config --json
```

Reload reads the same discovery paths as daemon startup. It validates the new config, rebuilds runtime config, replaces hotkeys and modifier-drag registration, reapplies rules, updates workspace visibility, retails if allowed, and syncs borders.

JSON output includes:

```json
{
  "config_path": "F:\\Projects\\winland\\winland.toml",
  "config_version": 2,
  "reloaded_at_unix_ms": 1234567899,
  "changed_sections": ["hotkeys", "layout"],
  "state": {}
}
```

`state` is the same shape as `state --json`.

## Windows

```powershell
cargo run -p winland-cli -- windows
cargo run -p winland-cli -- windows --manageable-only
```

The table shows HWND, manage/skip status, skip reason, title, class, PID, executable path, rect, styles, visibility, minimized/cloaked state, owner state, and tool-window state.

Common skip reasons include `not visible`, `minimized`, `DWM cloaked`, `empty title`, `owned window`, `tool window`, `empty rectangle`, and `shell window class`.

## Monitors

```powershell
cargo run -p winland-cli -- monitors
```

Use the printed monitor IDs in config overrides and monitor hotkey commands:

```toml
[layout.per_monitor]
"0x10001" = { layout = "vertical-stack", gap = 6 }
```

## Diagnose Window

```powershell
cargo run -p winland-cli -- diagnose-window
cargo run -p winland-cli -- diagnose-window --hwnd 0x123456
```

Without `--hwnd`, Winland diagnoses the foreground window. Output includes:

- manageability and reason
- monitor ownership
- fullscreen detection and area
- game-mode active/inactive state
- game-mode reason
- matched executable/rule
- layout pause scope
- border, animation, and hook pause status

## Tile Once

```powershell
cargo run -p winland-cli -- tile-once
```

This command loads config, enumerates windows and monitors, selects manageable windows whose centers are on the primary monitor, computes the configured layout for workspace `1`, and calls `SetWindowPos` for each assignment. It does not start the daemon or maintain state after the command exits.

## Config Validate

```powershell
cargo run -p winland-cli -- config validate --path winland.toml
cargo run -p winland-cli -- config validate
```

With no path, validation uses discovery paths or built-in defaults. Success output summarizes the effective base layout and game-mode counts.

## Experimental Shell Commands

These commands exist, but they are not normal Winland startup. Use them only in a VM with a checkpoint. Details are in [experimental-shell-replacement.md](experimental-shell-replacement.md).

| Command | Purpose |
| --- | --- |
| `shell status` | Inspect per-user shell registry state. |
| `shell test [--elevated-daemon]` | Launch one shell session without registry changes. |
| `shell install --experimental [--elevated-daemon]` | Persist Winland as the per-user shell. |
| `shell uninstall --experimental` | Restore previous per-user shell value. |
| `shell recover --experimental` | Recovery alias for uninstall; also removes elevated daemon task if present. |
| `shell explorer` | Launch `explorer.exe`. |
| `shell install-elevated-task --experimental` | Create/update the elevated daemon scheduled task. |
| `shell uninstall-elevated-task --experimental` | Delete the elevated daemon scheduled task. |
| `shell elevated-task-status` | Report whether the elevated task exists. |
