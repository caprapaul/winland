# Winland Roadmap

## Phase 1: Window Discovery

Goal: Build a trustworthy view of manageable desktop windows.

Tasks:
- Enumerate top-level windows with documented Win32 APIs.
- Collect title, class name, process id, executable path when available, visibility, cloaking state, styles, and current rectangle.
- Filter out obvious non-manageable windows such as tool windows, invisible windows, shell surfaces, and empty placeholders.
- Expose discovery through a CLI diagnostic command.
- Keep Win32 details inside `winland-win32`.

Done criteria:
- Running the CLI lists candidate windows with enough metadata to debug filtering.
- The discovery code does not move, resize, hide, or focus windows.
- The filter is conservative and documented.

Not yet:
- No tiling.
- No daemon.
- No hotkeys.
- No rules engine.
- No visual feedback.

## Phase 2: Tile-Once Command

Goal: Arrange current manageable windows once, on explicit command.

Tasks:
- Add pure geometry and monitor models to `winland-core`.
- Implement a simple deterministic tiling layout for one monitor.
- Query monitor work areas through `winland-win32`.
- Move and resize only the selected manageable windows.
- Provide a CLI command that performs one tiling pass.

Done criteria:
- A user can run one command and see windows arranged predictably.
- Layout math is covered by `winland-core` tests.
- Failed window moves are reported without aborting the whole operation unnecessarily.

Not yet:
- No background daemon.
- No automatic reaction to new windows.
- No fake workspaces.
- No animations.
- No border rendering.

## Phase 3: Event-Driven Daemon

Goal: Keep window state updated through events instead of repeated polling.

Tasks:
- Create the daemon event loop.
- Subscribe to documented window events through Win32 event hooks.
- Track created, destroyed, shown, hidden, moved, minimized, restored, and foreground window changes.
- Reconcile state after bursts of events.
- Add structured logging.

Done criteria:
- The daemon can start, observe window lifecycle changes, and maintain an internal state snapshot.
- The daemon does not use a permanent render loop.
- Any fallback polling is narrow, temporary, documented, and not the primary mechanism.

Not yet:
- No global keyboard control.
- No IPC protocol beyond minimal diagnostics if needed.
- No workspace switching.
- No visual effects.

## Phase 4: Keyboard Control

Goal: Add reliable keyboard commands for core window actions.

Tasks:
- Register hotkeys using documented Win32 APIs.
- Route hotkey events into daemon commands.
- Implement focus movement, swap, retile, float toggle, and quit or reload commands as appropriate.
- Make keybindings configurable once config basics exist.

Done criteria:
- The daemon responds to keyboard commands without a render loop.
- Hotkey registration failures are visible and actionable.
- Keyboard behavior can be tested at the command routing level without real hotkeys.

Not yet:
- No complex modal keybinding language.
- No command chaining.
- No animations.
- No bar integration.

## Phase 5: Layout Engine

Goal: Move from one simple tiling pass to reusable, tested layout behavior.

Tasks:
- Define layout tree or stack structures in `winland-core`.
- Support master-stack or equivalent baseline layout.
- Add gaps, borders as geometry reservations only, and per-monitor layout state.
- Support window insertion, removal, focus movement, swapping, resizing ratios, and layout reset.
- Cover layout behavior with pure tests.

Done criteria:
- Layout decisions are deterministic and independent of Win32.
- Multiple monitor work areas can be handled by core data structures.
- Common operations have focused tests.

Not yet:
- No visual border drawing.
- No fake workspaces unless layout state requires a placeholder model.
- No persistence format beyond what is necessary for tests.

## Phase 6: Fake Workspaces

Goal: Provide workspace-like behavior without replacing Windows virtual desktops or DWM.

Tasks:
- Model workspaces in `winland-core`.
- Assign windows to workspace state.
- Show, hide, move, or restore windows through documented APIs.
- Preserve enough placement state to switch workspaces safely.
- Handle windows that should appear on all workspaces when rules later support that.

Done criteria:
- Users can switch between fake workspaces and see the expected windows.
- Workspace state survives ordinary window create and destroy events during the daemon session.
- Minimized, fullscreen, and unmanaged windows are handled conservatively.

