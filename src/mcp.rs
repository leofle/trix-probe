//! An MCP server on stdio (newline-delimited JSON-RPC 2.0), so Claude Code or
//! any MCP client can drive trix with the probe's tools:
//!
//! ```sh
//! claude mcp add trix-probe -- trix-probe mcp --trix /path/to/trix
//! ```
//!
//! Only JSON goes to stdout; logs go to stderr.

use crate::probe::Probe;
use base64::Engine;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

pub const INSTRUCTIONS: &str = "trix-probe drives trix (a terminal workspace for coding agents) the way a person would, \
in an isolated sandbox, to find bugs and UX problems. Start with `launch`; every action returns the screen \
(text with row numbers, only the rows that changed when that's few; or a PNG in real colors) plus what a person would have noticed since the last action \
(agent state changes, notifications, sounds, bells, clipboard copies, server log errors). Agents are fake by \
default: type `help` into one, or use `fake_agent`, to make it ask for permission, ask a question, work for long, \
crash, and so on. Record each problem with `report_issue`: it saves a screenshot and a replayable repro. \
To test a particular build, pass its path as `launch`'s `trix`. `sweep` looks at many cases at many sizes in one \
call; `compare` runs the same steps on two binaries and shows what differs (a branch against main). \
Call `teardown` when done.";

pub fn serve(mut probe: Probe) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_msg(&mut stdout, &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}}))?;
                continue;
            }
        };
        // A batch is an array of messages.
        let batch = match msg {
            Value::Array(items) => items,
            one => vec![one],
        };
        let mut replies = Vec::new();
        for m in batch {
            if let Some(r) = handle(&mut probe, &m) {
                replies.push(r);
            }
        }
        match replies.len() {
            0 => {}
            1 => write_msg(&mut stdout, &replies[0])?,
            _ => write_msg(&mut stdout, &Value::Array(replies))?,
        }
    }
    probe.teardown();
    Ok(())
}

fn write_msg(out: &mut std::io::Stdout, v: &Value) -> anyhow::Result<()> {
    let mut lock = out.lock();
    writeln!(lock, "{v}")?;
    lock.flush()?;
    Ok(())
}

fn handle(probe: &mut Probe, m: &Value) -> Option<Value> {
    let method = m.get("method").and_then(Value::as_str)?;
    let id = m.get("id").cloned();
    // Notifications (no id) get no reply.
    let id = id?;
    let ok = |result: Value| Some(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    let err = |code: i64, message: String| Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
    match method {
        "initialize" => {
            let version = m.pointer("/params/protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18");
            ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "trix-probe", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => ok(json!({})),
        "tools/list" => {
            let tools: Vec<Value> = crate::tools::definitions()
                .into_iter()
                .map(|mut t| {
                    let schema = t.as_object_mut().and_then(|o| o.remove("input_schema")).unwrap_or(json!({"type": "object"}));
                    t["inputSchema"] = schema;
                    t
                })
                .collect();
            ok(json!({"tools": tools}))
        }
        "tools/call" => {
            let name = m.pointer("/params/name").and_then(Value::as_str).unwrap_or("");
            let args = m.pointer("/params/arguments").cloned().unwrap_or(json!({}));
            eprintln!("trix-probe: {name} {}", crate::probe::clip(&args.to_string(), 200));
            let out = crate::tools::call(probe, name, &args);
            let mut content = vec![json!({"type": "text", "text": out.text})];
            for png in &out.images {
                content.push(json!({"type": "image", "mimeType": "image/png", "data": base64::engine::general_purpose::STANDARD.encode(png)}));
            }
            ok(json!({"content": content, "isError": out.is_error}))
        }
        "resources/list" => ok(json!({"resources": []})),
        "prompts/list" => ok(json!({"prompts": []})),
        other => err(-32601, format!("method not found: {other}")),
    }
}
