# Experimental Shell Replacement

Winland normally runs as a layer over DWM and Explorer. This prototype adds an explicit, reversible shell path for VM testing only. It does not replace DWM, does not use private shell APIs, and does not change the normal daemon path.

## Mechanism

The prototype writes the per-user `Shell` value under:

```text
HKCU\Software\Microsoft\Windows NT\CurrentVersion\Winlogon
```

The installed command points at `winland-shell.exe`. The shell executable is intentionally small: it starts `winland-daemon.exe` without assuming `explorer.exe` is already running, then waits for the daemon process. The default `winland-daemon` behavior remains unchanged.

For VM testing of elevated windows, `winland-shell` can be asked to start only the daemon elevated:

```powershell
winland shell test --elevated-daemon
```

Without further setup, that may show a UAC prompt. To approve the elevated daemon once and avoid repeated prompts, create the scheduled task:

```powershell
winland shell install-elevated-task --experimental
```

Then use the elevated daemon shell command:

```powershell
winland shell test --elevated-daemon
```

or persist it:

```powershell
winland shell install --experimental --elevated-daemon
```

The scheduled task is imported with `HighestAvailable` run level and `InteractiveToken` logon type, then started on demand by `winland-shell`. Creating or deleting the task may require a UAC prompt. If the task is not installed, `winland-shell --elevated-daemon` falls back to the documented `ShellExecuteExW` `runas` verb, so Windows owns the UAC prompt. This is not the default because elevated daemons can move elevated windows, but they also run the tiling engine with more privilege than ordinary desktop apps.

The machine-wide `HKLM\Software\Microsoft\Windows NT\CurrentVersion\Winlogon\Shell` value and Microsoft's documented [Shell Launcher](https://learn.microsoft.com/en-za/windows/iot/iot-enterprise/customize/shell-launcher) enterprise/kiosk configuration are intentionally out of scope for this first prototype. The HKCU path keeps the blast radius to the VM user account.

## Commands

All examples below assume the binaries are on `PATH`. During local development from the workspace root, use the equivalent `cargo run` form:

```powershell
cargo run -p winland-cli -- shell status
```

Inspect the current state:

```powershell
winland shell status
```

Run one shell session without changing the registry:

```powershell
winland shell test
```

Install the per-user shell replacement after taking a VM checkpoint:

```powershell
winland shell install --experimental
```

Restore the captured previous shell value:

```powershell
winland shell uninstall --experimental
```

The recovery alias is equivalent:

```powershell
winland shell recover --experimental
```

Launch Explorer manually from Winland shell mode:

```powershell
winland shell explorer
```

Remove the elevated daemon scheduled task:

```powershell
winland shell uninstall-elevated-task --experimental
```

Check whether the elevated daemon scheduled task exists:

```powershell
winland shell elevated-task-status
```

## Quick Start

Build the workspace:

```powershell
cargo build --workspace
```

Run a one-session shell test without registry changes:

```powershell
cargo run -p winland-cli -- shell test
```

Run a one-session shell test with an elevated daemon. This can tile elevated windows such as Task Manager:

```powershell
cargo run -p winland-cli -- shell test --elevated-daemon
```

If you want the elevated daemon without repeated UAC prompts, create the scheduled task once:

```powershell
cargo run -p winland-cli -- shell install-elevated-task --experimental
```

Then use the same elevated shell command:

```powershell
cargo run -p winland-cli -- shell test --elevated-daemon
```

To persist Winland as the per-user shell in a VM:

```powershell
cargo run -p winland-cli -- shell install --experimental
```

To persist Winland as the per-user shell and start the daemon elevated through the scheduled task:

```powershell
cargo run -p winland-cli -- shell install --experimental --elevated-daemon
```

## Reversibility

Before changing `Shell`, Winland stores two values in the same HKCU Winlogon key:

```text
WinlandPreviousShell
WinlandPreviousShellWasPresent
```

If the previous `Shell` value existed, recovery writes it back. If it did not exist, recovery deletes `Shell` and removes the backup values. Install preserves an existing Winland backup instead of overwriting it, so repeated installs do not lose the original shell value.

Persistent changes refuse to run unless the command includes `--experimental`.

The elevated daemon scheduled task is separate from the `Shell` registry value. It is named:

```text
\Winland\DaemonElevated
```

It can be removed directly through Winland:

```powershell
winland shell uninstall-elevated-task --experimental
```

`winland shell recover --experimental` restores the previous shell value and also removes this task if it exists.

## Elevated Windows

Windows prevents lower-integrity processes from moving or resizing higher-integrity windows. If `winland-daemon` is not elevated, attempts to tile elevated windows can fail with `Access is denied`.

For elevated windows, use:

```powershell
winland shell install-elevated-task --experimental
winland shell test --elevated-daemon
```

or, for the persistent shell experiment:

```powershell
winland shell install-elevated-task --experimental
winland shell install --experimental --elevated-daemon
```

This does not make `winland-shell` itself elevated. The shell remains a small user-level entrypoint, and only the daemon is started through the highest-privilege scheduled task. That keeps the startup and recovery command surface simpler while allowing the tiling process to move elevated windows.

If the scheduled task is missing, `winland-shell --elevated-daemon` falls back to `ShellExecuteExW` with the `runas` verb, which may show a UAC prompt.

## Command Reference

| Command | Mutates Windows state | Purpose |
| --- | --- | --- |
| `winland shell status` | No | Shows the per-user shell value and stored backup values. |
| `winland shell test` | No | Starts `winland-shell` for the current session only. |
| `winland shell test --elevated-daemon` | No, unless UAC fallback is accepted | Starts `winland-shell` and asks it to start the daemon elevated. |
| `winland shell install --experimental` | Yes | Writes the per-user `Shell` value and stores backup values. |
| `winland shell install --experimental --elevated-daemon` | Yes | Writes a shell command that starts the daemon elevated. |
| `winland shell uninstall --experimental` | Yes | Restores the stored previous per-user `Shell` value. |
| `winland shell recover --experimental` | Yes | Recovery alias for uninstall; also removes the elevated task when installed. |
| `winland shell explorer` | No persistent change | Starts `explorer.exe` manually. |
| `winland shell install-elevated-task --experimental` | Yes | Creates or updates `\Winland\DaemonElevated`. |
| `winland shell uninstall-elevated-task --experimental` | Yes | Deletes `\Winland\DaemonElevated`. |
| `winland shell elevated-task-status` | No | Reports whether the elevated task exists. |

## VM Test Workflow

1. Build `winland-cli` and `winland-daemon` in the VM.
2. Confirm `winland shell test` starts `winland-shell`, which in turn starts the daemon, without registry changes.
3. Take a VM checkpoint.
4. Run `winland shell install --experimental`.
5. Sign out or restart the VM user.
6. Confirm the session starts `winland-shell.exe` instead of Explorer.
7. Use Task Manager, a terminal, or an existing command surface to run `winland shell explorer` if Explorer is needed.
8. Run `winland shell recover --experimental`. This also removes the elevated daemon scheduled task when one is installed.
9. Sign out or restart and confirm Explorer is restored.

## Recovery From a Bare Shell

If Winland starts but Explorer does not and there is no launcher yet:

1. Press `Ctrl+Shift+Esc` to open Task Manager.
2. Choose `Run new task`.
3. Start `powershell`.
4. From the workspace or installed binary directory, run:

```powershell
winland shell explorer
winland shell recover --experimental
```

If using `cargo run` from the workspace:

```powershell
cargo run -p winland-cli -- shell explorer
cargo run -p winland-cli -- shell recover --experimental
```

Then sign out or restart the VM user to confirm Explorer is back as the shell.

## Known Failure Modes

- If `winland-shell.exe` or `winland-daemon.exe` is moved after install, the next sign-in may not start Winland correctly. Use Task Manager's Run dialog to start a terminal, then run `winland shell recover --experimental`.
- If `--elevated-daemon` is used without first running `install-elevated-task`, sign-in may require accepting a UAC prompt before Winland can tile elevated windows.
- If no backup values exist, recovery refuses to guess. Launch Explorer with `winland shell explorer` and inspect `winland shell status`.
- Shell mode is currently just the daemon running as the user shell. It does not provide a taskbar, start menu, launcher, or recovery UI.
- This path has not been promoted to normal startup behavior. It is VM-only experimental work until the core tiling and recovery story are proven.