Not yet:
- No deep integration with Windows virtual desktops.
- No private desktop APIs.
- No workspace animations.
- No bar protocol.

## Phase 7: Configuration and Window Rules

Goal: Add a real user configuration system for hotkeys, layouts, workspaces, behavior, and window rules.

Tasks:
- Define a TOML config schema in `winland-config`.
- Decide config file discovery paths, default file name, and behavior when no config file exists.
- Add defaults and validation for all supported config sections.
- Add config for hotkeys as modifier/key combinations mapped to named daemon commands.
- Add config for layouts, including default layout, gaps, ratios, per-monitor or per-workspace layout choices, and layout-specific options.
- Add config for workspaces, including names, count, initial monitor assignment, and startup behavior.
- Add config for behavior toggles such as focus behavior, restore behavior, minimized-window handling, and conservative safety switches.
- Match windows by class, title, executable path, process name, and other stable metadata.
- Support manage, ignore, float, target workspace, initial layout hints, and always-on-workspace behavior.
- Add config validation through the CLI, such as `winland config validate`.
- Add explicit config reload behavior through IPC or CLI.
- Add tests for config parsing, defaults, validation, rule matching, and precedence.

Done criteria:
- Winland can run with no config file by using documented defaults.
- A TOML config can define hotkeys, layout defaults, workspace basics, behavior toggles, and window rules.
- Invalid config fails with useful messages before daemon state is mutated.
- Config validation and rule behavior are tested without Win32.
- Reloading config is explicit and reports success or failure clearly.

Not yet:
- No scripting language.
- No remote config service.
- No visual rule editor.
- No automatic online rule database.
- No automatic config file watching unless explicitly requested later.

## Phase 8: Automatic Retiling and Drag-to-Float

Goal: Make tiling the default live behavior: tile on daemon start, retile dynamically, and temporarily release windows while the user drags or resizes them.

Tasks:
- Retile once on daemon startup after initial window discovery.
- Retile dynamically when manageable windows are created, destroyed, shown, hidden, restored, minimized, or moved between monitors.
- Debounce retile requests so event storms produce one coherent layout update.
- Detect user move/resize start and end events, such as documented move-size WinEvents where available.
- Model window participation explicitly: tiled by default, permanently floating by rule or command, or temporarily floating during user drag/resize.
- During user drag or resize, mark the affected tiled window as temporarily floating so Winland does not fight the drag.
- Ensure retile operations exclude temporary floating windows during the drag and re-include them after drag end.
- On drag or resize end, clear temporary floating state and retile the affected workspace by default.
- Preserve enough previous geometry to make float transitions and recovery predictable.
- Extend config with startup retile, dynamic retile, drag-to-float, and retile-on-drag-end toggles.
- Add layout and daemon tests for tiled, permanently floating, and temporarily floating participation.

Done criteria:
- Starting the daemon tiles existing manageable windows by default.
- Opening, closing, showing, hiding, minimizing, restoring, or monitor-moving manageable windows causes an event-driven retile.
- Dragging or resizing a tiled window temporarily releases it from tiling, then returns it to the tiled layout when the drag ends.
- Winland does not continuously fight the user's pointer during interactive move or resize.
- Tiled, permanently floating, and temporarily floating states have clear behavior in layout tests.
- Retile after drag end reabsorbs the window into the tiled layout by default.
- The behavior can be disabled or adjusted through config.

Not yet:
- No animation while dragging.
- No permanent floating by manual drag alone unless a command or rule requests it.
- No polling-based drag detection unless a narrow documented fallback is required.
- No visual drag overlay.

## Phase 9: Hotkey Override Mode

Goal: Let users opt into overriding app-level hotkey conflicts where documented Windows mechanisms allow it.

Tasks:
- Keep ordinary hotkeys on the documented `RegisterHotKey` path by default.
- Add config for hotkey handling mode, such as normal registration versus advanced interception.
- Use documented low-level keyboard hook APIs, such as `WH_KEYBOARD_LL`, only for explicit opt-in override mode.
- Route intercepted key combinations through the same command system as normal hotkeys.
- Allow per-binding override intent where practical, so users can choose which shortcuts should suppress delivery to the focused app.
- Add clear diagnostics for failed registrations, intercepted bindings, suppressed keys, and unsupported protected shortcuts.
- Reserve a panic or escape hotkey that Winland never intercepts.
- Document limits clearly: secure desktop, UAC prompts, `Ctrl+Alt+Del`, and reserved OS shortcuts cannot be overridden.
- Test matching and command routing without requiring a real keyboard hook.

