//! `trix-probe explore`: Claude tests trix on its own, with the probe's tools,
//! and writes a report.
//!
//! It calls the Messages API over HTTP (Rust has no official Anthropic SDK).
//! The conversation is append-only, so prompt caching and thinking blocks
//! stay valid: old screenshots are cleared by the API's own context editing
//! (`clear_tool_uses_20250919`), never by editing history here. Budget
//! reminders are text blocks appended after the tool results.

use crate::probe::{Probe, clip};
use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

pub struct ExploreOpts {
    pub model: String,
    pub effort: String,
    pub focus: Vec<String>,
    pub max_steps: usize,
    pub max_usd: f64,
    pub docs: Option<PathBuf>,
    pub fallbacks: bool,
}

fn api_url() -> String {
    let base = std::env::var("ANTHROPIC_BASE_URL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "https://api.anthropic.com".into());
    format!("{}/v1/messages", base.trim_end_matches('/'))
}

const SYSTEM: &str = r#"You are an expert QA engineer and UX reviewer testing **trix**, a terminal workspace where several coding agents (Claude Code, Codex, shells, …) run side by side. You use trix through tools that act exactly like a person at a keyboard and mouse: you see the screen, press keys, type, click, resize the window, close it, open a second terminal. Your job is to find real bugs and real UX problems, and report each one clearly.

## How your tools work
- `launch` starts a fresh trix in an isolated sandbox (its own HOME and config; the user's real trix is never touched). Every result shows the screen as text with row numbers (rows and columns from 0, the coordinates `click` takes), or as a PNG when you ask for one ("png"/"both"). Use PNGs to judge colors, contrast, alignment and layout; use text to read exactly.
- Each result ends with what a person would have noticed since the last action: agent state changes, desktop notifications, sounds, bells, clipboard copies, the window title, server log errors. Treat these as part of what you observe.
- Agents are FAKE by default. A fake agent behaves like Claude Code and fires trix's real hooks. Type a prompt into its pane and press Enter. Words in the prompt pick what it does: `permission` (asks to run rm -rf dist and waits for 1/2/3: trix should show needs-you), `question` (asks which database), `long` (30 s of work), `flood` (3000 lines), `unicode`, `silent` (no hooks, so trix has to guess from the screen), `context 85`, `title <text>`, `bell`, `crash`, `exit`, `help`. Anything else is a short normal turn. Esc interrupts. `fake_agent` does the same from outside, for agents you aren't looking at.
- `run_trix` runs trix's command line (ls, ls --json, spawn, send, read, wait, close, events, layout, config, kill), the way scripts and other agents use it.
- The PNG renderer may draw some emoji or rare symbols as outlined boxes: that's the probe, not trix. When unsure whether something is trix's fault or the harness's, say so (confidence "unsure").

## How to test
1. Start where a new user starts: launch with no args, read the first screen, find the help (alt+?), open the new-agent form (alt+n), start an agent.
2. Then work through features systematically, checking the documentation's claims against what you actually see: the needs-you flow (an agent you're NOT looking at asks for permission: strip, alt+a, notification, sound), done/unseen states, the grid (alt+g), search (alt+f), broadcast (alt+b), rename, pin, close, the agent menu, the settings (alt+,) and every row in them, themes (try terminal_colors light too), detach (alt+d) and reattach, viewers (open_terminal with ["view"]), restore after kill_server, layouts, config.toml mistakes and live reload, the CLI guard rails (send to an agent that needs you, close a working agent).
3. Push on edges a real user hits: small windows (80x24, 60x15) and resizing while a modal is open; long names and long prompts; unicode; many agents (8+); rapid key presses; clicking every visible button; esc at every step; an agent crashing; output floods; double actions (pressing a key twice).
4. For every screen, ask: Is it clear what's going on and what to do next? Is anything cut off, overlapping, misaligned, unreadable, inconsistent with the rest of trix, or out of step with the docs? Is the feedback immediate? Would a new user get stuck?

## Reporting
- Use `report_issue` once per distinct problem, as soon as you're confident. Reproduce it a second time when you can, and say how sure you are. Write steps someone can follow by hand, and say what you expected and what happened.
- Severity: critical (data loss, crash, hang, a security guard bypassed), high (a feature doesn't work, or a user can't recover), medium (works, but confusing, wrong in a visible way, or contradicts the docs), low (polish).
- Don't report the same thing twice, and don't report things that are clearly working as designed and documented. UX opinions are welcome when they're specific and actionable.
- Keep going until your action budget runs out, then call `finish` with a summary of what you tested and what you didn't get to."#;

fn auth() -> Result<(String, String, bool)> {
    if let Ok(k) = std::env::var("ANTHROPIC_API_KEY")
        && !k.is_empty()
    {
        return Ok(("x-api-key".into(), k, false));
    }
    if let Ok(t) = std::env::var("ANTHROPIC_AUTH_TOKEN")
        && !t.is_empty()
    {
        return Ok(("authorization".into(), format!("Bearer {t}"), false));
    }
    // An `ant auth login` profile.
    if let Ok(o) = std::process::Command::new("ant").args(["auth", "print-credentials", "--access-token"]).output()
        && o.status.success()
    {
        let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !t.is_empty() {
            return Ok(("authorization".into(), format!("Bearer {t}"), true));
        }
    }
    bail!("no Claude API credentials: set ANTHROPIC_API_KEY, or run `ant auth login`")
}

/// $ per million tokens: (input, output, cache write 5m, cache read).
fn prices(model: &str) -> Option<(f64, f64, f64, f64)> {
    match model {
        "claude-opus-5-5" => Some((4.0, 20.0, 5.0, 0.20)),
        "claude-sonnet-5-5" => Some((2.0, 10.0, 2.5, 0.10)),
        "claude-fable-5-2" => Some((8.0, 40.0, 10.0, 0.20)),
        "claude-haiku-5-5" => Some((0.10, 0.50, 0.125, 0.01)),
        _ => None,
    }
}

#[derive(Default)]
struct Usage {
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
}

impl Usage {
    fn add(&mut self, u: &Value) {
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        self.input += n("input_tokens");
        self.output += n("output_tokens");
        self.cache_write += n("cache_creation_input_tokens");
        self.cache_read += n("cache_read_input_tokens");
    }
    fn usd(&self, model: &str) -> Option<f64> {
        let (i, o, w, r) = prices(model)?;
        Some((self.input as f64 * i + self.output as f64 * o + self.cache_write as f64 * w + self.cache_read as f64 * r) / 1e6)
    }
}

pub fn run(mut probe: Probe, opts: ExploreOpts) -> Result<()> {
    let (auth_header, auth_value, oauth) = auth()?;
    let out = probe.cfg.out.clone();
    std::fs::create_dir_all(&out)?;
    let mut transcript = std::fs::File::create(out.join("transcript.jsonl"))?;

    // What trix says about itself.
    let docs = match &opts.docs {
        Some(p) => std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
        None => {
            let help = std::process::Command::new(&probe.cfg.trix).arg("--help").output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
            format!("(Only `trix --help` is available; pass --docs README.md for the full documentation.)\n\n{help}")
        }
    };
    let system = format!("{SYSTEM}\n\n## trix version under test\n{}\n\n## trix's documentation\n<docs>\n{docs}\n</docs>", probe.trix_version());

    let mut tools = crate::tools::definitions();
    tools.push(json!({
        "name": "finish",
        "description": "End the test session: a summary of what you tested, what you found, and what you didn't get to.",
        "input_schema": {"type": "object", "properties": {"summary": {"type": "string"}}, "required": ["summary"]},
    }));

    let focus = if opts.focus.is_empty() {
        "Explore broadly: first-run experience, then each documented feature, then edge cases.".to_string()
    } else {
        format!("Focus on: {}. Test other things only as needed to reach these.", opts.focus.join("; "))
    };
    let mut messages: Vec<Value> = vec![json!({"role": "user", "content": [{"type": "text", "text": format!(
        "Test trix now. {focus}\n\nYou have {} actions (tool calls). Start with `launch`.", opts.max_steps
    )}]})];

    let mut betas = vec!["context-management-2025-06-27"];
    if opts.fallbacks {
        betas.push("server-side-fallback-2026-07-01");
    }
    if oauth {
        betas.push("oauth-2025-04-20");
    }
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(600)).build();
    let mut usage = Usage::default();
    let mut steps = 0usize;
    let mut summary: Option<String> = None;
    let mut nudges = 0;
    let mut grace = 0;

    eprintln!("trix-probe explore: {} at effort {}, up to {} actions and ${:.2}; output in {}", opts.model, opts.effort, opts.max_steps, opts.max_usd, out.display());
    'outer: loop {
        let mut body = json!({
            "model": opts.model,
            "max_tokens": 16000,
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": opts.effort},
            "system": [{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}],
            "tools": tools,
            "messages": messages,
            "cache_control": {"type": "ephemeral"},
            "context_management": {"edits": [{
                "type": "clear_tool_uses_20250919",
                "trigger": {"type": "input_tokens", "value": 120000},
                "keep": {"type": "tool_uses", "value": 10},
                "clear_at_least": {"type": "input_tokens", "value": 20000},
                "exclude_tools": ["report_issue", "finish"],
            }]},
        });
        if opts.fallbacks {
            body["fallbacks"] = json!("default");
        }
        let resp = send(&agent, &auth_header, &auth_value, &betas.join(","), &body)?;
        usage.add(resp.get("usage").unwrap_or(&Value::Null));
        let _ = writeln!(transcript, "{}", json!({"response": strip_images(&resp)}));

        let content = resp.get("content").cloned().unwrap_or(json!([]));
        let stop = resp.get("stop_reason").and_then(Value::as_str).unwrap_or("").to_string();
        for block in content.as_array().into_iter().flatten() {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => eprintln!("\n\x1b[36mclaude:\x1b[0m {}", block["text"].as_str().unwrap_or("").trim()),
                Some("fallback") => eprintln!("(a fallback model continued this turn)"),
                _ => {}
            }
        }
        // Append the turn exactly as it came back.
        messages.push(json!({"role": "assistant", "content": content}));

        match stop.as_str() {
            "tool_use" => {}
            "refusal" => {
                eprintln!("the model declined to continue: {}", resp.get("stop_details").map(|d| d.to_string()).unwrap_or_default());
                break;
            }
            "pause_turn" => continue,
            "max_tokens" => {
                messages.push(json!({"role": "user", "content": [{"type": "text", "text": "(Your reply hit the length limit. Continue, in shorter steps.)"}]}));
                continue;
            }
            _ => {
                if summary.is_some() || nudges >= 2 {
                    break;
                }
                nudges += 1;
                messages.push(json!({"role": "user", "content": [{"type": "text", "text": format!(
                    "Keep testing: you have {} actions left. Call `finish` when you're done.", opts.max_steps.saturating_sub(steps)
                )}]}));
                continue;
            }
        }

        let mut results = Vec::new();
        for block in content.as_array().into_iter().flatten() {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let id = block["id"].as_str().unwrap_or("").to_string();
            let name = block["name"].as_str().unwrap_or("").to_string();
            let input = block.get("input").cloned().unwrap_or(json!({}));
            if name == "finish" {
                summary = Some(input["summary"].as_str().unwrap_or("").to_string());
                results.push(json!({"type": "tool_result", "tool_use_id": id, "content": "Session finished. Thank you."}));
                continue;
            }
            steps += 1;
            eprintln!("\x1b[2m[{steps}/{}] {name} {}\x1b[0m", opts.max_steps, clip(&input.to_string(), 160));
            let r = crate::tools::call(&mut probe, &name, &input);
            if name == "report_issue" && !r.is_error {
                eprintln!("\x1b[33m{}\x1b[0m", r.text.lines().next().unwrap_or(""));
            }
            let mut parts = vec![json!({"type": "text", "text": r.text})];
            for png in &r.images {
                let _ = save_png(&out, png);
                parts.push(json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": base64::engine::general_purpose::STANDARD.encode(png)}}));
            }
            let mut result = json!({"type": "tool_result", "tool_use_id": id, "content": parts});
            if r.is_error {
                result["is_error"] = json!(true);
            }
            results.push(result);
        }
        if summary.is_some() {
            messages.push(json!({"role": "user", "content": results}));
            break 'outer;
        }

        let spent = usage.usd(&opts.model);
        let left = opts.max_steps.saturating_sub(steps);
        let over_money = spent.is_some_and(|s| s >= opts.max_usd);
        if left == 0 || over_money {
            grace += 1;
            if grace > 6 {
                eprintln!("out of budget; stopping.");
                break;
            }
            results.push(json!({"type": "text", "text": format!(
                "BUDGET: you're out of {}. Report anything you found but haven't reported yet, then call `finish`.", if over_money { "money" } else { "actions" }
            )}));
        } else if left == 10 || left == 25 {
            results.push(json!({"type": "text", "text": format!("Budget: {left} actions left.")}));
        }
        messages.push(json!({"role": "user", "content": results}));
    }

    probe.write_report()?;
    let mut report = std::fs::read_to_string(out.join("report.md")).unwrap_or_default();
    report += &format!(
        "\n## Session\n\n- model: {} (effort {})\n- actions: {steps}\n- tokens: {} in, {} cache write, {} cache read, {} out{}\n",
        opts.model,
        opts.effort,
        usage.input,
        usage.cache_write,
        usage.cache_read,
        usage.output,
        usage.usd(&opts.model).map(|u| format!("\n- cost: about ${u:.2}")).unwrap_or_default()
    );
    if let Some(s) = &summary {
        report += &format!("\n## Tester's summary\n\n{s}\n");
    }
    std::fs::write(out.join("report.md"), &report)?;
    probe.teardown();
    eprintln!(
        "\ndone: {} issue(s), {steps} actions{}. Report: {}",
        probe.issues().len(),
        usage.usd(&opts.model).map(|u| format!(", about ${u:.2}")).unwrap_or_default(),
        out.join("report.md").display()
    );
    Ok(())
}

