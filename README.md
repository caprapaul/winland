# Winland

Winland is a Windows-native tiling window manager layer written in Rust. It works with the normal Windows desktop, DWM, and Explorer instead of replacing the compositor. Winland discovers top-level windows through documented Win32 APIs, applies conservative manageability rules, and adds tiling, fake workspaces, keyboard control, window rules, IPC, and optional border feedback.

The project is an early prototype focused on correctness, safety, and recoverability. The tiling core comes first; visual polish is intentionally limited until core behavior is dependable.

## What Works

- Conservative top-level window and monitor discovery.
- Startup retile, one-shot tiling, and dynamic retile through WinEvent hooks.
- Layouts: `master-stack`, `dwindle`, `vertical-stack`, and `horizontal-stack`.
- Gaps, border reservation, master ratio, per-monitor layout overrides, and per-workspace layout overrides.
- Fake workspaces with active workspace state per monitor.
- Focus, swap, retile, toggle-float, workspace, monitor, reload, quit, and launch hotkey commands.
- `RegisterHotKey` mode, plus opt-in advanced low-level keyboard interception.
- Modifier-drag window movement through a low-level mouse hook.
- Window rules for manage/ignore, float/tile, workspace assignment, always-on-workspace, and game/fullscreen behavior.
- Constraint-aware tiling that learns fixed or minimum window sizes and promotes overflow windows to floating when needed.
- Optional lightweight Win32 border overlay feedback.
- Game-mode hardening for fullscreen windows and configured game executables.
- Local named-pipe IPC for state queries and explicit config reload.
- CLI diagnostics for windows, monitors, config validation, game-mode diagnosis, daemon state, config reload, and one-shot tiling.

## Current Limitations

Winland does not replace DWM, replace Explorer by default, manage wallpaper, provide a bar, or implement blur, shadows, animations, DirectComposition effects, graphics API hooks, or custom compositing.

Configuration reload is explicit; automatic file watching is not implemented. IPC is intentionally small and currently covers state queries and reload. Experimental shell replacement helpers exist for VM testing only and are not the normal startup path.

## Build

Winland targets Windows. Some pure crates can compile elsewhere, but `winland-win32`, the daemon, and desktop-facing CLI commands are Windows runtime features.

```powershell
cargo build --workspace
```

Recommended checks before submitting behavior changes:

```powershell
cargo fmt
cargo clippy --workspace --all-targets
cargo test --workspace
```

## Quick Start

From the workspace root:

```powershell
cargo run -p winland-cli -- config validate --path winland.toml
cargo run -p winland-cli -- windows
cargo run -p winland-cli -- monitors
cargo run -p winland-daemon
```

In another terminal:

```powershell
cargo run -p winland-cli -- state
cargo run -p winland-cli -- state --json
cargo run -p winland-cli -- reload-config
```

For a one-shot layout without starting the daemon:

```powershell
cargo run -p winland-cli -- tile-once
```

## Configuration

Winland runs with built-in defaults when no config file is found. A minimal config can be as small as:

```toml
[layout]
default = "dwindle"
gap = 8

[workspaces]
count = 9

[hotkeys]
mode = "advanced-interception"
bindings = [
  { keys = "Win+Left", command = "focus-left", override_app = true },
  { keys = "Win+Right", command = "focus-right", override_app = true },
  { keys = "Win+R", command = "retile", override_app = true },
]
```

The repository's [winland.toml](winland.toml) is an annotated example. Validate edits with:

```powershell
cargo run -p winland-cli -- config validate --path winland.toml
```

## CLI Commands

During local development, use `cargo run -p winland-cli -- <command>`.

| Command | Purpose |
| --- | --- |
| `winland windows` | List discovered windows and why each is managed or skipped. |
| `winland windows --manageable-only` | Show only windows passing the base filter. |
| `winland monitors` | List monitor IDs, rectangles, and work areas. |
| `winland diagnose-window` | Explain manageability, fullscreen detection, and game mode for the foreground window. |
| `winland diagnose-window --hwnd 0x123456` | Diagnose a specific HWND. |
| `winland tile-once` | Tile manageable windows on the primary monitor once. |
| `winland state` | Query the running daemon through IPC. |
| `winland state --json` | Print daemon state JSON. |
| `winland reload-config` | Ask the running daemon to reload config from disk. |
| `winland config validate --path winland.toml` | Parse and validate a config file. |

## Testing

The default regression suite is designed to be safe on a normal developer desktop. It focuses on pure layout, workspace, rule, config, IPC, diagnostics, game-mode, and border decision logic. It does not move real windows, require admin privileges, or require games or anti-cheat software.

```powershell
cargo fmt
cargo clippy --workspace --all-targets
cargo test --workspace
```

Desktop-mutating smoke tests should remain opt-in and clearly named if they are added later.

## Safety Notes

Winland uses documented Windows APIs such as window enumeration, window placement, monitor APIs, event hooks, hotkey registration, and local named pipes. It does not inject into games, hook DirectX/Vulkan/OpenGL, patch DWM, inspect game memory, or install kernel drivers.

Game mode is based on configured executable names, window rules, and fullscreen or borderless-fullscreen geometry. When active, Winland can pause layout, clear borders, and bypass low-level input hook handling according to config.

See [docs/GAME_MODE.md](docs/GAME_MODE.md) before using advanced interception or borders around games and anti-cheat software.

## Documentation

- [Features](docs/FEATURES.md)
- [Configuration](docs/CONFIG.md)
- [CLI](docs/CLI.md)
- [Window Rules](docs/RULES.md)
- [Troubleshooting](docs/TROUBLESHOOTING.md)
- [Architecture](docs/ARCHITECTURE.md)
- [Game Mode](docs/GAME_MODE.md)
- [Experimental Shell Replacement](docs/experimental-shell-replacement.md)
