use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcRequest {
    pub protocol_version: u16,
    pub command: IpcCommand,
}

impl IpcRequest {
    pub fn state() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            command: IpcCommand::State,
        }
    }

    pub fn reload_config() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            command: IpcCommand::ReloadConfig,
        }
    }

    pub fn subscribe_state() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            command: IpcCommand::SubscribeState,
        }
    }

    pub fn command(command: impl Into<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            command: IpcCommand::Command {
                command: command.into(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum IpcCommand {
    State,
    ReloadConfig,
    SubscribeState,
    Command { command: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcResponse {
    pub protocol_version: u16,
    pub result: IpcResponseResult,
}

impl IpcResponse {
    pub fn state(snapshot: DaemonStateSnapshot) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            result: IpcResponseResult::State(snapshot),
        }
    }

    pub fn reload_config(report: ReloadConfigReport) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            result: IpcResponseResult::ReloadConfig(report),
        }
    }

    pub fn command(report: CommandReport) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            result: IpcResponseResult::Command(report),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            result: IpcResponseResult::Error(IpcError {
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum IpcResponseResult {
    State(DaemonStateSnapshot),
    ReloadConfig(ReloadConfigReport),
    Command(CommandReport),
    Error(IpcError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcError {
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandReport {
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStateSnapshot {
    pub config_path: Option<String>,
    pub config_version: u64,
    pub config_loaded_at_unix_ms: u64,
    pub total_windows: usize,
    pub manageable_windows: usize,
    pub floating_windows: usize,
    pub temporary_floating_windows: usize,
    pub active_workspace: u16,
    pub foreground_window: Option<u64>,
    pub monitors: Vec<MonitorStateSnapshot>,
    pub windows: Vec<WindowStateSnapshot>,
    pub performance: DaemonPerformanceSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonPerformanceSnapshot {
    pub relayout_count: u64,
    pub skipped_relayout_count: u64,
    pub last_relayout_duration_ms: u64,
    pub last_relayout_move_count: usize,
    pub managed_window_count: usize,
    pub border_window_count: usize,
    pub game_mode_active: bool,
    pub config_reload_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadConfigReport {
    pub config_path: Option<String>,
    pub config_version: u64,
    pub reloaded_at_unix_ms: u64,
    pub changed_sections: Vec<String>,
    pub state: DaemonStateSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorStateSnapshot {
    pub monitor_id: u64,
    pub workspace_id: u16,
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowStateSnapshot {
    pub handle: u64,
    pub title: String,
    pub monitor_id: Option<u64>,
    pub workspace_id: Option<u16>,
    pub focused: bool,
    pub is_minimized: bool,
    pub participation: WindowParticipationSnapshot,
    pub constrained: bool,
    pub visible_on_active_workspace: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowParticipationSnapshot {
    Tiled,
    Floating,
    TemporarilyFloating,
    OverflowFloating,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("invalid IPC JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported IPC protocol version {actual}; expected {expected}")]
    UnsupportedVersion { actual: u16, expected: u16 },
}

pub fn encode_request(request: &IpcRequest) -> Result<Vec<u8>, ProtocolError> {
    ensure_version(request.protocol_version)?;
    encode_json_line(request)
}

pub fn decode_request(input: &[u8]) -> Result<IpcRequest, ProtocolError> {
    let request: IpcRequest = serde_json::from_slice(trim_message(input))?;
    ensure_version(request.protocol_version)?;
    Ok(request)
}

pub fn encode_response(response: &IpcResponse) -> Result<Vec<u8>, ProtocolError> {
    ensure_version(response.protocol_version)?;
    encode_json_line(response)
}

pub fn decode_response(input: &[u8]) -> Result<IpcResponse, ProtocolError> {
    let response: IpcResponse = serde_json::from_slice(trim_message(input))?;
    ensure_version(response.protocol_version)?;
    Ok(response)
}

fn encode_json_line<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    let mut output = serde_json::to_vec(value)?;
    output.push(b'\n');
    Ok(output)
}

fn trim_message(input: &[u8]) -> &[u8] {
    input.trim_ascii_end()
}

fn ensure_version(actual: u16) -> Result<(), ProtocolError> {
    if actual == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ProtocolError::UnsupportedVersion {
            actual,
            expected: PROTOCOL_VERSION,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_request_round_trips_with_version() {
        let encoded = encode_request(&IpcRequest::state()).unwrap();
        let decoded = decode_request(&encoded).unwrap();

        assert_eq!(decoded, IpcRequest::state());
        assert!(encoded.ends_with(b"\n"));
    }

    #[test]
    fn state_response_round_trips() {
        let snapshot = DaemonStateSnapshot {
            config_path: Some(r"C:\Users\me\.config\winland\winland.toml".to_owned()),
            config_version: 3,
            config_loaded_at_unix_ms: 1234,
            total_windows: 4,
            manageable_windows: 3,
            floating_windows: 1,
            temporary_floating_windows: 0,
            active_workspace: 2,
            foreground_window: Some(0x1234),
            monitors: vec![MonitorStateSnapshot {
                monitor_id: 1,
                workspace_id: 2,
                focused: true,
            }],
            windows: vec![WindowStateSnapshot {
                handle: 0x1234,
                title: "Editor".to_owned(),
                monitor_id: Some(1),
                workspace_id: Some(2),
                focused: true,
                is_minimized: false,
                participation: WindowParticipationSnapshot::Tiled,
                constrained: false,
                visible_on_active_workspace: true,
            }],
            performance: performance(),
        };

        let encoded = encode_response(&IpcResponse::state(snapshot.clone())).unwrap();
        let decoded = decode_response(&encoded).unwrap();

        assert_eq!(decoded, IpcResponse::state(snapshot));
    }

    #[test]
    fn state_response_json_uses_stable_kebab_case_tags_and_values() {
        let snapshot = DaemonStateSnapshot {
            config_path: None,
            config_version: 1,
            config_loaded_at_unix_ms: 10,
            total_windows: 1,
            manageable_windows: 1,
            floating_windows: 0,
            temporary_floating_windows: 1,
            active_workspace: 1,
            foreground_window: Some(0xBEEF),
            monitors: vec![MonitorStateSnapshot {
                monitor_id: 7,
                workspace_id: 1,
                focused: true,
            }],
            windows: vec![WindowStateSnapshot {
                handle: 0xBEEF,
                title: "Diagnostics".to_owned(),
                monitor_id: Some(7),
                workspace_id: Some(1),
                focused: true,
                is_minimized: true,
                participation: WindowParticipationSnapshot::TemporarilyFloating,
                constrained: true,
                visible_on_active_workspace: false,
            }],
            performance: performance(),
        };

        let encoded = encode_response(&IpcResponse::state(snapshot)).unwrap();
        let json: serde_json::Value = serde_json::from_slice(encoded.trim_ascii_end()).unwrap();

        assert_eq!(json["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(json["result"]["type"], "state");
        assert_eq!(
            json["result"]["windows"][0]["participation"],
            "temporarily-floating"
        );
        assert_eq!(json["result"]["windows"][0]["constrained"], true);
        assert_eq!(
            json["result"]["windows"][0]["visible_on_active_workspace"],
            false
        );
        assert_eq!(json["result"]["windows"][0]["is_minimized"], true);
    }

    #[test]
    fn reload_config_request_and_response_round_trip() {
        let encoded = encode_request(&IpcRequest::reload_config()).unwrap();
        let decoded = decode_request(&encoded).unwrap();

        assert_eq!(decoded, IpcRequest::reload_config());

        let snapshot = DaemonStateSnapshot {
            config_path: None,
            config_version: 2,
            config_loaded_at_unix_ms: 42,
            total_windows: 0,
            manageable_windows: 0,
            floating_windows: 0,
            temporary_floating_windows: 0,
            active_workspace: 1,
            foreground_window: None,
            monitors: Vec::new(),
            windows: Vec::new(),
            performance: performance(),
        };
        let report = ReloadConfigReport {
            config_path: None,
            config_version: 2,
            reloaded_at_unix_ms: 42,
            changed_sections: vec!["hotkeys".to_owned(), "layout".to_owned()],
            state: snapshot,
        };

        let encoded = encode_response(&IpcResponse::reload_config(report.clone())).unwrap();
        let decoded = decode_response(&encoded).unwrap();

        assert_eq!(decoded, IpcResponse::reload_config(report));
    }

    #[test]
    fn subscribe_state_request_round_trips() {
        let encoded = encode_request(&IpcRequest::subscribe_state()).unwrap();
        let decoded = decode_request(&encoded).unwrap();
        let json: serde_json::Value = serde_json::from_slice(encoded.trim_ascii_end()).unwrap();

        assert_eq!(decoded, IpcRequest::subscribe_state());
        assert_eq!(json["command"]["type"], "subscribe-state");
    }

    #[test]
    fn command_request_and_response_round_trip() {
        let encoded = encode_request(&IpcRequest::command("switch-workspace 2")).unwrap();
        let decoded = decode_request(&encoded).unwrap();
        let json: serde_json::Value = serde_json::from_slice(encoded.trim_ascii_end()).unwrap();

        assert_eq!(decoded, IpcRequest::command("switch-workspace 2"));
        assert_eq!(json["command"]["type"], "command");
        assert_eq!(json["command"]["command"], "switch-workspace 2");

        let report = CommandReport {
            command: "switch-workspace 2".to_owned(),
        };
        let encoded = encode_response(&IpcResponse::command(report.clone())).unwrap();
        let decoded = decode_response(&encoded).unwrap();

        assert_eq!(decoded, IpcResponse::command(report));
    }

    #[test]
    fn unsupported_versions_are_rejected() {
        let input = br#"{"protocol_version":99,"command":{"type":"state"}}"#;

        assert!(matches!(
            decode_request(input),
            Err(ProtocolError::UnsupportedVersion {
                actual: 99,
                expected: PROTOCOL_VERSION,
            })
        ));
    }

    fn performance() -> DaemonPerformanceSnapshot {
        DaemonPerformanceSnapshot {
            relayout_count: 0,
            skipped_relayout_count: 0,
            last_relayout_duration_ms: 0,
            last_relayout_move_count: 0,
            managed_window_count: 0,
            border_window_count: 0,
            game_mode_active: false,
            config_reload_count: 0,
        }
    }
}
