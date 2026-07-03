# Widget API

Winland widgets are normal user processes launched by the CLI. They are not part of the tiling core, and they do not automatically reserve desktop space. A widget gets data from the CLI runner, renders it with Slint, can handle pointer input like any other app, and can optionally read status blocks from external programs.

```powershell
cargo run -p winland-cli -- widget run taskbar
cargo run -p winland-cli -- widget run --file .\widgets\my-bar.slint --component MyBar
cargo run -p winland-cli -- widget run taskbar --plugin-stream "my-status.exe"
```

The built-in taskbar is just a Slint widget at `winland-cli/widgets/taskbar.slint`. Use it as the reference implementation for custom bars.

## Runtime Model

`winland widget run` creates one or more Slint windows and pushes data into public Slint properties. Missing properties are ignored, so small widgets can declare only the properties they need.

Widgets own their own interaction logic. Winland does not intercept widget clicks or route widget UI events. A Slint widget can declare `callback run-command(string);`; when present, the CLI runner registers that callback and launches the supplied command line as a normal child process. This callback is generic: it can run `notepad.exe`, a PowerShell script, a custom status tool, or the Winland CLI.

Data sources are event-driven:

- daemon state snapshots arrive through the IPC `subscribe-state` stream
- local time updates once per second
- `--plugin-once` reads one JSON object from an executable
- `--plugin-stream` reads newline-delimited JSON objects from an executable

When any source produces an update, the CLI schedules a Slint event-loop update immediately.

## Slint Surface

A custom widget should export a root `Window` component. For a taskbar-style panel, set `no-frame: true` so Windows does not create a titlebar before Winland configures the panel window.

```slint
export struct WorkspaceRow {
    id: int,
    name: string,
    command: string,
    active: bool,
    window-count: int,
}

export struct WindowRow {
    handle: float,
    handle-text: string,
    command: string,
    title: string,
    workspace-id: int,
    focused: bool,
    visible: bool,
    is-minimized: bool,
    participation: string,
}

export struct PluginBlock {
    source: string,
    label: string,
    text: string,
}

export component MyBar inherits Window {
    in property <bool> topmost;
    in property <string> label;
    in property <[WorkspaceRow]> workspaces;
    in property <[WindowRow]> windows;
    in property <[PluginBlock]> plugin-blocks;
    in property <string> time-text;
    callback run-command(string);

    no-frame: true;
    always-on-top: root.topmost;
    title: "My Winland Bar";
}
```

Available properties:

| Property | Type | Source | Meaning |
| --- | --- | --- | --- |
| `topmost` | `bool` | CLI flag | `true` unless `--no-topmost` is passed. |
| `always-on-top` | `bool` | CLI flag | Compatibility alias for widgets that bind Slint's built-in `always-on-top` directly. |
| `label` | `string` | Derived | Compact summary: active workspace, focused window, and time. Useful for simple widgets. |
| `workspaces` | `[WorkspaceRow]` | Daemon | Workspace id/name, active flag, visible window count, and a generic command string for the built-in taskbar. |
| `windows` | `[WindowRow]` | Daemon | Open windows known to the daemon, including minimized tracked windows on the active workspace. |
| `plugin-blocks` | `[PluginBlock]` | External plugins | Status blocks from plugin executables. |
| `time-text` | `string` | Local clock | Local time in `HH:MM` form. |

`WorkspaceRow.command` and `WindowRow.command` are ordinary command lines. The built-in taskbar fills them with commands such as `winland command switch-workspace 2` and `winland command focus-window 0x123456`, resolved to the current `winland-cli.exe` path when possible.

`WindowRow.handle-text` is the HWND formatted for display or CLI use, `WindowRow.is-minimized` reports whether the tracked window is minimized, and `WindowRow.participation` is a display string such as `tiled`, `floating`, or `temporary-floating`.

## Widget Commands

If the root Slint component declares `callback run-command(string);`, the CLI runner registers it. The callback launches the given command line through the platform shell and records attempts, exit status, stdout, and stderr in:

```powershell
$env:TEMP\winland-widget-commands.log
```

This logging is intentionally small and local to the widget process. It is useful when a widget is launched by the daemon's `[ui].startup_commands` and does not have a visible console.

The built-in taskbar uses `TouchArea` handlers to call `root.run-command(...)` for workspace pills and window buttons. Custom widgets can use the same callback for any command:

```slint
TouchArea {
    clicked => {
        root.run-command("notepad.exe");
    }
}
```

## Minimal Widget

This widget only uses the summary label:

```slint
export component TinyBar inherits Window {
    in property <bool> topmost;
    in property <string> label;

    no-frame: true;
    always-on-top: root.topmost;
    title: "Tiny Winland Bar";
    preferred-height: 32px;
    background: #20242a;

    Text {
        text: root.label;
        color: white;
        font-size: 13px;
        vertical-alignment: center;
        x: 10px;
        width: parent.width - 20px;
        height: parent.height;
        overflow: elide;
    }
}
```

Run it with:

```powershell
cargo run -p winland-cli -- widget run --file .\widgets\tiny-bar.slint --component TinyBar --height 32
```

## External Plugins

External widget plugins are ordinary executables. They do not need to link against Winland.

`--plugin-once` runs the command once and reads one JSON object from stdout:

```powershell
cargo run -p winland-cli -- widget run taskbar --plugin-once "battery-status.exe"
```

`--plugin-stream` keeps the process alive and reads newline-delimited JSON objects:

```powershell
cargo run -p winland-cli -- widget run taskbar --plugin-stream "cpu-status.exe"
```

Each object may use either `label` or `name` for the block label, and `text`, `value`, or `status` for the displayed value:

```json
{"label":"CPU","text":"14%"}
{"name":"VPN","status":"up"}
{"label":"Build","value":"green"}
```

For streams, flush stdout after each JSON line. The CLI replaces the existing block with the same plugin command source, so a plugin can keep updating one status badge.

A tiny PowerShell stream plugin:

```powershell
while ($true) {
  $cpu = Get-Counter '\Processor(_Total)\% Processor Time'
  $value = [math]::Round($cpu.CounterSamples[0].CookedValue)
  Write-Output (@{ label = "CPU"; text = "$value%" } | ConvertTo-Json -Compress)
  Start-Sleep -Seconds 1
}
```

Run it with:

```powershell
cargo run -p winland-cli -- widget run taskbar --plugin-stream "powershell -ExecutionPolicy Bypass -File .\widgets\cpu.ps1"
```

## Layout Cooperation

Widgets do not reserve space automatically. Reserve panel space in config:

```toml
[layout]
offset = { bottom = 40 }
```

Ignore the widget window with a normal rule when needed:

```toml
[[window_rules]]
name = "ignore my widget"

[window_rules.match]
title = "My Winland Bar"

[window_rules.action]
manage = false
```

Use `--no-topmost` if you want the widget to stay in the normal z-order band:

```powershell
cargo run -p winland-cli -- widget run taskbar --no-topmost
```

## Direct Daemon Subscription

Widget executables normally do not need to speak IPC directly; the CLI runner subscribes to daemon state and maps snapshots into Slint properties. Advanced tools can use the IPC request named `subscribe-state`. It keeps the named pipe open and streams JSON-line state responses whenever daemon state changes.

For one-shot daemon actions, prefer the human CLI:

```powershell
cargo run -p winland-cli -- command switch-workspace 2
cargo run -p winland-cli -- command focus-window 0x123456
```

For one-shot inspection, prefer:

```powershell
cargo run -p winland-cli -- state --json
```