fn send(agent: &ureq::Agent, auth_header: &str, auth_value: &str, betas: &str, body: &Value) -> Result<Value> {
    let mut delay = Duration::from_secs(2);
    for attempt in 0..6 {
        let r = agent
            .post(&api_url())
            .set(auth_header, auth_value)
            .set("anthropic-version", "2026-10-21")
            .set("anthropic-beta", betas)
            .set("content-type", "application/json")
            .send_json(body.clone());
        match r {
            Ok(resp) => return Ok(resp.into_json()?),
            Err(ureq::Error::Status(code, resp)) => {
                let retry_after = resp.header("retry-after").and_then(|s| s.parse::<u64>().ok());
                let text = resp.into_string().unwrap_or_default();
                if matches!(code, 408 | 409 | 429 | 500 | 502 | 503 | 504 | 529) && attempt < 5 {
                    let wait = retry_after.map(Duration::from_secs).unwrap_or(delay);
                    eprintln!("API {code}, retrying in {}s: {}", wait.as_secs(), clip(&text, 200));
                    std::thread::sleep(wait);
                    delay *= 2;
                    continue;
                }
                bail!("Claude API error {code}: {text}");
            }
            Err(e) if attempt < 5 => {
                eprintln!("network error, retrying: {e}");
                std::thread::sleep(delay);
                delay *= 2;
            }
            Err(e) => return Err(e.into()),
        }
    }
    bail!("Claude API: gave up after retries")
}

fn save_png(out: &std::path::Path, png: &[u8]) -> Result<PathBuf> {
    let dir = out.join("screens");
    std::fs::create_dir_all(&dir)?;
    let n = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0) + 1;
    let p = dir.join(format!("{n:04}.png"));
    std::fs::write(&p, png)?;
    Ok(p)
}

/// The response without base64 image data, for the transcript.
fn strip_images(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            if o.get("type").and_then(Value::as_str) == Some("base64") {
                return json!({"type": "base64", "data": "<omitted>"});
            }
            Value::Object(o.iter().map(|(k, v)| (k.clone(), strip_images(v))).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(strip_images).collect()),
        other => other.clone(),
    }
}
