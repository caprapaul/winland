# Window Rules

Rules live at top level as repeated `[[window_rules]]` tables.

```toml
[[window_rules]]
name = "float settings"
[window_rules.match]
title = { contains = "Settings" }
process_name = "SystemSettings.exe"
[window_rules.action]
float = true
workspace = 2
```

## Matching

All match fields in one rule must match. A rule with no match fields is invalid.

| TOML field | Runtime metadata |
| --- | --- |
| `class` | Win32 window class name. |
| `title` | Window title. |
| `executable_path` | Full process executable path when available. |
| `process_name` | File name extracted from executable path. |

Each matcher can be a string exact match:

```toml
process_name = "notepad.exe"
```

or a detailed matcher:

```toml
title = { contains = "Settings" }
class = { prefix = "Chrome" }
executable_path = { suffix = "\\notepad.exe" }
```

Supported detailed fields:

- `exact`
- `contains`
- `prefix`
- `suffix`

Exactly one detailed field must be set. Matching is case-insensitive in core rule evaluation.

## Actions

| Field | Values | Implemented behavior |
| --- | --- | --- |
| `manage` | `true`/`false` | `false` removes the window from tiling/workspace management. |
| `float` | `true`/`false` | Sets floating or tiled participation when the rule is applied. |
| `workspace` | workspace number | Tracks new matching windows on that workspace. Reload can move matching windows. |
| `always_on_workspace` | `true`/`false` | Keeps the window visible across workspace switches. |
| `layout` | string | Parsed and validated, but currently not used to choose per-window layout. Validation only accepts the configured default layout. |
| `mode` | `ignore`, `game`, `fullscreen` | Makes the window unmanageable and can activate game mode when focused. |

## Order And Precedence

Rules are evaluated in file order. Later matching rules override earlier action fields, but fields not set by the later rule keep the previous value.

```toml
[[window_rules]]
name = "float all setup windows"
[window_rules.match]
title = { contains = "Setup" }
[window_rules.action]
float = true

[[window_rules]]
name = "keep trusted setup tiled"
[window_rules.match]
process_name = "trusted-setup.exe"
[window_rules.action]
float = false
```

If both rules match, `float = false` wins while any other action fields from earlier matching rules remain.

## Ignore And Game Rules

Use `manage = false` for utilities that should not tile:

```toml
[[window_rules]]
name = "ignore helper window"
[window_rules.match]
class = { suffix = "ToolWindow" }
[window_rules.action]
manage = false
```

Use `mode = "game"` or `mode = "fullscreen"` for windows that should also activate game-mode behavior when focused:

```toml
[[window_rules]]
name = "game wrapper"
[window_rules.match]
process_name = "game.exe"
[window_rules.action]
manage = false
mode = "game"
```

`mode = "ignore"` is also treated as a game-mode trigger by current detection. This is intentionally conservative.

## Workspace Examples

Move chat to workspace 3:

```toml
[[window_rules]]
name = "chat workspace"
[window_rules.match]
process_name = "Discord.exe"
[window_rules.action]
workspace = 3
```

Keep a small utility visible on every fake workspace:

```toml
[[window_rules]]
name = "sticky meter"
[window_rules.match]
title = { contains = "Meter" }
[window_rules.action]
float = true
always_on_workspace = true
```

## Interactions

- Game executable lists in `[game_mode]` make matching windows unmanaged even without a rule.
- Fullscreen windows are skipped by workspace tracking when first discovered.
- A rule can force a previously floating window tiled on reload with `float = false`.
- A rule can remove a window from tile order on reload with `manage = false` or a mode of `ignore`, `game`, or `fullscreen`.

