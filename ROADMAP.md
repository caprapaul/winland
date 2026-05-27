# Winland Roadmap

This roadmap is an agile backlog, not a fixed sequence. Work should move in thin vertical slices that produce usable behavior, preserve the Rust/Win32 boundaries, and can be tested without destabilizing the desktop.

## Planning Model

- Keep a short current focus and finish it before pulling broad new work.
- Prefer user-visible behavior over architecture-only milestones, but do the architecture needed to keep the slice safe.
- Each slice should include core state, Win32 integration, config if user-facing, CLI or diagnostics if useful, and tests.
- Reorder the backlog when new risks or user needs appear.
- Do not treat later backlog items as blocked forever; pull a small version forward when it helps validate the product.

## Definition of Done

- The behavior works through documented Windows APIs.
- Unsafe Win32 code stays inside `winland-win32`.
- Platform-independent decisions are tested in `winland-core` or another non-Win32 layer.
- Desktop-mutating tests are opt-in.
- `cargo fmt`, `cargo clippy --workspace --all-targets`, and `cargo test --workspace` pass when practical.
- Logs or CLI diagnostics make failures understandable.
- The feature has clear "not doing yet" boundaries.

## Baseline

These capabilities are treated as the working baseline for planning:

- Window discovery and conservative filtering.
- A tile-once path.
- Event-driven daemon foundations.
- Live tiling loop: startup retile, dynamic retile, drag-to-float during interactive move/resize, and retile-on-drag-end.
- Keyboard command foundations.
- Hotkey override mode with documented low-level hook support, opt-in interception, game-safe bypass rules, and measured hook decision latency.
- IPC and CLI control with a versioned local protocol and state/query commands.
- A platform-independent layout core.
- Fake workspace foundations where present.
- TOML config and window rules.

## Current Focus: Experimental Shell Replacement

Goal: Implement an opt-in, reversible experimental shell replacement path so Winland can run as the user shell in the VM while keeping DWM in place and preserving a clear recovery path.

Tasks:
- Add a small shell-mode executable or daemon mode that starts Winland without assuming Explorer is already running.
- Add explicit CLI commands for the experimental path, such as installing, uninstalling, showing status, and launching a one-session shell test.
- Restrict persistent shell changes to an explicit command that prints what will change and how to undo it.
- Prefer per-user shell replacement in the VM over machine-wide replacement for the first prototype.
- Store the previous shell value before changing it and provide a recovery command that restores it.
- Add a command to launch or restore `explorer.exe` manually from Winland shell mode.
- Write a short design note alongside the prototype explaining the chosen registry key or documented mechanism, VM checkpoint workflow, recovery steps, and known failure modes.
- Keep DWM in place. This must not become compositor replacement work.
- Keep the normal Winland daemon path working without shell swapping.

Done criteria:
- A VM user can opt into Winland shell mode, sign out or restart, and land in Winland instead of Explorer.
- The same user can restore Explorer through a documented Winland recovery command.
- The previous shell setting is captured before modification and can be restored.
- The implementation refuses to run persistent shell changes unless an explicit experimental flag or command is used.
- The design note states the VM setup and checkpoint workflow used for tests.
- The prototype is reversible, opt-in, and isolated from the default daemon.
- The project can still run as a normal layer over DWM and Explorer.

Not doing yet:
- No default shell replacement.
- No machine-wide shell replacement.
- No persistent registry changes without an explicit user command, stored previous value, and recovery command.
- No private shell APIs.
- No DWM replacement.
- No kiosk-only or enterprise-only assumption unless clearly documented as such.

## Backlog

Workspace UX:
- Improve fake workspace switching, window assignment, and per-monitor workspace behavior.
- Keep behavior compatible with documented APIs and do not depend on private Windows virtual desktop APIs.

Layout depth:
- Add layout variants, per-workspace layout state, ratio control, focus movement, swaps, and reset behavior.
- Keep all layout decisions deterministic and testable without Win32.

Window rule hardening:
- Expand match criteria only when real use cases need them.
- Keep rule precedence simple, documented, and tested.

Config polish:
- Improve validation, defaults, examples, and reload diagnostics.
- Keep config reload explicit until automatic file watching proves useful.

Visual feedback:
- Add optional focus borders or lightweight indicators after tiling behavior is stable.
- Keep rendering event-driven and optional.
- Do not add blur, shadows, complex decorations, or compositor-like effects yet.

Bar/status integration:
- Expose workspace, focused window, layout name, monitor state, and daemon health through IPC.
- Avoid polling-driven status consumers.

Hardening:
- Handle DPI scaling, mixed monitors, taskbar changes, monitor hotplug, sleep and resume, RDP, elevated windows, fullscreen apps, UAC prompts, and stubborn windows.
- Add recovery commands for restoring previous placements.
- Improve logging and diagnostics.
- Review unsafe code and Win32 lifetime assumptions.

Optional animations:
- Only consider animations after layout, events, IPC, rules, and visual feedback are stable.
- Animations must be optional, disableable, and unable to break the final layout state.
- No custom compositor and no undocumented DWM hooks.
