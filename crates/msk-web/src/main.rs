//! Loopback web client for the kernel daemon.
//!
//! The page is a static shell; every API request opens an authenticated IPC
//! connection to `mskd`, calls the same methods `mskctl` uses, and returns JSON.
//! It is deliberately a frontend client: no kernel state lives here.

use std::io::Read;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result, bail};
use clap::Parser;
use msk_ipc::{Client, methods};
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const MAX_BODY: u64 = 1 << 20;
const UI: &str = include_str!("ui.html");

#[derive(Debug, Parser)]
struct Cli {
    /// Kernel state directory containing the IPC token.
    #[arg(long, default_value = ".msk")]
    root: PathBuf,
    /// Listen address. Non-loopback addresses require --allow-remote.
    #[arg(long, default_value = "127.0.0.1:8787")]
    listen: String,
    /// Bearer token for the browser session (defaults to a random token).
    #[arg(long)]
    token: Option<String>,
    /// Permit binding to a non-loopback address. The token is the only auth.
    #[arg(long)]
    allow_remote: bool,
}

struct App {
    root: PathBuf,
    token: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let address: SocketAddr = cli
        .listen
        .parse()
        .with_context(|| format!("parse listen address {}", cli.listen))?;
    if !address.ip().is_loopback() && !cli.allow_remote {
        bail!("refusing to bind {address} without --allow-remote");
    }
    let root = msk_ipc::normalize_root(&cli.root);
    if !msk_ipc::token_path(&root).exists() {
        bail!(
            "kernel token not found under {} — is mskd running?",
            root.display()
        );
    }
    let token = cli
        .token
        .filter(|token| !token.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let server =
        Arc::new(Server::http(address).map_err(|error| anyhow::anyhow!("listen: {error}"))?);
    let app = Arc::new(App { root, token });

    println!("msk-web listening on http://{address}/");
    println!("open: http://{address}/?token={}", app.token);
    if !address.ip().is_loopback() {
        eprintln!("warning: web UI is reachable beyond this host; the URL token is the only auth");
    }

    let mut workers = Vec::new();
    for _ in 0..4 {
        let server = server.clone();
        let app = app.clone();
        workers.push(thread::spawn(move || {
            loop {
                match server.recv() {
                    Ok(request) => handle(request, &app),
                    Err(error) => {
                        eprintln!("msk-web accept: {error}");
                        break;
                    }
                }
            }
        }));
    }
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

fn handle(mut request: Request, app: &App) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let path = path.to_string();

    if method == Method::Get && path == "/" {
        respond(
            request,
            200,
            UI.to_string(),
            "text/html; charset=utf-8",
            &[
                (
                    "Content-Security-Policy",
                    "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'",
                ),
                ("Referrer-Policy", "no-referrer"),
            ],
        );
        return;
    }
    if method == Method::Get && path == "/favicon.ico" {
        let _ = request.respond(Response::empty(StatusCode(204)));
        return;
    }
    if !path.starts_with("/api/") {
        respond_json(request, 404, &json!({"error": "not found"}));
        return;
    }
    if !authorized(&request, &app.token) {
        respond_json(request, 401, &json!({"error": "unauthorized"}));
        return;
    }

    let body = if method == Method::Post {
        let mut bytes = Vec::new();
        if let Err(error) = request.as_reader().take(MAX_BODY).read_to_end(&mut bytes) {
            respond_json(
                request,
                400,
                &json!({"error": format!("read body: {error}")}),
            );
            return;
        }
        bytes
    } else {
        Vec::new()
    };
    match route(app, &method, &path, query, &body) {
        Ok(value) => respond_json(request, 200, &value),
        Err((status, message)) => respond_json(request, status, &json!({"error": message})),
    }
}

fn authorized(request: &Request, token: &str) -> bool {
    let Some(header) = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Authorization"))
    else {
        return false;
    };
    let value = header.value.as_str();
    let Some(bearer) = value.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(bearer.as_bytes(), token.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

type ApiResult = Result<Value, (u16, String)>;

fn route(app: &App, method: &Method, path: &str, query: &str, body: &[u8]) -> ApiResult {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match (method, segments.as_slice()) {
        (&Method::Get, ["api", "health"]) => Ok(json!({
            "ok": true,
            "root": app.root.display().to_string()
        })),
        (&Method::Get, ["api", "tasks"]) => rpc(app, methods::TASK_LIST, json!({})),
        (&Method::Post, ["api", "tasks"]) => create_task(app, body),
        (&Method::Get, ["api", "tasks", task]) => rpc(
            app,
            methods::TASK_RESULT,
            json!({"task_id": task_id(task)?}),
        ),
        (&Method::Post, ["api", "tasks", task, "message"]) => {
            let body = body_json(body)?;
            let text = body
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| (400, "text is required".to_string()))?;
            rpc(
                app,
                methods::TASK_MESSAGE,
                json!({"task_id": task_id(task)?, "text": text}),
            )
        }
        (&Method::Post, ["api", "tasks", task, "interrupt"]) => rpc(
            app,
            methods::TASK_INTERRUPT,
            json!({"task_id": task_id(task)?}),
        ),
        (&Method::Post, ["api", "tasks", task, "cancel"]) => rpc(
            app,
            methods::TASK_CANCEL,
            json!({"task_id": task_id(task)?}),
        ),
        (&Method::Get, ["api", "approvals"]) => rpc(app, methods::APPROVAL_LIST, json!({})),
        (&Method::Post, ["api", "approvals", task, id]) => {
            let body = body_json(body)?;
            let approved = body
                .get("approved")
                .and_then(Value::as_bool)
                .ok_or_else(|| (400, "approved is required".to_string()))?;
            let id: u64 = id
                .parse()
                .map_err(|_| (400, "invalid approval id".to_string()))?;
            rpc(
                app,
                methods::APPROVAL_RESPOND,
                json!({"task_id": task_id(task)?, "id": id, "approved": approved}),
            )
        }
        (&Method::Get, ["api", "blackboard"]) => rpc(app, methods::BLACKBOARD_LIST, json!({})),
        (&Method::Get, ["api", "events"]) => {
            let mut params = json!({"limit": 200});
            for (key, value) in parse_query(query) {
                match key.as_str() {
                    "task" => params["task_id"] = json!(value),
                    "kind" => params["kind"] = json!(value),
                    "from_seq" => {
                        if let Ok(seq) = value.parse::<u64>() {
                            params["from_seq"] = json!(seq);
                        }
                    }
                    "limit" => {
                        if let Ok(limit) = value.parse::<u64>() {
                            params["limit"] = json!(limit.clamp(1, 1000));
                        }
                    }
                    _ => {}
                }
            }
            rpc(app, methods::EVENTS_QUERY, params)
        }
        _ => Err((404, "not found".to_string())),
    }
}

fn task_id(value: &str) -> Result<&str, (u16, String)> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err((400, "invalid task id".to_string()));
    }
    Ok(value)
}

