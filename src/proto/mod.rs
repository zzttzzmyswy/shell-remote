#![allow(dead_code)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub session_id: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TerminalInputPayload {
    pub data: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TerminalOutputPayload {
    pub data: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TerminalResizePayload {
    pub cols: u16,
    pub rows: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub size: u64,
    pub mode: String,
    pub owner: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FsResultPayload {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<FileEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpExecPayload {
    pub cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpResultPayload {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecStartPayload {
    pub cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _mcp_request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecInputPayload {
    pub exec_id: String,
    pub data_b64: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _mcp_request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecClosePayload {
    pub exec_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _mcp_request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecListPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _mcp_request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecResultPayload {
    pub exec_id: String,
    pub stdout: String,
    pub stderr: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _mcp_request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecSessionInfo {
    pub exec_id: String,
    pub cmd: String,
    pub status: String,
    pub started_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserInfo {
    pub user_id: String,
    pub permission: Permission,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    ReadWrite,
    ReadOnly,
}

pub const WRITE_TYPES: &[&str] = &[
    "terminal:input",
    "terminal:resize",
    "fs:write",
    "fs:delete",
    "fs:rename",
    "mcp:exec",
    "mcp:exec_start",
    "mcp:exec_input",
    "mcp:exec_close",
    "mcp:exec_list",
    "fs:mkdir",
    "session:tab_create",
    "session:tab_close",
    "session:tab_switch",
];

pub fn requires_write(msg_type: &str) -> bool {
    WRITE_TYPES.contains(&msg_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_roundtrip() {
        let msg = Message {
            msg_type: "terminal:input".to_string(),
            session_id: "abc-123".to_string(),
            payload: serde_json::json!({"data": "aGVsbG8="}),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.msg_type, "terminal:input");
        assert_eq!(decoded.session_id, "abc-123");
        assert_eq!(
            decoded.payload["data"].as_str().unwrap(),
            "aGVsbG8="
        );
    }

    #[test]
    fn test_terminal_output_roundtrip() {
        let output = TerminalOutputPayload {
            data: "SGVsbG8gV29ybGQ=".to_string(),
            tab_id: Some("tab-1".to_string()),
        };
        let msg = Message {
            msg_type: "terminal:output".to_string(),
            session_id: "session-1".to_string(),
            payload: serde_json::to_value(&output).unwrap(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        let decoded_output: TerminalOutputPayload =
            serde_json::from_value(decoded.payload).unwrap();
        assert_eq!(decoded_output.data, "SGVsbG8gV29ybGQ=");
    }

    #[test]
    fn test_fs_result_roundtrip() {
        let entry = FileEntry {
            name: "test.txt".to_string(),
            path: "/home/user/test.txt".to_string(),
            entry_type: "file".to_string(),
            size: 1024,
            mode: "-rw-r--r--".to_string(),
            owner: "1000:1000".to_string(),
        };
        let result = FsResultPayload {
            success: true,
            error: None,
            entries: Some(vec![entry]),
            content: None,
            path: None,
            new_path: None,
        };
        let msg = Message {
            msg_type: "fs:result".to_string(),
            session_id: "session-1".to_string(),
            payload: serde_json::to_value(&result).unwrap(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        let decoded_result: FsResultPayload =
            serde_json::from_value(decoded.payload).unwrap();
        assert!(decoded_result.success);
        assert_eq!(decoded_result.entries.unwrap().len(), 1);
    }

    #[test]
    fn test_requires_write() {
        assert!(requires_write("terminal:input"));
        assert!(requires_write("fs:write"));
        assert!(requires_write("fs:delete"));
        assert!(!requires_write("terminal:output"));
        assert!(!requires_write("session:join"));
        assert!(!requires_write("fs:list"));
        assert!(!requires_write("fs:read"));
    }

    #[test]
    fn test_error_payload_roundtrip() {
        let err = ErrorPayload {
            code: "AUTH_INVALID_TOKEN".to_string(),
            message: "Invalid token".to_string(),
        };
        let msg = Message {
            msg_type: "error".to_string(),
            session_id: "session-1".to_string(),
            payload: serde_json::to_value(&err).unwrap(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        let decoded_err: ErrorPayload =
            serde_json::from_value(decoded.payload).unwrap();
        assert_eq!(decoded_err.code, "AUTH_INVALID_TOKEN");
    }

    #[test]
    fn test_mcp_exec_roundtrip() {
        let exec = McpExecPayload {
            cmd: "ls -la".to_string(),
            timeout_ms: Some(5000),
        };
        let json = serde_json::to_string(&exec).unwrap();
        let decoded: McpExecPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.cmd, "ls -la");
        assert_eq!(decoded.timeout_ms, Some(5000));
    }

    #[test]
    fn test_mcp_result_roundtrip() {
        let result = McpResultPayload {
            stdout: "file.txt".to_string(),
            stderr: String::new(),
            exit_code: 0,
        };
        let json = serde_json::to_string(&result).unwrap();
        let decoded: McpResultPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.stdout, "file.txt");
        assert_eq!(decoded.exit_code, 0);
    }

    #[test]
    fn test_exec_start_roundtrip() {
        let payload = ExecStartPayload {
            cmd: "sudo apt update".to_string(),
            _mcp_request_id: Some("req-1".to_string()),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let decoded: ExecStartPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.cmd, "sudo apt update");
        assert_eq!(decoded._mcp_request_id, Some("req-1".to_string()));
    }

    #[test]
    fn test_exec_result_roundtrip() {
        let payload = ExecResultPayload {
            exec_id: "abc123".to_string(),
            stdout: "output".to_string(),
            stderr: String::new(),
            status: "running".to_string(),
            exit_code: None,
            error: None,
            _mcp_request_id: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let decoded: ExecResultPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.exec_id, "abc123");
        assert_eq!(decoded.status, "running");
        assert_eq!(decoded.exit_code, None);
    }

    #[test]
    fn test_exec_result_exited_roundtrip() {
        let payload = ExecResultPayload {
            exec_id: "abc123".to_string(),
            stdout: "done\n".to_string(),
            stderr: String::new(),
            status: "exited".to_string(),
            exit_code: Some(0),
            error: None,
            _mcp_request_id: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let decoded: ExecResultPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.status, "exited");
        assert_eq!(decoded.exit_code, Some(0));
    }

    #[test]
    fn test_exec_session_info_roundtrip() {
        let info = ExecSessionInfo {
            exec_id: "abc123".to_string(),
            cmd: "sleep 10".to_string(),
            status: "running".to_string(),
            started_at: 1718300000,
        };
        let json = serde_json::to_string(&info).unwrap();
        let decoded: ExecSessionInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.exec_id, "abc123");
        assert_eq!(decoded.cmd, "sleep 10");
        assert_eq!(decoded.status, "running");
    }

    #[test]
    fn test_exec_start_cmd_only() {
        let payload = ExecStartPayload {
            cmd: "ls".to_string(),
            _mcp_request_id: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(!json.contains("_mcp_request_id"));
        let decoded: ExecStartPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.cmd, "ls");
    }
}
