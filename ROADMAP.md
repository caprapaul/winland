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
- A platform-independent layout core.
- Fake workspace foundations where present.
- TOML config and window rules.

## Current Focus: IPC and CLI Control

Goal: Make the daemon controllable and observable from command-line tools.

Tasks:
- Define a local IPC protocol.
- Add CLI commands for state, windows, monitors, workspaces, focus, move, swap, retile, reload, and quit.
- Support human-readable output first and explicit JSON output where useful.
- Add protocol versioning from the start.
- Handle daemon-not-running errors cleanly.

Current slice:
- Ship a versioned local named-pipe protocol with a `state` request.
- Add `winland state` and `winland state --json` for daemon health and snapshot counts.
- Defer mutating IPC commands until the protocol and daemon request path are proven.

Done criteria:
- CLI commands can control a running daemon.
- State inspection is good enough for troubleshooting.
- IPC errors do not crash the daemon.

Not doing yet:
- No network IPC.
- No remote control from other machines.
- No plugin system.

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

Lower-level shell integration research:
- Investigate whether Winland can offer a lower-level desktop experience through documented Windows shell mechanisms, custom shell modes, or reversible "shell swapping" experiments.
- Treat this as a research spike before any implementation. Write down what "shell swapping" means for Winland: replacing Explorer as the shell, supervising Explorer, launching beside Explorer, or switching between shells for a session.
- Identify documented APIs, registry settings, Windows editions, permission requirements, recovery steps, and failure modes.
- Keep DWM in place. This must not become compositor replacement work.
- Require an explicit opt-in experimental mode before changing shell startup behavior, Explorer behavior, or session-level shell settings.
- Design a safe recovery path before any prototype, including restoring Explorer and undoing any persistent setting.
- Keep the normal Winland daemon path working without shell swapping.

Done criteria:
- A written design note explains viable documented approaches, risks, recovery steps, and why the preferred approach is safe enough to prototype.
- Any prototype is reversible, opt-in, and isolated from the default daemon.
- The project can still run as a normal layer over DWM and Explorer.

Not doing yet:
- No default shell replacement.
- No persistent registry changes without an explicit user command and recovery command.
- No private shell APIs.
- No DWM replacement.
- No kiosk-only or enterprise-only assumption unless clearly documented as such.

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
