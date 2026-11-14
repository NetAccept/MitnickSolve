//! Test doubles and conformance helpers. Nothing here is a production tool;
//! production tools are WASM modules only.

use msk_core::{CoreError, Observation, ToolCall, ToolRuntime, ToolStatus};

pub struct EchoToolRuntime;

impl ToolRuntime for EchoToolRuntime {
    fn invoke(&self, call: &ToolCall) -> Result<Observation, CoreError> {
        Ok(Observation {
            call_id: call.id.clone(),
            status: ToolStatus::Ok,
            summary: format!("echo: {}", call.args),
            artifacts: Vec::new(),
            truncated: false,
            cost_micros: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_runtime_returns_the_arguments() {
        let runtime = EchoToolRuntime;
        let call = ToolCall {
            id: "c1".into(),
            tool: "echo".into(),
            args: serde_json::json!({"x": 1}),
        };
        let observation = runtime.invoke(&call).unwrap();
        assert_eq!(observation.status, ToolStatus::Ok);
        assert!(observation.summary.contains("\"x\":1"));
    }
}
