#[cfg(windows)]
mod platform {
    use std::fs;
    use std::mem::zeroed;
    use std::path::Path;
    use std::process::{Command, Output};
    use std::ptr::null;
    use std::time::{SystemTime, UNIX_EPOCH};

    use windows::Win32::Foundation::{
        CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WAIT_FAILED,
    };
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS,
        REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW,
        RegQueryValueExW, RegSetValueExW,
    };
    use windows::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, GetExitCodeProcess, INFINITE,
        PROCESS_INFORMATION, STARTUPINFOW, WaitForSingleObject,
    };
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, PWSTR};

    use crate::{Result, Win32Error};

    pub const USER_WINLOGON_KEY: &str =
        r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\Winlogon";
    const USER_WINLOGON_SUBKEY: &str = r"Software\Microsoft\Windows NT\CurrentVersion\Winlogon";
    const SHELL_VALUE: &str = "Shell";
    const WINLAND_PREVIOUS_SHELL_VALUE: &str = "WinlandPreviousShell";
    const WINLAND_PREVIOUS_SHELL_WAS_PRESENT_VALUE: &str = "WinlandPreviousShellWasPresent";
    const ELEVATED_DAEMON_TASK_NAME: &str = r"\Winland\DaemonElevated";

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ShellReplacementStatus {
        pub registry_key: &'static str,
        pub current_shell: Option<String>,
        pub backup_shell: Option<String>,
        pub backup_shell_was_present: Option<bool>,
        pub is_winland_shell: bool,
    }

    impl ShellReplacementStatus {
        fn from_registry() -> Result<Self> {
            let key = match open_winlogon_key(KEY_READ) {
                Ok(key) => key,
                Err(Win32Error::Registry { code, .. }) if code == ERROR_FILE_NOT_FOUND.0 => {
                    return Ok(Self {
                        registry_key: USER_WINLOGON_KEY,
                        current_shell: None,
                        backup_shell: None,
                        backup_shell_was_present: None,
                        is_winland_shell: false,
                    });
                }
                Err(error) => return Err(error),
            };
            let current_shell = query_string_value(&key, SHELL_VALUE)?;
            let backup_shell = query_string_value(&key, WINLAND_PREVIOUS_SHELL_VALUE)?;
            let backup_shell_was_present =
                query_string_value(&key, WINLAND_PREVIOUS_SHELL_WAS_PRESENT_VALUE)?
                    .map(|value| value == "1");

            let is_winland_shell = current_shell
                .as_deref()
                .is_some_and(shell_command_looks_like_winland);

            Ok(Self {
                registry_key: USER_WINLOGON_KEY,
                current_shell,
                backup_shell,
                backup_shell_was_present,
                is_winland_shell,
            })
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ShellReplacementChange {
        pub registry_key: &'static str,
        pub previous_shell: Option<String>,
        pub new_shell: Option<String>,
    }

    pub fn shell_replacement_status() -> Result<ShellReplacementStatus> {
        ShellReplacementStatus::from_registry()
    }

    pub fn install_shell_replacement(shell_command: &str) -> Result<ShellReplacementChange> {
        let shell_command = shell_command.trim();
        if shell_command.is_empty() {
            return Err(Win32Error::InvalidShellCommand);
        }

        let key = create_winlogon_key()?;
        let previous_shell = query_string_value(&key, SHELL_VALUE)?;

        if query_string_value(&key, WINLAND_PREVIOUS_SHELL_WAS_PRESENT_VALUE)?.is_none() {
            let was_present = if previous_shell.is_some() { "1" } else { "0" };
            set_string_value(&key, WINLAND_PREVIOUS_SHELL_WAS_PRESENT_VALUE, was_present)?;
            set_string_value(
                &key,
                WINLAND_PREVIOUS_SHELL_VALUE,
                previous_shell.as_deref().unwrap_or(""),
            )?;
        }

        set_string_value(&key, SHELL_VALUE, shell_command)?;

        Ok(ShellReplacementChange {
            registry_key: USER_WINLOGON_KEY,
            previous_shell,
            new_shell: Some(shell_command.to_owned()),
        })
    }

    pub fn restore_shell_replacement() -> Result<ShellReplacementChange> {
        let key = open_winlogon_key(KEY_READ | KEY_SET_VALUE)?;
        let previous_shell = query_string_value(&key, SHELL_VALUE)?;
        let Some(was_present) = query_string_value(&key, WINLAND_PREVIOUS_SHELL_WAS_PRESENT_VALUE)?
        else {
            return Err(Win32Error::MissingShellBackup);
        };

        let restored_shell = if was_present == "1" {
            let shell = query_string_value(&key, WINLAND_PREVIOUS_SHELL_VALUE)?
                .ok_or(Win32Error::MissingShellBackup)?;
            set_string_value(&key, SHELL_VALUE, &shell)?;
            Some(shell)
        } else {
            delete_value_if_present(&key, SHELL_VALUE)?;
            None
        };

        delete_value_if_present(&key, WINLAND_PREVIOUS_SHELL_VALUE)?;
        delete_value_if_present(&key, WINLAND_PREVIOUS_SHELL_WAS_PRESENT_VALUE)?;

        Ok(ShellReplacementChange {
            registry_key: USER_WINLOGON_KEY,
            previous_shell,
            new_shell: restored_shell,
        })
    }

    pub fn launch_explorer() -> Result<()> {
        launch_process("explorer.exe")
    }

    pub fn launch_shell_test(shell_command: &str) -> Result<()> {
        let shell_command = shell_command.trim();
        if shell_command.is_empty() {
            return Err(Win32Error::InvalidShellCommand);
        }

        launch_process(shell_command)
    }

    pub fn launch_elevated_process_and_wait(
        executable: &Path,
        arguments: &[String],
    ) -> Result<u32> {
        let verb = wide_null("runas");
        let executable = wide_null(&executable.display().to_string());
        let parameters = arguments
            .iter()
            .map(|argument| quote_windows_arg(argument))
            .collect::<Vec<_>>()
            .join(" ");
        let parameters = wide_null(&parameters);

        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(executable.as_ptr()),
            lpParameters: PCWSTR(parameters.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };

        // SAFETY: SHELLEXECUTEINFOW points at null-terminated UTF-16 strings
        // that live for this call. The documented "runas" verb asks Windows to
        // broker elevation through UAC, and SEE_MASK_NOCLOSEPROCESS requests an
        // owned process handle that is closed below.
        unsafe {
            ShellExecuteExW(&mut info).map_err(|source| Win32Error::Windows {
                context: "ShellExecuteExW(runas)",
                source,
            })?;
        }

        let process = info.hProcess;
        if process.is_invalid() {
            return Err(Win32Error::InvalidProcessHandle {
                context: "ShellExecuteExW(runas)",
            });
        }

        // SAFETY: process is the handle returned by ShellExecuteExW with
        // SEE_MASK_NOCLOSEPROCESS. Waiting does not mutate unrelated state.
        let wait = unsafe { WaitForSingleObject(process, INFINITE) };
        if wait == WAIT_FAILED {
            // SAFETY: process is owned by this function and closed exactly once.
            unsafe {
                let _ = CloseHandle(process);
            }
            return Err(Win32Error::Windows {
                context: "WaitForSingleObject(elevated process)",
                source: windows::core::Error::from_win32(),
            });
        }

        let mut exit_code = 1;
        // SAFETY: process is still a valid process handle after the wait returns,
        // and exit_code points to writable storage.
        unsafe {
            GetExitCodeProcess(process, &mut exit_code).map_err(|source| Win32Error::Windows {
                context: "GetExitCodeProcess(elevated process)",
                source,
            })?;
            let _ = CloseHandle(process);
        }

        Ok(exit_code)
    }

    pub fn install_elevated_daemon_task(daemon: &Path) -> Result<()> {
        let xml = elevated_daemon_task_xml(daemon);
        let xml_path = temporary_task_xml_path();
        fs::write(&xml_path, xml).map_err(|source| Win32Error::ShellOperation {
            operation: "write elevated task XML",
            message: source.to_string(),
        })?;

        let args = vec![
            "/Create".to_owned(),
            "/TN".to_owned(),
            ELEVATED_DAEMON_TASK_NAME.to_owned(),
            "/XML".to_owned(),
            xml_path.display().to_string(),
            "/F".to_owned(),
        ];
        let result = launch_elevated_process_and_wait(Path::new("schtasks.exe"), &args);
        let _ = fs::remove_file(&xml_path);

        match result {
            Ok(0) => Ok(()),
            Ok(code) => Err(Win32Error::ShellOperation {
                operation: "create elevated daemon scheduled task",
                message: format!("schtasks.exe exited with code {code}"),
            }),
            Err(error) => Err(error),
        }
    }

    pub fn uninstall_elevated_daemon_task() -> Result<()> {
        if !elevated_daemon_task_installed()? {
            return Ok(());
        }

        let args = vec![
            "/Delete".to_owned(),
            "/TN".to_owned(),
            ELEVATED_DAEMON_TASK_NAME.to_owned(),
            "/F".to_owned(),
        ];
        match launch_elevated_process_and_wait(Path::new("schtasks.exe"), &args)? {
            0 => Ok(()),
            code => Err(Win32Error::ShellOperation {
                operation: "delete elevated daemon scheduled task",
                message: format!("schtasks.exe exited with code {code}"),
            }),
        }
    }

    pub fn elevated_daemon_task_installed() -> Result<bool> {
        let output = run_schtasks(["/Query", "/TN", ELEVATED_DAEMON_TASK_NAME])?;

        Ok(output.status.success())
    }

    pub fn run_elevated_daemon_task() -> Result<()> {
        let output = run_schtasks(["/Run", "/I", "/TN", ELEVATED_DAEMON_TASK_NAME])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(command_failure(
                "run elevated daemon scheduled task",
                output,
            ))
        }
    }

    pub fn shell_command_for_executable(executable: &Path) -> String {
        quote_windows_arg(&executable.display().to_string())
    }

    pub fn shell_command_with_daemon(
        shell: &Path,
        daemon: Option<&Path>,
        elevated_daemon: bool,
    ) -> String {
        let mut command = shell_command_for_executable(shell);
        if elevated_daemon {
            command.push_str(" --elevated-daemon");
        }
        if let Some(daemon) = daemon {
            command.push_str(" --daemon ");
            command.push_str(&quote_windows_arg(&daemon.display().to_string()));
        }
        command
    }

    pub fn quote_windows_arg(argument: &str) -> String {
        if !argument.is_empty()
            && !argument
                .chars()
                .any(|ch| ch.is_ascii_whitespace() || matches!(ch, '"' | '\\'))
        {
            return argument.to_owned();
        }

        let mut quoted = String::from("\"");
        let mut backslashes = 0;
        for ch in argument.chars() {
            match ch {
                '\\' => backslashes += 1,
                '"' => {
                    quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                    quoted.push('"');
                    backslashes = 0;
                }
                _ => {
                    quoted.push_str(&"\\".repeat(backslashes));
                    backslashes = 0;
                    quoted.push(ch);
                }
            }
        }
        quoted.push_str(&"\\".repeat(backslashes * 2));
        quoted.push('"');
        quoted
    }

    fn run_schtasks<const N: usize>(args: [&str; N]) -> Result<Output> {
        Command::new("schtasks.exe")
            .args(args)
            .output()
            .map_err(|source| Win32Error::ShellOperation {
                operation: "run schtasks.exe",
                message: source.to_string(),
            })
    }

    fn command_failure(operation: &'static str, output: Output) -> Win32Error {
        let code = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            "no output".to_owned()
        };

        Win32Error::ShellOperation {
            operation,
            message: format!("schtasks.exe exited with code {code}: {detail}"),
        }
    }

    fn temporary_task_xml_path() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("winland-elevated-daemon-{nanos}.xml"))
    }

    pub(super) fn elevated_daemon_task_xml(daemon: &Path) -> String {
        let daemon = escape_xml(&daemon.display().to_string());
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts the Winland daemon elevated for experimental shell replacement testing.</Description>
  </RegistrationInfo>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{daemon}</Command>
    </Exec>
  </Actions>
</Task>"#
        )
    }

    fn escape_xml(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    struct RegistryKey(HKEY);

    impl Drop for RegistryKey {
        fn drop(&mut self) {
            // SAFETY: RegistryKey only wraps handles returned by RegOpenKeyExW or
            // RegCreateKeyExW in this module, and Drop runs at most once.
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    fn open_winlogon_key(access: REG_SAM_FLAGS) -> Result<RegistryKey> {
        let subkey = wide_null(USER_WINLOGON_SUBKEY);
        let mut key = HKEY::default();
        // SAFETY: subkey is a null-terminated UTF-16 string that lives for this
        // call. key points to valid writable storage for the opened handle.
        let code = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                0,
                access,
                &mut key,
            )
        };
        registry_result(code, "RegOpenKeyExW(HKCU Winlogon)")?;
        Ok(RegistryKey(key))
    }

    fn create_winlogon_key() -> Result<RegistryKey> {
        let subkey = wide_null(USER_WINLOGON_SUBKEY);
        let mut key = HKEY::default();
        // SAFETY: subkey is a null-terminated UTF-16 string that lives for this
        // call. key points to valid writable storage for the created/opened key.
        let code = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                0,
                PWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
        };
        registry_result(code, "RegCreateKeyExW(HKCU Winlogon)")?;
        Ok(RegistryKey(key))
    }

    fn query_string_value(key: &RegistryKey, name: &str) -> Result<Option<String>> {
        let name = wide_null(name);
        let mut value_type = REG_VALUE_TYPE(0);
        let mut byte_len = 0u32;

        // SAFETY: name is null-terminated and byte_len/value_type are writable.
        // Passing no data buffer asks Win32 for the required buffer size.
        let code = unsafe {
            RegQueryValueExW(
                key.0,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut value_type),
                None,
                Some(&mut byte_len),
            )
        };
        if code == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        registry_result(code, "RegQueryValueExW(size)")?;
        if value_type != REG_SZ {
            return Err(Win32Error::UnexpectedRegistryValueType {
                name: wide_to_string_without_nul(&name),
                expected: "REG_SZ",
                actual: value_type.0,
            });
        }

        let mut bytes = vec![0u8; byte_len as usize];
        // SAFETY: bytes is allocated to the exact length Win32 requested above.
        let code = unsafe {
            RegQueryValueExW(
                key.0,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut value_type),
                Some(bytes.as_mut_ptr()),
                Some(&mut byte_len),
            )
        };
        registry_result(code, "RegQueryValueExW(data)")?;

        let u16_len = byte_len as usize / 2;
        let words =
            // SAFETY: REG_SZ data is UTF-16. RegQueryValueExW reported an even
            // byte count for this value, and bytes lives for the resulting slice.
            unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const u16, u16_len) };
        Ok(Some(wide_to_string_without_nul(words)))
    }

    fn set_string_value(key: &RegistryKey, name: &str, value: &str) -> Result<()> {
        let name = wide_null(name);
        let value = wide_null(value);
        let bytes =
            // SAFETY: value is a live UTF-16 buffer. Reinterpreting it as bytes is
            // the representation RegSetValueExW expects for REG_SZ data.
            unsafe {
                std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2)
            };

        // SAFETY: name is null-terminated, bytes points to REG_SZ UTF-16 data and
        // remains valid for the duration of this call.
        let code = unsafe { RegSetValueExW(key.0, PCWSTR(name.as_ptr()), 0, REG_SZ, Some(bytes)) };
        registry_result(code, "RegSetValueExW")
    }

    fn delete_value_if_present(key: &RegistryKey, name: &str) -> Result<()> {
        let name = wide_null(name);
        // SAFETY: name is a null-terminated UTF-16 value name for this registry key.
        let code = unsafe { RegDeleteValueW(key.0, PCWSTR(name.as_ptr())) };
        if code == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            registry_result(code, "RegDeleteValueW")
        }
    }

    fn launch_process(command_line: &str) -> Result<()> {
        let mut command_line = wide_null(command_line);
        let startup_info = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        // SAFETY: PROCESS_INFORMATION is a plain Win32 output struct that is
        // fully initialized by CreateProcessW on success.
        let mut process_info: PROCESS_INFORMATION = unsafe { zeroed() };

        // SAFETY: command_line is a mutable null-terminated UTF-16 buffer because
        // CreateProcessW may write into it during parsing. All security,
        // inheritance, environment, and directory pointers are null/default.
        unsafe {
            CreateProcessW(
                PCWSTR(null()),
                PWSTR(command_line.as_mut_ptr()),
                None,
                None,
                false,
                CREATE_UNICODE_ENVIRONMENT,
                None,
                PCWSTR(null()),
                &startup_info,
                &mut process_info,
            )
            .map_err(|source| Win32Error::Windows {
                context: "CreateProcessW",
                source,
            })?;
        }

        // SAFETY: CreateProcessW initialized these handles on success, and this
        // helper does not need to retain ownership after the child is launched.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(process_info.hProcess);
            let _ = windows::Win32::Foundation::CloseHandle(process_info.hThread);
        }

        Ok(())
    }

    fn registry_result(
        code: windows::Win32::Foundation::WIN32_ERROR,
        context: &'static str,
    ) -> Result<()> {
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(Win32Error::Registry {
                context,
                code: code.0,
            })
        }
    }

    fn shell_command_looks_like_winland(shell: &str) -> bool {
        let lower = shell.to_ascii_lowercase();
        lower.contains("winland-shell")
            || lower.contains("winland-daemon") && lower.contains("--shell-mode")
    }

    fn wide_null(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn wide_to_string_without_nul(value: &[u16]) -> String {
        let end = value.iter().position(|ch| *ch == 0).unwrap_or(value.len());
        String::from_utf16_lossy(&value[..end])
    }
}