Done criteria:
- Normal hotkey mode remains the default and still uses `RegisterHotKey`.
- Override mode is disabled unless explicitly configured.
- App-level shortcut conflicts can be intercepted where Windows allows it.
- Protected or unsupported Windows shortcuts fail safely with clear diagnostics.
- A user has a documented way to recover if an override binding is bad.

Not yet:
- No keyboard driver.
- No undocumented keyboard injection or private APIs.
- No attempt to override secure desktop shortcuts.
- No default interception of all keyboard input.
- No modal keybinding language unless explicitly added later.

## Phase 10: IPC and CLI

Goal: Make the daemon controllable and observable from command-line tools.

Tasks:
- Define a local IPC protocol.
- Add CLI commands for state, windows, monitors, workspaces, focus, move, swap, retile, reload, and quit.
- Support human-readable output first and explicit JSON output where useful.
- Add protocol versioning from the start.
- Handle daemon-not-running errors cleanly.

Done criteria:
- CLI commands can control a running daemon.
- State inspection is good enough for troubleshooting.
- IPC errors do not crash the daemon.

Not yet:
- No network IPC.
- No remote control from other machines.
- No plugin system.
- No bar-specific protocol unless needed for status experiments.

## Phase 11: Borders / Visual Feedback

Goal: Add minimal feedback for focus and managed state after core tiling is stable.

Tasks:
- Decide on a documented, low-risk rendering approach.
- Draw focus borders or lightweight indicators.
- Keep rendering event-driven.
- Avoid permanent animation or render loops.
- Make feedback optional and easy to disable.

Done criteria:
- Focus or managed state can be seen without destabilizing tiling.
- Visual feedback does not require Electron.
- Rendering resources are cleaned up correctly.

Not yet:
- No blur.
- No shadows.
- No complex decorations.
- No animations.
- No compositor replacement behavior.

## Phase 12: Optional Animations

Goal: Add restrained, optional motion only after correctness is established.

Tasks:
- Prototype animations behind a feature flag or config option.
- Animate only window geometry transitions that are already valid without animation.
- Provide a no-animation path as the default until proven stable.
- Respect reduced motion preferences where practical.

Done criteria:
- Animations can be disabled completely.
- Animation failure cannot break the final layout state.
- There is no unconditional permanent render loop.

Not yet:
- No custom compositor.
- No undocumented DWM hooks.
- No animation-first redesign of the layout engine.

## Phase 13: Bar/Status Integration

Goal: Expose useful state to external bars or status tools.

Tasks:
- Publish current workspace, focused window, layout name, monitor state, and daemon health through IPC.
- Add CLI commands suitable for scripts.
- Consider event subscription for status consumers.
- Document stable fields and unstable fields separately.

Done criteria:
- External tools can display Winland state without scraping logs.
- Status consumers do not force the daemon into polling.
- IPC remains local and versioned.

Not yet:
- No built-in full bar unless explicitly requested.
- No Electron-based UI.
- No online service dependency.

## Phase 14: Hardening and Edge Cases

Goal: Make Winland robust enough for daily use on real Windows desktops.

Tasks:
- Handle DPI scaling, mixed monitors, taskbar changes, monitor hotplug, sleep and resume, RDP, elevated windows, fullscreen apps, UAC prompts, and stubborn windows.
- Add recovery commands for restoring previous placements.
- Improve logging and diagnostics.
- Add opt-in integration tests for risky desktop operations.
- Review unsafe code and Win32 lifetime assumptions.

Done criteria:
- Known edge cases have documented behavior.
- The daemon can recover from common failures without restart where practical.
- Unsafe Win32 code has been reviewed and minimized.
- The project has a clear issue template or diagnostic checklist.

Not yet:
- No private Windows APIs by default.
- No unsupported compositor replacement.
- No feature work that hides unresolved stability issues.
