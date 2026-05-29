# Winland

Winland is a Windows-native tiling window manager layer written in Rust. It runs on top of the normal Windows desktop, DWM, and Explorer instead of replacing the compositor. It discovers ordinary top-level windows through documented Win32 APIs, filters out risky desktop infrastructure, and applies tiling, fake workspaces, hotkeys, rules, IPC, and optional border feedback.

The project is usable as an early WM prototype, but it is still conservative by design. Window correctness and recoverability matter more than visual polish.

## Current Status

Implemented today:

- Window discovery, monitor discovery, and conservative filtering.
- Startup retile, dynamic retile from WinEvent hooks, and one-shot tiling.
- Layouts: `master-stack`, `dwindle`, `vertical-stack`, and `horizontal-stack`.
- Gaps, layout border reservation, master ratio, per-monitor layout overrides, and per-workspace layout overrides.
- Fake workspaces with per-monitor active workspace state.
- Focus, swap, retile, toggle-float, workspace, monitor, reload, quit, and launch hotkey commands.
- Normal `RegisterHotKey` mode and opt-in advanced low-level keyboard interception.
- Modifier-drag window movement through a low-level mouse hook.
- Window rules for manage/ignore, float, workspace, always-on-workspace, layout validation, and game/fullscreen modes.
- Constraint-aware tiling that learns fixed/minimum window sizes and floats overflow windows when needed.
- Optional lightweight Win32 border overlay feedback.
- Game mode hardening for fullscreen windows and configured game executables.
- Local named-pipe IPC for `state` and `reload-config`.
- CLI diagnostics for windows, monitors, config validation, game-mode diagnosis, IPC state, reload, and one-shot tiling.
- Experimental VM-only shell replacement commands. This is not the normal startup path.

Not implemented as normal features:

- DWM replacement, shell replacement by default, wallpaper management, a bar, animations, blur, shadows, DirectComposition effects, or graphics API hooks.
- Automatic config file watching. Reload is explicit.
- A stable scripting IPC beyond the current state and reload commands.

## Build

Winland targets Windows. Some pure crates can compile elsewhere, but `winland-win32`, the daemon, and the CLI's desktop commands are Windows-only at runtime.

```powershell
cargo build --workspace
```

Recommended checks before submitting behavior changes:

```powershell
cargo fmt
cargo clippy --workspace --all-targets
cargo test --workspace
```

## Tests

The default regression suite is designed to be safe on a normal developer desktop. It focuses on pure layout, workspace, rule, config, IPC, diagnostics, game-mode, and border decision logic; it does not move real windows, require admin privileges, or require games/anti-cheat software.

Run the full local suite from the workspace root:

```powershell
cargo fmt
cargo clippy --workspace --all-targets
cargo test --workspace
```

Desktop-mutating smoke tests should remain opt-in and clearly named if they are added later.

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

## Minimal Config

Winland runs with built-in defaults when no config file is found. A minimal file can be as small as:

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

The repository's [winland.toml](winland.toml) is an annotated example, not a promise that every future field already exists. Validate edits with:

```powershell
cargo run -p winland-cli -- config validate --path winland.toml
```

## Common Commands

| Command | Purpose |
| --- | --- |
| `winland windows` | List discovered windows and why each is managed or skipped. |
| `winland windows --manageable-only` | Show only windows passing the base filter. |
| `winland monitors` | List Winland monitor IDs, rectangles, and work areas. |
| `winland diagnose-window` | Explain manageability, fullscreen detection, and game mode for the foreground window. |
| `winland diagnose-window --hwnd 0x123456` | Diagnose a specific HWND. |
| `winland tile-once` | Tile manageable windows on the primary monitor once. |
| `winland state` | Query the running daemon through IPC. |
| `winland state --json` | Print daemon state JSON. |
| `winland reload-config` | Ask the running daemon to reload config from disk. |
| `winland config validate --path winland.toml` | Parse and validate a config file. |

During local development, use `cargo run -p winland-cli -- <command>`.

## Safety Note For Games

Winland does not inject into games, hook DirectX/Vulkan/OpenGL, patch DWM, inspect game memory, or install kernel drivers. Game mode is based on documented window metadata: configured executable names, window rules, and fullscreen/borderless fullscreen geometry. When active, Winland can pause layout, clear borders, and bypass low-level input hook handling according to config.

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