fn create_task(app: &App, body: &[u8]) -> ApiResult {
    let body = body_json(body)?;
    let goal = body
        .get("goal")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|goal| !goal.is_empty())
        .ok_or_else(|| (400, "goal is required".to_string()))?;
    if goal.len() > 16 * 1024 {
        return Err((400, "goal exceeds 16 KiB".to_string()));
    }
    let provider = body
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("scripted");
    if !matches!(provider, "scripted" | "openai" | "responses" | "component") {
        return Err((400, "unsupported provider".to_string()));
    }
    let interactive = body
        .get("interactive")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let headless = body
        .get("headless")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let approvals = body
        .get("approvals")
        .and_then(Value::as_str)
        .unwrap_or(if interactive { "ask" } else { "allow" });
    let mut params = json!({
        "goal": goal,
        "demo": body.get("demo").and_then(Value::as_str).unwrap_or("session"),
        "provider": provider,
        "interactive": interactive,
        "stream": body.get("stream").and_then(Value::as_bool).unwrap_or(true),
        "headless": headless,
        "approvals": approvals
    });
    if let Some(tokens) = body.get("max_context_tokens").and_then(Value::as_u64) {
        if !(1_000..=10_000_000).contains(&tokens) {
            return Err((
                400,
                "max_context_tokens must be between 1000 and 10000000".into(),
            ));
        }
        params["max_context_tokens"] = json!(tokens);
    }
    for key in ["parent_task", "fork_from"] {
        if let Some(value) = body.get(key).and_then(Value::as_str) {
            task_id(value)?;
            params[key] = json!(value);
        }
    }
    rpc(app, methods::TASK_CREATE, params)
}

fn rpc(app: &App, method: &str, params: Value) -> ApiResult {
    let mut client =
        Client::connect(&app.root).map_err(|error| (502, format!("connect to mskd: {error}")))?;
    client
        .hello()
        .map_err(|error| (502, format!("kernel.hello: {error}")))?;
    client
        .request(method, params)
        .map_err(|error| (502, error.to_string()))
}

fn body_json(body: &[u8]) -> Result<Value, (u16, String)> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(body).map_err(|error| (400, format!("invalid JSON body: {error}")))
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let high = hex(bytes[index + 1]);
                let low = hex(bytes[index + 2]);
                if let (Some(high), Some(low)) = (high, low) {
                    output.push(high << 4 | low);
                    index += 3;
                    continue;
                }
                output.push(bytes[index]);
                index += 1;
            }
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn respond_json(request: Request, status: u16, value: &Value) {
    respond(
        request,
        status,
        serde_json::to_string(value).unwrap_or_else(|_| "{\"error\":\"encode\"}".into()),
        "application/json; charset=utf-8",
        &[
            ("Cache-Control", "no-store"),
            ("X-Content-Type-Options", "nosniff"),
        ],
    );
}

fn respond(
    request: Request,
    status: u16,
    body: String,
    content_type: &str,
    headers: &[(&str, &str)],
) {
    let mut response = Response::from_string(body)
        .with_status_code(StatusCode(status))
        .with_header(header("Content-Type", content_type));
    for (name, value) in headers {
        response = response.with_header(header(name, value));
    }
    let _ = request.respond(response);
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap_or_else(|_| {
        Header::from_bytes(&b"X-Content-Type-Options"[..], &b"nosniff"[..]).unwrap()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison_is_length_checked() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn query_values_are_decoded_but_not_split_again() {
        let parsed = parse_query("task=task-1&kind=turn.end&value=a%2Bb+c");
        assert_eq!(parsed[0], ("task".into(), "task-1".into()));
        assert_eq!(parsed[1], ("kind".into(), "turn.end".into()));
        assert_eq!(parsed[2], ("value".into(), "a+b c".into()));
    }

    #[test]
    fn task_ids_reject_path_traversal() {
        assert!(task_id("task-1").is_ok());
        assert!(task_id("../etc").is_err());
        assert!(task_id("a b").is_err());
        assert!(task_id("").is_err());
    }
}
