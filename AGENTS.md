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
- Do not change Windows shell startup behavior, replace Explorer, or persist shell-related settings outside the explicitly requested experimental shell replacement path.

## Crate Boundaries

- `winland-core`: platform-independent state, layout data structures, geometry, window metadata models, workspace models, config-facing domain types, rules evaluation, and pure tests.
- `winland-win32`: all direct Win32 calls, HWND handling, monitor discovery, window enumeration, event hook registration, hotkey registration, movement and resize calls, and safe wrappers around unsafe code.
- `winland-daemon`: long-running process orchestration, event loop, state synchronization, IPC server, and integration between `winland-core` and `winland-win32`.
- `winland-cli`: human and script-facing commands that talk to the daemon or run narrow diagnostics.
- `winland-config`: config schema, TOML parsing, validation, defaults, config file discovery, and conversion from user config into core domain types.

If a module needs `HWND`, `RECT`, `BOOL`, raw pointers, callbacks, or Win32 handles, it belongs in `winland-win32` or behind a type exported by `winland-win32`. If a module can be tested without Windows, it probably belongs in `winland-core`.

## Delivery Model

- Use a requirement-first workflow in the current chat. Do not make the user manage a separate roadmap chat or manually switch between planning files before implementation.
- Start from the user's plain-language task description, even when it is rough or incomplete.
- Before coding, ask focused requirement questions when behavior, safety, configuration, or done criteria are unclear.
- Keep questions short and practical. Prefer asking only what is needed to turn the task into an implementable slice.
- After questions are answered, summarize the requirement back to the user in concrete terms:
  - desired behavior
  - important exceptions or safety rules
  - configuration or CLI surface, if any
  - done criteria
  - tests or manual verification expected
- Wait for confirmation or correction before implementing when the requirement changes product behavior in a meaningful way.
- Once confirmed, implement the slice end to end in the same chat: core model, Win32 integration, config if user-facing, diagnostics or CLI hooks when useful, and tests.
- Prefer thin vertical slices that deliver usable behavior end to end.
- Keep the active task small enough to finish, test, and explain.
- Use `ROADMAP.md` only as background context when useful. Do not treat it as the source of truth over the user's current task.
- Update planning documents only when the user explicitly asks for documentation changes or when a durable rule should guide future agents.
- Treat shell replacement as experimental work: it must be opt-in, VM-tested first, documented, and include a recovery command before any persistent shell setting is changed.
- Do not start visual polish while core tiling behavior is unstable.

## Testing Rules

- Put pure layout, geometry, workspace, and rule tests in `winland-core`.
- Keep `winland-core` tests deterministic and platform-independent.
- Add focused unit tests for every new layout behavior before wiring it into Win32.
- Treat `winland-win32` tests as integration or smoke tests unless the code can be tested through pure adapters.
- Do not require a developer's desktop to be rearranged by default test runs.
- Any test that moves, hides, focuses, or resizes real windows must be opt-in and clearly named.
- Prefer fake window models in `winland-core` for broad behavioral coverage.
- Before merging behavior changes, run `cargo fmt`, `cargo clippy --workspace --all-targets`, and `cargo test --workspace` when practical.

## Configuration Policy

- Use TOML for human-edited configuration unless there is a strong reason to change.
- Keep config parsing, defaults, validation, and file discovery in `winland-config`.
- Keep config-independent behavior in `winland-core`; do not make core layout logic read files or environment variables.
- Treat configuration as a stable user interface. Add fields conservatively and validate them with clear errors.
- Provide sensible defaults so Winland can run without a config file.
- Once the configuration system lands, user-facing workflow behavior should be configurable when practical.
- Cover at least these config areas:
  - Hotkeys: modifier/key combinations mapped to named commands, plus an explicit mode for normal registration versus advanced interception.
  - Layouts: default layout, gaps, ratios, per-monitor or per-workspace layout choices, and layout-specific options.
  - Workspaces: names, count, initial monitor assignment, and startup behavior.
  - Window rules: match criteria and actions such as manage, ignore, float, target workspace, and always-on-workspace.
  - Behavior toggles: startup retile, dynamic retile, drag-to-float, retile-on-drag-end, focus behavior, restore behavior, handling of minimized windows, and conservative safety switches.
  - Visual feedback: border enablement, colors, widths, and related options after the visual feedback backlog item is accepted.
  - Daemon and IPC: logging level, IPC endpoint selection when needed, reload behavior, and diagnostics settings.
- Add a `winland config validate` or equivalent CLI command when config files become user-editable.
- Config reload should be explicit at first. Automatic file watching can come later if it proves useful.

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
- Tile manageable windows by default once automatic tiling is enabled.
- Retile on daemon startup after initial window discovery, unless the user disables that behavior.
- Retile in response to window lifecycle and monitor events, but debounce event bursts so Winland does not thrash.
- When the user starts dragging or resizing a managed tiled window, treat it as a temporary floating window and pause tiling pressure on that window.
- When the drag or resize ends, return the window to tiled state and retile the affected workspace by default.
- Hotkey override mode must not add meaningful input latency to games. Bypass fullscreen, exclusive, borderless-fullscreen, and rule-matched game processes by default.
- If a low-level keyboard hook is used, keep hook callbacks tiny: decide quickly, never run command logic inside the hook, and return immediately for bypassed game windows.
- Prefer reversible operations. Track previous placement when changing window geometry so recovery features can be added later.

## Visuals Policy

Do not build blur, shadows, animations, custom compositing, overlays, or decorative effects before the tiling core is stable. Borders and visual feedback are allowed only after layout correctness, event handling, keyboard control, and basic rules are dependable.