#[cfg(not(windows))]
mod platform {
    use std::path::Path;

    use crate::{Result, Win32Error};

    pub const USER_WINLOGON_KEY: &str =
        r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\Winlogon";

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ShellReplacementStatus {
        pub registry_key: &'static str,
        pub current_shell: Option<String>,
        pub backup_shell: Option<String>,
        pub backup_shell_was_present: Option<bool>,
        pub is_winland_shell: bool,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ShellReplacementChange {
        pub registry_key: &'static str,
        pub previous_shell: Option<String>,
        pub new_shell: Option<String>,
    }

    pub fn shell_replacement_status() -> Result<ShellReplacementStatus> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn install_shell_replacement(_shell_command: &str) -> Result<ShellReplacementChange> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn restore_shell_replacement() -> Result<ShellReplacementChange> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn launch_explorer() -> Result<()> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn launch_shell_test(_shell_command: &str) -> Result<()> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn launch_elevated_process_and_wait(
        _executable: &Path,
        _arguments: &[String],
    ) -> Result<u32> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn install_elevated_daemon_task(_daemon: &Path) -> Result<()> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn uninstall_elevated_daemon_task() -> Result<()> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn elevated_daemon_task_installed() -> Result<bool> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn run_elevated_daemon_task() -> Result<()> {
        Err(Win32Error::UnsupportedPlatform)
    }

    pub fn shell_command_for_executable(executable: &Path) -> String {
        quote_windows_arg(&executable.display().to_string())
    }

    pub fn shell_command_with_daemon(
        shell: &Path,
        daemon: Option<&Path>,
        elevated_daemon: bool,
    ) -> String {
        let mut command = shell_command_for_executable(shell);
        if elevated_daemon {
            command.push_str(" --elevated-daemon");
        }
        if let Some(daemon) = daemon {
            command.push_str(" --daemon ");
            command.push_str(&quote_windows_arg(&daemon.display().to_string()));
        }
        command
    }

    pub fn quote_windows_arg(argument: &str) -> String {
        if !argument.is_empty()
            && !argument
                .chars()
                .any(|ch| ch.is_ascii_whitespace() || matches!(ch, '"' | '\\'))
        {
            return argument.to_owned();
        }

        let mut quoted = String::from("\"");
        let mut backslashes = 0;
        for ch in argument.chars() {
            match ch {
                '\\' => backslashes += 1,
                '"' => {
                    quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                    quoted.push('"');
                    backslashes = 0;
                }
                _ => {
                    quoted.push_str(&"\\".repeat(backslashes));
                    backslashes = 0;
                    quoted.push(ch);
                }
            }
        }
        quoted.push_str(&"\\".repeat(backslashes * 2));
        quoted.push('"');
        quoted
    }
}

pub use platform::ShellReplacementChange;
pub use platform::ShellReplacementStatus;
pub use platform::USER_WINLOGON_KEY;
pub use platform::elevated_daemon_task_installed;
pub use platform::install_elevated_daemon_task;
pub use platform::install_shell_replacement;
pub use platform::launch_elevated_process_and_wait;
pub use platform::launch_explorer;
pub use platform::launch_shell_test;
pub use platform::quote_windows_arg;
pub use platform::restore_shell_replacement;
pub use platform::run_elevated_daemon_task;
pub use platform::shell_command_for_executable;
pub use platform::shell_command_with_daemon;
pub use platform::shell_replacement_status;
pub use platform::uninstall_elevated_daemon_task;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn shell_command_quotes_shell_path() {
        let command =
            shell_command_for_executable(Path::new(r"C:\Program Files\Winland\winland-shell.exe"));

        assert_eq!(command, r#""C:\Program Files\Winland\winland-shell.exe""#);
    }

    #[test]
    fn shell_command_quotes_optional_daemon_path() {
        let command = shell_command_with_daemon(
            Path::new(r"C:\Program Files\Winland\winland-shell.exe"),
            Some(Path::new(r"C:\Tools\winland-daemon.exe")),
            false,
        );

        assert_eq!(
            command,
            r#""C:\Program Files\Winland\winland-shell.exe" --daemon "C:\Tools\winland-daemon.exe""#
        );
    }

    #[test]
    fn shell_command_includes_elevated_daemon_flag_before_daemon_path() {
        let command = shell_command_with_daemon(
            Path::new(r"C:\Program Files\Winland\winland-shell.exe"),
            Some(Path::new(r"C:\Tools\winland-daemon.exe")),
            true,
        );

        assert_eq!(
            command,
            r#""C:\Program Files\Winland\winland-shell.exe" --elevated-daemon --daemon "C:\Tools\winland-daemon.exe""#
        );
    }

    #[test]
    fn quote_windows_arg_escapes_embedded_quotes() {
        assert_eq!(
            quote_windows_arg(r#"C:\Tools\a"b.exe"#),
            r#""C:\Tools\a\"b.exe""#
        );
    }

    #[test]
    fn elevated_task_xml_escapes_daemon_path() {
        let xml = platform::elevated_daemon_task_xml(Path::new(r"C:\A&B\winland-daemon.exe"));

        assert!(xml.contains(r"C:\A&amp;B\winland-daemon.exe"));
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<AllowStartOnDemand>true</AllowStartOnDemand>"));
    }
}
