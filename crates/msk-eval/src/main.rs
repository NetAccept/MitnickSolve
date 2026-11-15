//! Scenario evaluator for a running `mskd`.
//!
//! A suite is a JSON document of headless tasks. The evaluator creates each
//! task, waits for a terminal state, and checks the transcript, turn/event
//! counts, and shared blackboard. It never links the kernel: all access is the
//! normal frontend IPC.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use msk_ipc::{Client, methods};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Parser)]
struct Cli {
    /// Kernel state directory containing the IPC token.
    #[arg(long, default_value = ".msk")]
    root: PathBuf,
    /// Suite JSON file.
    #[arg(long, default_value = "evals/core.json")]
    suite: PathBuf,
    /// Per-case timeout.
    #[arg(long, default_value_t = 120_000)]
    timeout_ms: u64,
    /// Emit one JSON report instead of human-readable lines.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Deserialize)]
struct Suite {
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    goal: String,
    #[serde(default = "default_provider")]
    provider: String,
    #[serde(default = "default_demo")]
    demo: String,
    #[serde(default = "default_approvals")]
    approvals: String,
    max_context_tokens: Option<u64>,
    max_steps: Option<u64>,
    #[serde(default)]
    expect: Expect,
}

#[derive(Debug, Deserialize)]
struct Expect {
    #[serde(default = "default_state")]
    state: String,
    #[serde(default)]
    transcript_contains: Vec<String>,
    #[serde(default)]
    transcript_not_contains: Vec<String>,
    min_turns: Option<u64>,
    max_turns: Option<u64>,
    min_events: Option<u64>,
    #[serde(default)]
    blackboard: Vec<BlackboardCheck>,
}

impl Default for Expect {
    fn default() -> Self {
        Self {
            state: default_state(),
            transcript_contains: Vec::new(),
            transcript_not_contains: Vec::new(),
            min_turns: None,
            max_turns: None,
            min_events: None,
            blackboard: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct BlackboardCheck {
    key_prefix: String,
    value_contains: String,
}

fn default_provider() -> String {
    "scripted".into()
}

fn default_demo() -> String {
    "session".into()
}

fn default_approvals() -> String {
    "allow".into()
}

fn default_state() -> String {
    "done".into()
}

struct CaseReport {
    name: String,
    ok: bool,
    state: String,
    turns: u64,
    events: u64,
    failures: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let text = std::fs::read_to_string(&cli.suite)
        .with_context(|| format!("read suite {}", cli.suite.display()))?;
    let suite: Suite = serde_json::from_str(&text).context("parse suite")?;
    if suite.cases.is_empty() {
        bail!("suite has no cases");
    }
    let mut seen = std::collections::HashSet::new();
    for case in &suite.cases {
        if case.name.trim().is_empty() {
            bail!("case name must not be empty");
        }
        if !seen.insert(case.name.as_str()) {
            bail!("duplicate case name: {}", case.name);
        }
    }

    let mut client = Client::connect(&cli.root)
        .with_context(|| format!("connect to mskd at {}", cli.root.display()))?;
    client.hello().context("kernel.hello")?;

    let timeout = Duration::from_millis(cli.timeout_ms.max(1));
    let mut reports = Vec::new();
    for case in &suite.cases {
        let report = run_case(&mut client, case, timeout);
        if !cli.json {
            if report.ok {
                println!(
                    "PASS {} state={} turns={} events={}",
                    report.name, report.state, report.turns, report.events
                );
            } else {
                println!(
                    "FAIL {} state={} turns={} events={}",
                    report.name, report.state, report.turns, report.events
                );
                for failure in &report.failures {
                    println!("  - {failure}");
                }
            }
        }
        reports.push(report);
    }
    let passed = reports.iter().filter(|report| report.ok).count();
    let failed = reports.len() - passed;
    if cli.json {
        let value = json!({
            "ok": failed == 0,
            "passed": passed,
            "failed": failed,
            "cases": reports.iter().map(|report| json!({
                "name": report.name,
                "ok": report.ok,
                "state": report.state,
                "turns": report.turns,
                "events": report.events,
                "failures": report.failures
            })).collect::<Vec<_>>()
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{passed} passed, {failed} failed");
    }
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn run_case(client: &mut Client, case: &Case, timeout: Duration) -> CaseReport {
    match execute(client, case, timeout) {
        Ok(report) => report,
        Err(error) => CaseReport {
            name: case.name.clone(),
            ok: false,
            state: "error".into(),
            turns: 0,
            events: 0,
            failures: vec![error.to_string()],
        },
    }
}

fn execute(client: &mut Client, case: &Case, timeout: Duration) -> Result<CaseReport> {
    let mut params = json!({
        "goal": case.goal,
        "provider": case.provider,
        "demo": case.demo,
        "interactive": false,
        "headless": true,
        "stream": false,
        "approvals": case.approvals
    });
    if let Some(tokens) = case.max_context_tokens {
        params["max_context_tokens"] = json!(tokens);
    }
    if let Some(steps) = case.max_steps {
        params["max_steps"] = json!(steps);
    }
    let created = client
        .request(methods::TASK_CREATE, params)
        .context("task.create")?;
    let task_id = created
        .get("task_id")
        .and_then(Value::as_str)
        .context("task.create returned no task_id")?
        .to_string();

    let deadline = Instant::now() + timeout;
    let detail = loop {
        let detail = client
            .request(methods::TASK_RESULT, json!({"task_id": task_id}))
            .with_context(|| format!("task.result {task_id}"))?;
        let state = detail
            .get("task")
            .and_then(|task| task.get("state"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if matches!(state, "done" | "failed" | "cancelled") {
            break detail;
        }
        if Instant::now() >= deadline {
            let _ = client.request(methods::TASK_CANCEL, json!({"task_id": task_id}));
            bail!(
                "timed out after {} ms in state {state}",
                timeout.as_millis()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    let task = detail.get("task").unwrap_or(&Value::Null);
    let state = task
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let turns = task.get("turns").and_then(Value::as_u64).unwrap_or(0);
    let events = detail.get("events").and_then(Value::as_u64).unwrap_or(0);
    let transcript = detail
        .get("transcript")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut failures = Vec::new();

    if state != case.expect.state {
        failures.push(format!("expected state {}, got {state}", case.expect.state));
    }
    for needle in &case.expect.transcript_contains {
        if !transcript.contains(needle) {
            failures.push(format!("transcript is missing {needle:?}"));
        }
    }
    for needle in &case.expect.transcript_not_contains {
        if transcript.contains(needle) {
            failures.push(format!("transcript unexpectedly contains {needle:?}"));
        }
    }
    if let Some(minimum) = case.expect.min_turns
        && turns < minimum
    {
        failures.push(format!("expected at least {minimum} turns, got {turns}"));
    }
    if let Some(maximum) = case.expect.max_turns
        && turns > maximum
    {
        failures.push(format!("expected at most {maximum} turns, got {turns}"));
    }
    if let Some(minimum) = case.expect.min_events
        && events < minimum
    {
        failures.push(format!("expected at least {minimum} events, got {events}"));
    }
    if !case.expect.blackboard.is_empty() {
        let board = client
            .request(methods::BLACKBOARD_LIST, json!({}))
            .context("blackboard.list")?;
        let entries = board
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for check in &case.expect.blackboard {
            let matching = entries.iter().find(|entry| {
                entry
                    .get("key")
                    .and_then(Value::as_str)
                    .is_some_and(|key| key.starts_with(&check.key_prefix))
                    && entry
                        .get("value")
                        .is_some_and(|value| value.to_string().contains(&check.value_contains))
            });
            if matching.is_none() {
                failures.push(format!(
                    "blackboard has no {} entry containing {:?}",
                    check.key_prefix, check.value_contains
                ));
            }
        }
    }

    Ok(CaseReport {
        name: case.name.clone(),
        ok: failures.is_empty(),
        state,
        turns,
        events,
        failures,
    })
}
