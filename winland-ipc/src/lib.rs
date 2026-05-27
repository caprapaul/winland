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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum IpcCommand {
    State,
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
    Error(IpcError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcError {
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStateSnapshot {
    pub total_windows: usize,
    pub manageable_windows: usize,
    pub floating_windows: usize,
    pub temporary_floating_windows: usize,
    pub active_workspace: u16,
    pub foreground_window: Option<u64>,
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
            total_windows: 4,
            manageable_windows: 3,
            floating_windows: 1,
            temporary_floating_windows: 0,
            active_workspace: 2,
            foreground_window: Some(0x1234),
        };

        let encoded = encode_response(&IpcResponse::state(snapshot.clone())).unwrap();
        let decoded = decode_response(&encoded).unwrap();

        assert_eq!(decoded, IpcResponse::state(snapshot));
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
}
