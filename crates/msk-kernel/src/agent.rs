//! Session/turn engine. A session owns the model provider, tool registry,
//! context, artifact store, and event sink across many user turns. A failed
//! turn ends that turn, not the session.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use msk_core::context::{ContextLimits, ContextManager, Role};
use msk_core::{PolicyDecision, ToolCall};
use msk_data::ObjectStore;
use msk_events::{Event, EventSink};
use msk_llm::{LlmDelta, LlmProvider};
use serde_json::{Value, json};

use crate::runtime::{
    RegisteredTool, build_observation, complete_or_stream, extract_data, llm_request, substitute,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    AwaitingInput,
    Interrupted,
    Stopped(String),
    Error(String),
}

impl TurnOutcome {
    pub fn status(&self) -> &'static str {
        match self {
            Self::AwaitingInput => "awaiting_input",
            Self::Interrupted => "interrupted",
            Self::Stopped(_) => "stopped",
            Self::Error(_) => "error",
        }
    }

    pub fn message(&self) -> Option<&str> {
        match self {
            Self::AwaitingInput | Self::Interrupted => None,
            Self::Stopped(message) | Self::Error(message) => Some(message),
        }
    }
}

/// Cooperative cancellation shared by the stdin reader and the turn engine.
/// A blocking provider or tool call is not aborted mid-call; the engine
/// observes the flag before the next model/tool step and closes the turn with
/// a paired tool-result set so the session remains usable.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn clear(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub id: u64,
    pub tool: String,
    pub args: Value,
    pub reason: String,
}

/// Blocking approval channel. The session calls this from the turn thread; the
/// implementation may wait for IPC, a terminal prompt, or a timeout. Returning
/// false denies the call and the model receives an error observation.
pub trait ApprovalHandler {
    fn request(&mut self, request: &ApprovalRequest, cancel: &CancelToken) -> bool;
}

pub struct SessionOptions {
    pub env_id: String,
    pub registry: HashMap<String, RegisteredTool>,
    pub provider: Box<dyn LlmProvider>,
    pub context: ContextManager,
    pub limits: ContextLimits,
    pub objects: Arc<dyn ObjectStore>,
    pub sink: Arc<dyn EventSink>,
    pub max_steps: usize,
    pub stream: bool,
    pub task: Option<String>,
    pub cancel: CancelToken,
    pub approvals: Option<Box<dyn ApprovalHandler>>,
}

pub struct Session {
    env_id: String,
    registry: HashMap<String, RegisteredTool>,
    provider: Box<dyn LlmProvider>,
    context: ContextManager,
    limits: ContextLimits,
    objects: Arc<dyn ObjectStore>,
    sink: Arc<dyn EventSink>,
    max_steps: usize,
    stream: bool,
    task: Option<String>,
    cancel: CancelToken,
    approvals: Option<Box<dyn ApprovalHandler>>,
    next_approval: u64,
    last_session: Option<String>,
    last_artifact: Option<String>,
    turns: u64,
}

impl Session {
    pub fn new(options: SessionOptions) -> Self {
        Self {
            env_id: options.env_id,
            registry: options.registry,
            provider: options.provider,
            context: options.context,
            limits: options.limits,
            objects: options.objects,
            sink: options.sink,
            max_steps: options.max_steps,
            stream: options.stream,
            task: options.task,
            cancel: options.cancel,
            approvals: options.approvals,
            next_approval: 0,
            last_session: None,
            last_artifact: None,
            turns: 0,
        }
    }

    pub fn clear_cancel(&self) {
        self.cancel.clear();
    }

    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    pub fn turns(&self) -> u64 {
        self.turns
    }

    pub fn context(&self) -> &ContextManager {
        &self.context
    }

