//! Versioned JSON-RPC messages between the kernel daemon and its frontends.
//!
//! Transport is a local Unix domain socket or Windows named pipe. This crate
//! deliberately contains only the wire types.

use serde::{Deserialize, Serialize};

pub const JSONRPC_VERSION: &str = "2.0";
pub const IPC_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

impl RpcRequest {
    pub fn new(id: u64, method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: u64, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

pub mod methods {
    pub const HELLO: &str = "kernel.hello";
    pub const TASK_CREATE: &str = "task.create";
    pub const TASK_CANCEL: &str = "task.cancel";
    pub const TASK_STATE: &str = "task.state";
    pub const APPROVAL_RESPOND: &str = "approval.respond";
    pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";
}

pub mod events {
    pub const TASK_STATE: &str = "task.state";
    pub const TOOL_CALL: &str = "tool.call";
    pub const TOOL_RESULT: &str = "tool.result";
    pub const POLICY_DECISION: &str = "policy.decision";
    pub const APPROVAL_REQUEST: &str = "approval.request";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let req = RpcRequest::new(7, methods::TASK_CREATE, serde_json::json!({"goal": "x"}));
        let text = serde_json::to_string(&req).unwrap();
        let back: RpcRequest = serde_json::from_str(&text).unwrap();
        assert_eq!(back.id, 7);
        assert_eq!(back.method, "task.create");
    }

    #[test]
    fn error_response_omits_result() {
        let res = RpcResponse::err(1, -32601, "method not found");
        let text = serde_json::to_string(&res).unwrap();
        assert!(!text.contains("result"));
        assert!(text.contains("method not found"));
    }
}
