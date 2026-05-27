# Winland

Winland is an early Windows-native tiling window manager experiment written in Rust. It is inspired by Hyprland, i3, and komorebi, but it does not replace DWM. Instead, it aims to run as a careful layer over the normal Windows desktop, arranging ordinary application windows through documented Win32 APIs.

The project is intentionally starting with the boring hard parts: window discovery, safe tiling, event handling, keyboard control, layouts, fake workspaces, rules, IPC, and diagnostics. Borders, animations, and other visual feedback come later, after the tiling core is stable.

## Current Shape

- `winland-core`: platform-independent layout and window state logic.
- `winland-win32`: documented Win32 integration through the `windows` crate.
- `winland-daemon`: event-driven background process with local IPC foundations.
- `winland-shell`: experimental user-shell entrypoint that starts the daemon without Explorer.
- `winland-cli`: command-line diagnostics and local daemon control.
- `winland-config`: TOML configuration parsing, defaults, validation, and config file discovery.

## Principles

- Rust only.
- Windows native.
- No Electron.
- No DWM replacement.
- No permanent render loop.
- Event-driven behavior before polling.
- Unsafe Win32 code stays isolated in `winland-win32`.
- Human-editable config for hotkeys, layouts, workspaces, behavior, and window rules once the core is ready for it.

## Status

This repo is in the planning and early implementation stage. See [ROADMAP.md](ROADMAP.md) for the agile backlog and [AGENTS.md](AGENTS.md) for project rules.

## Experimental Shell Replacement

The shell replacement prototype is VM-only, opt-in, and reversible. It uses `winland-shell` as a small user-shell entrypoint and can optionally start `winland-daemon` elevated through a scheduled task so elevated windows can be tiled. See [docs/experimental-shell-replacement.md](docs/experimental-shell-replacement.md) before installing it.

## Current IPC Slice

The first IPC control slice exposes `winland state`, backed by a versioned local named-pipe protocol. It reports daemon health and high-level window/workspace counts in human-readable form, with `winland state --json` available for scripts. Mutating IPC commands such as focus, swap, retile, reload, and quit are intentionally still upcoming.