    /// Runs one user turn to completion. Tool failures are returned to the
    /// model as observations; provider failures and step exhaustion end only
    /// this turn, so the caller can keep the session alive.
    pub fn run_turn<F: FnMut(&str)>(&mut self, user: &str, mut on_text: F) -> TurnOutcome {
        self.turns += 1;
        self.emit("turn.start", json!({"turn": self.turns, "user": user}));
        self.context.push(Role::User, user.to_string());
        self.compact();

        for _step in 0..self.max_steps {
            if self.cancel.is_cancelled() {
                return self.finish_interrupted();
            }
            let request = llm_request(&self.context, &self.registry);
            let response = match complete_or_stream(
                self.provider.as_ref(),
                &request,
                self.stream,
                &mut |delta| {
                    let LlmDelta::Text(text) = delta;
                    on_text(&text);
                },
            ) {
                Ok(response) => response,
                Err(err) => {
                    let message = format!("LLM call failed: {err}");
                    self.emit(
                        "turn.end",
                        json!({"turn": self.turns, "status": "error", "message": message}),
                    );
                    return TurnOutcome::Error(message);
                }
            };

            // Non-streaming providers return the complete text at once.
            if !self.stream && !response.text.is_empty() {
                on_text(&response.text);
            }
            self.emit(
                "llm.response",
                json!({
                    "text": response.text,
                    "tool_calls": response.tool_calls.len(),
                    "usage_tokens": response.usage_tokens,
                    "streamed": self.stream
                }),
            );

            if self.cancel.is_cancelled() {
                return self.finish_interrupted();
            }
            if response.tool_calls.is_empty() {
                self.context.push(Role::Assistant, response.text);
                self.compact();
                self.emit(
                    "turn.end",
                    json!({"turn": self.turns, "status": "awaiting_input"}),
                );
                return TurnOutcome::AwaitingInput;
            }

            self.context
                .push_assistant_tool_calls(response.text, response.tool_calls.clone());
            let mut interrupted = false;
            for call in response.tool_calls {
                if interrupted || self.cancel.is_cancelled() {
                    interrupted = true;
                    // Every assistant tool call needs a matching result, even
                    // when the user interrupted before the tool ran, or the
                    // next provider request would be invalid.
                    self.push_tool_error(&call, "interrupted by user".into());
                    continue;
                }
                self.run_tool(call);
            }
            if self.cancel.is_cancelled() {
                interrupted = true;
            }
            self.compact();
            if interrupted {
                return self.finish_interrupted();
            }
        }

        let message = format!("turn exceeded {} steps", self.max_steps);
        self.emit(
            "turn.end",
            json!({"turn": self.turns, "status": "stopped", "message": message}),
        );
        TurnOutcome::Stopped(message)
    }

    fn finish_interrupted(&mut self) -> TurnOutcome {
        self.emit(
            "turn.end",
            json!({"turn": self.turns, "status": "interrupted"}),
        );
        TurnOutcome::Interrupted
    }

    fn run_tool(&mut self, call: ToolCall) {
        let mut args = call.args.clone();
        if let Err(err) = substitute(
            &mut args,
            &self.env_id,
            self.last_session.as_deref(),
            self.last_artifact.as_deref(),
        ) {
            self.push_tool_error(&call, format!("invalid arguments: {err}"));
            return;
        }

        let (decision, context) = match self.registry.get(&call.tool) {
            Some(tool) => (tool.decision, tool.context.clone()),
            None => {
                self.emit(
                    "policy.decision",
                    json!({"tool": call.tool, "decision": "deny", "reason": "tool not registered"}),
                );
                self.push_tool_error(&call, format!("tool not registered: {}", call.tool));
                return;
            }
        };
        let input = match serde_json::to_vec(&args) {
            Ok(input) => input,
            Err(err) => {
                self.push_tool_error(&call, format!("invalid tool input: {err}"));
                return;
            }
        };

        self.emit("tool.call", json!({"tool": call.tool, "args": args}));
        self.emit(
            "policy.decision",
            json!({
                "tool": context.tool,
                "granted": context.capabilities,
                "decision": decision.as_str()
            }),
        );

        match decision {
            PolicyDecision::Allow => {}
            PolicyDecision::Deny => {
                self.push_tool_error(&call, format!("policy denied tool {}", call.tool));
                return;
            }
            PolicyDecision::Ask => {
                let id = self.next_approval + 1;
                self.next_approval = id;
                let request = ApprovalRequest {
                    id,
                    tool: call.tool.clone(),
                    args: args.clone(),
                    reason: approval_reason(&call.tool),
                };
                self.emit(
                    "approval.request",
                    json!({
                        "id": request.id,
                        "tool": request.tool,
                        "args": request.args,
                        "reason": request.reason
                    }),
                );
                let approved = match self.approvals.as_mut() {
                    Some(handler) => handler.request(&request, &self.cancel),
                    None => false,
                };
                self.emit("approval.result", json!({"id": id, "approved": approved}));
                if !approved {
                    let reason = if self.cancel.is_cancelled() {
                        "approval interrupted"
                    } else {
                        "approval denied"
                    };
                    self.push_tool_error(&call, reason.into());
                    return;
                }
            }
        }

        let Some(tool) = self.registry.get(&call.tool) else {
            self.push_tool_error(&call, format!("tool disappeared: {}", call.tool));
            return;
        };
        let started = Instant::now();
        let output = match tool.runtime.run_with_context(&tool.context, &input) {
            Ok(output) => output,
            Err(err) => {
                self.push_tool_error(&call, format!("WASM tool failed: {err}"));
                return;
            }
        };
        let duration_ms = started.elapsed().as_millis() as u64;

        let value: Value = match serde_json::from_slice(&output) {
            Ok(value) => value,
            Err(err) => {
                self.push_tool_error(&call, format!("WASM tool returned non-JSON: {err}"));
                return;
            }
        };
        let data = match extract_data(&value) {
            Ok(data) => data,
            Err(err) => {
                self.push_tool_error(&call, err.to_string());
                return;
            }
        };

        let created_session = if call.tool == "session_create" {
            match data.get("session_id").and_then(Value::as_str) {
                Some(session_id) => Some(session_id.to_string()),
                None => {
                    self.push_tool_error(&call, "session_create did not return session_id".into());
                    return;
                }
            }
        } else {
            None
        };

        let (observation, artifact) =
            match build_observation(&call.tool, &data, &self.limits, self.objects.as_ref()) {
                Ok(result) => result,
                Err(err) => {
                    self.push_tool_error(&call, format!("artifact store failed: {err}"));
                    return;
                }
            };
        if let Some(object) = &artifact {
            self.last_artifact = Some(object.id.clone());
            self.emit(
                "data.put",
                json!({
                    "id": object.id,
                    "bytes": object.size,
                    "label": object.label,
                    "hash": object.hash
                }),
            );
        }

        self.emit(
            "tool.result",
            json!({
                "tool": call.tool,
                "ok": true,
                "duration_ms": duration_ms,
                "artifact": artifact.as_ref().map(|object| object.id.clone()),
                "observation": observation
            }),
        );
        self.context
            .push_tool_result(call.id.clone(), call.tool.clone(), observation);

        if let Some(session_id) = created_session {
            self.last_session = Some(session_id.clone());
            self.context
                .push_pinned(Role::System, format!("session_id={session_id}"));
        } else if call.tool == "session_close" {
            self.last_session = None;
            self.context.push_pinned(Role::System, "session_id=closed");
        }
    }

