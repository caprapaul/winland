# Winland Agent Guide

## Project Goal

Winland is a Windows-native tiling window manager layer written in Rust. It is inspired by Hyprland, i3, and komorebi, but it does not replace DWM. Winland should cooperate with the existing Windows desktop compositor and window manager, then add reliable tiling, keyboard control, rules, fake workspaces, IPC, and optional visual feedback on top.

The first priority is a correct, stable tiling core. Visual polish comes after the core behavior is dependable.

## Non-Negotiables

- This is a Rust-only Windows-native project.
- Winland runs as a WM layer over DWM. It must not attempt to replace DWM.
- Use the `windows` crate for Win32 APIs.
- Keep unsafe Win32 code inside `winland-win32`.
- Keep `winland-core` platform-independent.
- Do not add Electron.
- Do not add a permanent render loop.
- Prefer event-driven behavior over polling.
- Do not use undocumented or private Windows APIs unless the user explicitly asks for that.
- Do not implement visual effects before the tiling core is stable.

## Crate Boundaries

- `winland-core`: platform-independent state, layout data structures, geometry, window metadata models, workspace models, rules evaluation, and pure tests.
- `winland-win32`: all direct Win32 calls, HWND handling, monitor discovery, window enumeration, event hook registration, hotkey registration, movement and resize calls, and safe wrappers around unsafe code.
- `winland-daemon`: long-running process orchestration, event loop, state synchronization, IPC server, and integration between `winland-core` and `winland-win32`.
- `winland-cli`: human and script-facing commands that talk to the daemon or run narrow diagnostics.
- `winland-config`: config schema, TOML parsing, validation, and defaults.

If a module needs `HWND`, `RECT`, `BOOL`, raw pointers, callbacks, or Win32 handles, it belongs in `winland-win32` or behind a type exported by `winland-win32`. If a module can be tested without Windows, it probably belongs in `winland-core`.

## Development Order

1. Discover and classify top-level windows.
2. Implement a safe tile-once command that can arrange existing windows.
3. Add an event-driven daemon that reacts to window changes.
4. Add keyboard control through documented Win32 mechanisms.
5. Build a real layout engine in `winland-core`.
6. Add fake workspaces by hiding, showing, moving, or restoring window sets through documented APIs.
7. Add window rules and config.
8. Add IPC and a stronger CLI.
9. Add borders or visual feedback only after tiling is stable.
10. Add optional animations only after layout, events, IPC, and rules are reliable.
11. Add bar or status integration.
12. Harden edge cases.

## Testing Rules

- Put pure layout, geometry, workspace, and rule tests in `winland-core`.
- Keep `winland-core` tests deterministic and platform-independent.
- Add focused unit tests for every new layout behavior before wiring it into Win32.
- Treat `winland-win32` tests as integration or smoke tests unless the code can be tested through pure adapters.
- Do not require a developer's desktop to be rearranged by default test runs.
- Any test that moves, hides, focuses, or resizes real windows must be opt-in and clearly named.
- Prefer fake window models in `winland-core` for broad behavioral coverage.
- Before merging behavior changes, run `cargo fmt`, `cargo clippy --workspace --all-targets`, and `cargo test --workspace` when practical.

## Code Style Rules

- Use Rust 2024 idioms already established in the workspace.
- Prefer small, explicit types for geometry and window identity instead of loosely passing tuples or strings.
- Keep public APIs narrow and boring until requirements are proven.
- Make platform-specific errors descriptive at the boundary where Win32 calls fail.
- Wrap unsafe code in the smallest practical scope.
- Every unsafe block must have a clear safety comment explaining the preconditions.
- Avoid global mutable state. If process-wide state is required for Win32 callbacks, isolate it inside `winland-win32`.
- Prefer structured logging with `tracing` over ad hoc prints in daemon code.
- CLI output should be stable enough for humans first, scripts later through explicit machine-readable flags.
- Do not add dependencies casually. Favor the standard library and existing workspace dependencies unless a crate clearly reduces risk.

## Windows Behavior Rules

- Use documented APIs such as window enumeration, window placement, monitor APIs, event hooks, and hotkeys.
- Expect apps to be strange: cloaked windows, tool windows, child windows, UWP windows, elevated windows, fullscreen apps, minimized windows, and windows that refuse movement.
- Never assume all visible top-level windows are manageable.
- Avoid fighting the user. If the foreground window or active monitor changes, react conservatively.
- Prefer reversible operations. Track previous placement when changing window geometry so recovery features can be added later.

## Visuals Policy

Do not build blur, shadows, animations, custom compositing, overlays, or decorative effects before the tiling core is stable. Borders and visual feedback are allowed only after layout correctness, event handling, keyboard control, and basic rules are dependable.