    fn push_tool_error(&mut self, call: &ToolCall, message: String) {
        self.emit("tool.error", json!({"tool": call.tool, "error": message}));
        let observation = json!({"ok": false, "error": message}).to_string();
        self.context
            .push_tool_result(call.id.clone(), call.tool.clone(), observation);
    }

    fn compact(&mut self) {
        let removed = self.context.compact();
        if removed > 0 {
            self.emit(
                "context.compact",
                json!({
                    "removed": removed,
                    "messages": self.context.messages().len(),
                    "approx_tokens": self.context.approx_tokens()
                }),
            );
        }
        self.emit(
            "context.state",
            json!({
                "messages": self.context.messages().len(),
                "approx_tokens": self.context.approx_tokens()
            }),
        );
    }

    fn emit(&self, kind: &str, data: Value) {
        let event = Event::new(kind, "agent", data);
        let event = match &self.task {
            Some(task) => event.with_task(task.clone()),
            None => event,
        };
        if let Err(err) = self.sink.emit(&event) {
            eprintln!("event sink error: {err}");
        }
    }
}

fn approval_reason(tool: &str) -> String {
    match tool {
        "exec" | "session_exec" => "execute a command".to_string(),
        "session_write" => "write input to a terminal session".to_string(),
        "write_file" => "write a file in the environment workspace".to_string(),
        "spawn_agent" => "spawn a subagent with delegated tool access".to_string(),
        _ => "tool requires approval".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use msk_core::ToolCall;
    use msk_data::MemoryObjectStore;
    use msk_events::EventError;
    use msk_llm::{LlmResponse, ScriptedProvider};
    use msk_wasm::{ToolContext, ToolHost, ToolManifest, WasmtimeRuntime};

    struct NullSink;

    impl EventSink for NullSink {
        fn emit(&self, _event: &Event) -> Result<(), EventError> {
            Ok(())
        }
    }

    struct UnusedHost;

    impl ToolHost for UnusedHost {
        fn call(&self, _ctx: &ToolContext, _input: &[u8]) -> Result<Vec<u8>, String> {
            Err("host not used by this test".into())
        }
    }

    fn session_with(responses: Vec<LlmResponse>) -> Session {
        session_with_approval(responses, PolicyDecision::Allow, None)
    }

    fn session_with_approval(
        responses: Vec<LlmResponse>,
        decision: PolicyDecision,
        approvals: Option<Box<dyn ApprovalHandler>>,
    ) -> Session {
        let runtime = WasmtimeRuntime::from_wat(
            Arc::new(UnusedHost),
            r#"(module
                (memory (export "memory") 1)
                (func (export "run") (param i32 i32) (result i64)
                    unreachable))"#,
        )
        .unwrap();
        let manifest = ToolManifest {
            name: "boom".into(),
            description: "always traps".into(),
            input_schema: json!({"type": "object"}),
            capabilities: Vec::new(),
            max_memory_bytes: None,
            timeout_ms: None,
        };
        let mut registry = HashMap::new();
        registry.insert(
            "boom".into(),
            RegisteredTool {
                context: ToolContext::new("boom", Vec::new()),
                runtime,
                manifest,
                decision,
            },
        );

        let limits = ContextLimits::default();
        let mut context = ContextManager::new(limits.clone());
        context.push_pinned(Role::System, "env_id=test");
        Session::new(SessionOptions {
            env_id: "test".into(),
            registry,
            provider: Box::new(ScriptedProvider::new("scripted", responses)),
            context,
            limits,
            objects: Arc::new(MemoryObjectStore::new()),
            sink: Arc::new(NullSink),
            max_steps: 4,
            stream: false,
            task: None,
            cancel: CancelToken::new(),
            approvals,
        })
    }

    #[test]
    fn tool_trap_becomes_an_observation_and_the_turn_recovers() {
        let mut session = session_with(vec![
            LlmResponse {
                text: "try the tool".into(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    tool: "boom".into(),
                    args: json!({}),
                }],
                usage_tokens: 1,
            },
            LlmResponse {
                text: "recovered".into(),
                tool_calls: Vec::new(),
                usage_tokens: 1,
            },
        ]);

        let outcome = session.run_turn("go", |_| {});
        assert_eq!(outcome, TurnOutcome::AwaitingInput);
        assert_eq!(session.turns(), 1);
        assert!(session.context().messages().iter().any(|message| {
            message.tool_call_id.as_deref() == Some("call-1")
                && message.content.contains("WASM tool failed")
        }));
        assert!(
            session
                .context()
                .messages()
                .iter()
                .any(|message| message.content == "recovered")
        );
    }

    struct RecordingApproval {
        approve: bool,
        seen: Arc<std::sync::Mutex<Vec<ApprovalRequest>>>,
    }

    impl ApprovalHandler for RecordingApproval {
        fn request(&mut self, request: &ApprovalRequest, _cancel: &CancelToken) -> bool {
            self.seen.lock().unwrap().push(request.clone());
            self.approve
        }
    }

    fn ask_session(approve: bool) -> (Session, Arc<std::sync::Mutex<Vec<ApprovalRequest>>>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler = Box::new(RecordingApproval {
            approve,
            seen: seen.clone(),
        });
        let session = session_with_approval(
            vec![
                LlmResponse {
                    text: "ask first".into(),
                    tool_calls: vec![ToolCall {
                        id: "a1".into(),
                        tool: "boom".into(),
                        args: json!({"command": "rm -rf /"}),
                    }],
                    usage_tokens: 1,
                },
                LlmResponse {
                    text: "done".into(),
                    tool_calls: Vec::new(),
                    usage_tokens: 1,
                },
            ],
            PolicyDecision::Ask,
            Some(handler),
        );
        (session, seen)
    }

    #[test]
    fn denied_approval_becomes_a_paired_error_observation() {
        let (mut session, seen) = ask_session(false);
        assert_eq!(session.run_turn("go", |_| {}), TurnOutcome::AwaitingInput);
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].tool, "boom");
        assert_eq!(requests[0].args["command"], "rm -rf /");
        drop(requests);
        let messages = session.context().messages();
        assert!(messages.iter().any(|message| {
            message.tool_call_id.as_deref() == Some("a1")
                && message.content.contains("approval denied")
        }));
        assert!(messages.iter().any(|message| message.content == "done"));
    }

    #[test]
    fn approved_approval_reaches_the_tool() {
        let (mut session, seen) = ask_session(true);
        assert_eq!(session.run_turn("go", |_| {}), TurnOutcome::AwaitingInput);
        assert_eq!(seen.lock().unwrap().len(), 1);
        let messages = session.context().messages();
        assert!(messages.iter().any(|message| {
            message.tool_call_id.as_deref() == Some("a1")
                && message.content.contains("WASM tool failed")
        }));
    }

    #[test]
    fn provider_failure_ends_only_the_turn() {
        let mut session = session_with(Vec::new());
        assert!(matches!(
            session.run_turn("one", |_| {}),
            TurnOutcome::Error(_)
        ));
        assert!(matches!(
            session.run_turn("two", |_| {}),
            TurnOutcome::Error(_)
        ));
        assert_eq!(session.turns(), 2);
    }

    #[test]
    fn echo_provider_keeps_context_across_turns() {
        let limits = ContextLimits::default();
        let mut context = ContextManager::new(limits.clone());
        let mut session = Session::new(SessionOptions {
            env_id: "test".into(),
            registry: HashMap::new(),
            provider: Box::new(msk_llm::EchoProvider::new("echo")),
            context: {
                context.push_pinned(Role::System, "system");
                context
            },
            limits,
            objects: Arc::new(MemoryObjectStore::new()),
            sink: Arc::new(NullSink),
            max_steps: 4,
            stream: false,
            task: None,
            cancel: CancelToken::new(),
            approvals: None,
        });
        let mut first = String::new();
        assert_eq!(
            session.run_turn("alpha", |text| first.push_str(text)),
            TurnOutcome::AwaitingInput
        );
        assert_eq!(first, "echo: alpha");
        let mut second = String::new();
        assert_eq!(
            session.run_turn("beta", |text| second.push_str(text)),
            TurnOutcome::AwaitingInput
        );
        assert_eq!(second, "echo: beta");
        assert_eq!(session.turns(), 2);
        assert_eq!(
            session
                .context()
                .messages()
                .iter()
                .filter(|message| message.role == Role::User)
                .count(),
            2
        );
        assert_eq!(session.context().messages().len(), 5);
    }

    #[test]
    fn pre_cancelled_turn_ends_without_calling_provider() {
        let mut session = session_with(Vec::new());
        session.cancel_token().cancel();
        assert_eq!(session.run_turn("stop", |_| {}), TurnOutcome::Interrupted);
        assert_eq!(session.turns(), 1);
        assert_eq!(
            session
                .context()
                .messages()
                .iter()
                .filter(|message| message.role == Role::User)
                .count(),
            1
        );
    }

    struct CancelHost {
        cancel: CancelToken,
    }

    impl ToolHost for CancelHost {
        fn call(&self, _ctx: &ToolContext, input: &[u8]) -> Result<Vec<u8>, String> {
            self.cancel.cancel();
            Ok(input.to_vec())
        }
    }

    fn cancelling_session() -> Session {
        let cancel = CancelToken::new();
        let host: Arc<dyn ToolHost> = Arc::new(CancelHost {
            cancel: cancel.clone(),
        });
        let wat = r#"(module
            (import "msk" "tool_call" (func $tool_call (param i32 i32) (result i64)))
            (memory (export "memory") 1)
            (func (export "run") (param i32 i32) (result i64)
                local.get 0
                local.get 1
                call $tool_call))"#;
        let mut registry = HashMap::new();
        for name in ["first", "second"] {
            let runtime = WasmtimeRuntime::from_wat(host.clone(), wat).unwrap();
            registry.insert(
                name.to_string(),
                RegisteredTool {
                    context: ToolContext::new(name, Vec::new()),
                    runtime,
                    manifest: ToolManifest {
                        name: name.into(),
                        description: String::new(),
                        input_schema: json!({"type": "object"}),
                        capabilities: Vec::new(),
                        max_memory_bytes: None,
                        timeout_ms: None,
                    },
                    decision: PolicyDecision::Allow,
                },
            );
        }
        let limits = ContextLimits::default();
        Session::new(SessionOptions {
            env_id: "test".into(),
            registry,
            provider: Box::new(ScriptedProvider::new(
                "scripted",
                vec![LlmResponse {
                    text: "run both".into(),
                    tool_calls: vec![
                        ToolCall {
                            id: "c1".into(),
                            tool: "first".into(),
                            args: json!({}),
                        },
                        ToolCall {
                            id: "c2".into(),
                            tool: "second".into(),
                            args: json!({}),
                        },
                    ],
                    usage_tokens: 1,
                }],
            )),
            context: ContextManager::new(limits.clone()),
            limits,
            objects: Arc::new(MemoryObjectStore::new()),
            sink: Arc::new(NullSink),
            max_steps: 4,
            stream: false,
            task: None,
            cancel: cancel.clone(),
            approvals: None,
        })
    }

    #[test]
    fn interrupt_keeps_tool_call_result_pairing() {
        let mut session = cancelling_session();
        assert_eq!(session.run_turn("go", |_| {}), TurnOutcome::Interrupted);
        let messages = session.context().messages();
        for call_id in ["c1", "c2"] {
            assert!(
                messages
                    .iter()
                    .any(|message| message.tool_call_id.as_deref() == Some(call_id)),
                "missing tool result for {call_id}"
            );
        }
        assert!(messages.iter().any(|message| {
            message.tool_call_id.as_deref() == Some("c2") && message.content.contains("interrupted")
        }));
    }
}
