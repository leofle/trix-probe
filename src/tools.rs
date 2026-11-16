//! The tools a model uses to drive trix, shared by the MCP server and the
//! autonomous runner: their schemas, and dispatch onto a `Probe`.

use crate::probe::{Probe, Shot, ToolOut, clip};
use anyhow::Context;
use serde_json::{Value, json};

const TERMINAL: &str = "Which terminal (default \"main\", the one launch opened).";
const SHOT: &str = "What to return after the action: \"text\" (default: the screen as text with row numbers; when only a few rows changed since you last saw it, just those rows), \"full\" (the whole screen as text), \"png\" (an image in real colors), \"both\" (text and png), or \"none\".";

fn shot() -> Value {
    json!({"type": "string", "enum": ["text", "full", "png", "both", "none"], "description": SHOT})
}
fn term() -> Value {
    json!({"type": "string", "description": TERMINAL})
}
fn wait() -> Value {
    json!({"type": "integer", "description": "How long to let the screen settle before looking, in ms (default 1500; it returns early once the screen is still for 250 ms, the clock and spinners aside)."})
}

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "launch",
            "description": "Start a fresh, isolated trix in a virtual terminal and show its first screen. Any previous session is torn down first. The sandbox has its own HOME, config and runtime folders, so the user's real trix and agents are never touched. By default every agent kind (claude, codex, …) is a scriptable FAKE agent: it fires trix's real hooks, so trix shows working / needs-you / done just as with a real one. Type `help` into a fake agent's pane to see the words that make it ask for permission, ask a question, work for long, flood output, crash, and so on. Agents start in a demo git repo with an uncommitted change. Desktop notifications, sounds and clipboard copies are captured, not delivered; they show up in each result under 'since the last action'.",
            "input_schema": {"type": "object", "properties": {
                "args": {"type": "array", "items": {"type": "string"}, "description": "Arguments to trix, e.g. [] for an empty workspace, [\"claude\", \"shell\"] to open those agents, [\"-s\", \"work\"] for a named session."},
                "cols": {"type": "integer", "description": "Terminal width (default 140)."},
                "rows": {"type": "integer", "description": "Terminal height (default 40)."},
                "agents": {"type": "string", "enum": ["fake", "real"], "description": "fake (default) or real agents from PATH with the real HOME (costs tokens, uses the user's accounts; only when asked)."},
                "terminal_colors": {"type": "string", "enum": ["dark", "light"], "description": "The outer terminal's own color scheme (default dark). Try light to catch UI that only reads well on dark."},
                "keyboard": {"type": "string", "enum": ["legacy", "kitty"], "description": "legacy (default: alt is an ESC prefix, like Terminal.app/iTerm2) or kitty (the kitty keyboard protocol, like Ghostty/kitty/WezTerm, an approximation)."},
                "config_toml": {"type": "string", "description": "Contents for ~/.config/trix/config.toml (optional). A [notify] block that logs notifications is added unless you write your own."},
                "env": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Extra environment variables for trix, e.g. {\"TRIX_THEME\": \"…\"}."},
                "cwd": {"type": "string", "description": "Folder to start trix in, relative to the sandbox (default work/demo)."},
                "trix": {"type": "string", "description": "The trix binary to test in this session, e.g. ~/Projects/trix/target/release/trix for a branch you just built (default: the one trix-probe was started with). The result names the binary and its version; reports and repros record it."},
                "screenshot": shot(),
            }},
        }),
        json!({
            "name": "screen",
            "description": "Look at the screen now, without doing anything: always the whole screen. Default format both (text and image).",
            "input_schema": {"type": "object", "properties": {"terminal": term(), "format": shot()}},
        }),
        json!({
            "name": "press",
            "description": "Press keys, one after another, as a person would. Key names: alt+n, ctrl+c, enter, esc, tab, shift+tab, backspace, up/down/left/right, home, end, pgup, pgdn, delete, f1..f12, space, alt+, (comma), alt+? , alt+plus, ctrl+down, shift+pgup, or a single character like y or 1. Every trix shortcut is alt+something. Returns the screen afterwards.",
            "input_schema": {"type": "object", "properties": {
                "keys": {"type": "array", "items": {"type": "string"}, "description": "Keys in order, e.g. [\"alt+n\"] or [\"down\", \"down\", \"enter\"]."},
                "delay_ms": {"type": "integer", "description": "Pause between keys (default 80)."},
                "terminal": term(), "screenshot": shot(), "wait_ms": wait(),
            }, "required": ["keys"]},
        }),
        json!({
            "name": "type_text",
            "description": "Type text into whatever has focus (an agent's prompt, a form field, a search box), character by character. Newlines press Enter. paste: true sends it as one bracketed paste instead, like pasting from the clipboard.",
            "input_schema": {"type": "object", "properties": {
                "text": {"type": "string"},
                "enter": {"type": "boolean", "description": "Press Enter afterwards (default false)."},
                "paste": {"type": "boolean"},
                "terminal": term(), "screenshot": shot(), "wait_ms": wait(),
            }, "required": ["text"]},
        }),
        json!({
            "name": "click",
            "description": "Click with the mouse at a row and column (from 0, as the text screen numbers them), or on text shown on screen (its middle). trix's bottom-bar keys, pins, sidebar cards and the needs-you strip are clickable.",
            "input_schema": {"type": "object", "properties": {
                "row": {"type": "integer"}, "col": {"type": "integer"},
                "text": {"type": "string", "description": "Click this text instead of row/col."},
                "nth": {"type": "integer", "description": "Which match of text, from 0 (default 0)."},
                "button": {"type": "string", "enum": ["left", "right", "middle"]},
                "double": {"type": "boolean", "description": "Double-click."},
                "modifiers": {"type": "array", "items": {"type": "string", "enum": ["shift", "alt", "ctrl"]}},
                "terminal": term(), "screenshot": shot(), "wait_ms": wait(),
            }},
        }),
        json!({
            "name": "drag",
            "description": "Press the left button at one place, move to another, release: selecting text, for instance. Give from_row/from_col or from_text, and to_row/to_col or to_text.",
            "input_schema": {"type": "object", "properties": {
                "from_row": {"type": "integer"}, "from_col": {"type": "integer"}, "from_text": {"type": "string"},
                "to_row": {"type": "integer"}, "to_col": {"type": "integer"}, "to_text": {"type": "string"},
                "terminal": term(), "screenshot": shot(), "wait_ms": wait(),
            }},
        }),
        json!({
            "name": "scroll",
            "description": "Turn the mouse wheel over a place on screen (row/col or text).",
            "input_schema": {"type": "object", "properties": {
                "row": {"type": "integer"}, "col": {"type": "integer"}, "text": {"type": "string"},
                "direction": {"type": "string", "enum": ["up", "down"]},
                "amount": {"type": "integer", "description": "Wheel steps (default 3)."},
                "terminal": term(), "screenshot": shot(), "wait_ms": wait(),
            }, "required": ["direction"]},
        }),
        json!({
            "name": "resize",
            "description": "Resize the terminal window, as dragging its corner would. Try small sizes (80x24, 60x15) and very wide ones.",
            "input_schema": {"type": "object", "properties": {
                "cols": {"type": "integer"}, "rows": {"type": "integer"},
                "terminal": term(), "screenshot": shot(), "wait_ms": wait(),
            }, "required": ["cols", "rows"]},
        }),
        json!({
            "name": "sweep",
            "description": "Look at each case at each size, in this session, in one call: for every case and size it resizes, presses the case's keys (and types its text), looks, then presses reset (default esc) to close it again. Returns every screen as text (or contact-sheet PNGs, one per case) and saves the sheets. Use it for layout at many sizes, e.g. every dialog from 24 rows down to 5. Each screen is checked for text drawn over a border, and for a dialog drawing outside its box (text that changed outside it when it opened); the result says which screens have problems. Inside compare, each of its screens is compared on its own.",
            "input_schema": {"type": "object", "properties": {
                "sizes": {"type": "array", "items": {"type": "string"}, "description": "Sizes as COLSxROWS, e.g. [\"80x24\", \"80x9\", \"80x6\", \"50x12\"]."},
                "cases": {"type": "array", "description": "What to open at each size. Leave out to look at the screen as it is.", "items": {"type": "object", "properties": {
                    "name": {"type": "string", "description": "A label, e.g. \"broadcast\" (default: the keys)."},
                    "keys": {"type": "array", "items": {"type": "string"}, "description": "Keys that open it, as press takes them, e.g. [\"alt+b\"]."},
                    "text": {"type": "string", "description": "Text to type after the keys, e.g. a message to see it in place."},
                    "reset": {"type": "array", "items": {"type": "string"}, "description": "Keys that close it again (default [\"esc\"]; [] for none)."},
                }}},
                "check_layout": {"type": "boolean", "description": "Fail (an error result) when any screen has a layout problem, for regressions (default false: only report them)."},
                "resize_while_open": {"type": "boolean", "description": "Open each case once at the first size, then resize it through the rest (default false: open it fresh at each size). Resizing an open dialog finds different bugs."},
                "wait_ms": {"type": "integer", "description": "How long each screen may take to settle (default 800; it moves on once the screen is still for 250 ms)."},
                "terminal": term(), "screenshot": shot(),
            }, "required": ["sizes"]},
        }),
        json!({
            "name": "batch",
            "description": "Run several actions in one call, in order, and look once at the end: for steps you already know (open a form, fill it in, submit it; send three prompts). Each step is a tool call as {\"tool\": \"press\", \"args\": {…}}. The steps' own screens are skipped, but what each reports is kept: a wait_for's result, find_text's matches, run_trix's output, and what a person would have noticed. It stops at the first step that fails (stop_on_error: false goes on). Each step goes into the repro on its own.",
            "input_schema": {"type": "object", "properties": {
                "steps": {"type": "array", "items": {"type": "object"}, "description": "The tool calls in order, e.g. [{\"tool\": \"press\", \"args\": {\"keys\": [\"alt+n\"]}}, {\"tool\": \"type_text\", \"args\": {\"text\": \"review\", \"enter\": true}}, {\"tool\": \"wait_for\", \"args\": {\"text\": \"done\"}}]."},
                "stop_on_error": {"type": "boolean", "description": "Stop at the first step that fails (default true)."},
                "terminal": {"type": "string", "description": "Which terminal to show at the end (default \"main\")."},
                "screenshot": shot(),
            }, "required": ["steps"]},
        }),
        json!({
            "name": "compare",
            "description": "Run the same steps on two trix binaries, one after the other in matching fresh sandboxes, and show where their screens and observations differ: before/after a fix, a branch against main. Steps are tool calls ({\"tool\": \"press\", \"args\": {…}}) starting with launch; leave them out to replay what this session has done since its launch. Clock, durations, spinners, sparklines and fake-agent ids are masked so only real differences show. Your own session is left as it is. Before/after images of each differing screen are saved.",
            "input_schema": {"type": "object", "properties": {
                "after": {"type": "string", "description": "The trix binary to set against before, e.g. ~/Projects/trix/target/release/trix."},
                "before": {"type": "string", "description": "The baseline trix binary (default: the one trix-probe was started with)."},
                "steps": {"type": "array", "items": {"type": "object"}, "description": "The tool calls to run on both, starting with launch, e.g. [{\"tool\": \"launch\", \"args\": {\"args\": [\"claude\", \"shell\"], \"cols\": 80, \"rows\": 24}}, {\"tool\": \"sweep\", \"args\": {\"sizes\": [\"80x9\", \"80x6\"], \"cases\": [{\"keys\": [\"alt+b\"]}]}}]."},
                "repro": {"type": "string", "description": "A repro.json (from report_issue) to run instead of steps."},
                "ignore": {"type": "array", "items": {"type": "string"}, "description": "More regexes to mask on both sides before comparing."},
                "raw": {"type": "boolean", "description": "Compare without the default masks."},
                "show": {"type": "string", "enum": ["diff", "all"], "description": "diff (default): only the lines that differ; all: also both whole screens of each that differs."},
                "screenshot": {"type": "string", "enum": ["text", "png", "both"], "description": "png or both: also return before/after images of the first 4 screens that differ."},
            }, "required": ["after"]},
        }),
        json!({
            "name": "set_focus",
            "description": "Tell trix its terminal window gained or lost focus (the user switched to another app). trix notifies differently when unfocused.",
            "input_schema": {"type": "object", "properties": {"focused": {"type": "boolean"}, "terminal": term()}, "required": ["focused"]},
        }),
        json!({
            "name": "wait_for",
            "description": "Wait until text (or a regex) appears on screen, or with gone: true until it disappears. Use it instead of guessing sleep times, e.g. wait for an agent to finish.",
            "input_schema": {"type": "object", "properties": {
                "text": {"type": "string"}, "regex": {"type": "string"},
                "gone": {"type": "boolean"},
                "timeout_ms": {"type": "integer", "description": "Default 10000, at most 120000."},
                "terminal": term(), "screenshot": shot(),
            }},
        }),
        json!({
            "name": "expect",
            "description": "Check that text (or a regex) is on screen, or with absent: true that it isn't, giving the screen up to timeout_ms to get there. A failed check fails the step and shows the screen, so a replay of the repro fails: use it to turn a bug into a regression test (e.g. after the steps that showed it, expect what should be there).",
            "input_schema": {"type": "object", "properties": {
                "text": {"type": "string"}, "regex": {"type": "string"},
                "absent": {"type": "boolean", "description": "Check that it's NOT on screen (default false)."},
                "row": {"type": "integer", "description": "Look only at this row (from 0, as the text screen numbers them)."},
                "timeout_ms": {"type": "integer", "description": "How long the screen may take to get there (default 3000, at most 120000)."},
                "why": {"type": "string", "description": "What this checks, shown with the result, e.g. \"pane titles keep the name\"."},
                "terminal": term(), "screenshot": shot(),
            }},
        }),
        json!({
            "name": "sleep",
            "description": "Do nothing for a while (time passing matters: timers, 'time in state', the clock, animations), then look.",
            "input_schema": {"type": "object", "properties": {"ms": {"type": "integer"}, "terminal": term(), "screenshot": shot()}, "required": ["ms"]},
        }),
        json!({
            "name": "find_text",
            "description": "Where text appears on screen: row and column of each match.",
            "input_schema": {"type": "object", "properties": {"text": {"type": "string"}, "terminal": term()}, "required": ["text"]},
        }),
        json!({
            "name": "open_terminal",
            "description": "Open another terminal window on the same sandbox, running trix with these args: [\"view\"] to watch read-only, [] or [\"attach\"] to attach (e.g. after closing the main one), [\"-s\", \"work\"] for another session. The new window starts focused.",
            "input_schema": {"type": "object", "properties": {
                "name": {"type": "string", "description": "A name for it, used as `terminal` in other tools."},
                "args": {"type": "array", "items": {"type": "string"}},
                "cols": {"type": "integer"}, "rows": {"type": "integer"}, "screenshot": shot(),
            }, "required": ["name"]},
        }),
        json!({
            "name": "close_terminal",
            "description": "Close a terminal window: how \"hangup\" (default) is closing the window or losing SSH, \"kill\" is the terminal app crashing. The trix server and its agents should keep running; reattach with open_terminal.",
            "input_schema": {"type": "object", "properties": {"name": {"type": "string"}, "how": {"type": "string", "enum": ["hangup", "kill"]}}},
        }),
        json!({
            "name": "run_trix",
            "description": "Run a trix command in the sandbox, outside the terminal, and get its exit code and output: ls, ls --json, spawn, send, read, wait, close, events, layout save/open/ls/rm, config, kill. This is how scripts and other agents use trix (see the docs' Agent control).",
            "input_schema": {"type": "object", "properties": {
                "args": {"type": "array", "items": {"type": "string"}, "description": "e.g. [\"spawn\", \"claude\", \"--name\", \"api\", \"--prompt\", \"permission\"]"},
                "stdin": {"type": "string"},
                "timeout_ms": {"type": "integer", "description": "Default 15000."},
            }, "required": ["args"]},
        }),
        json!({
            "name": "kill_server",
            "description": "Crash the trix server (SIGKILL by default, or a polite TERM), to test what the terminal shows and whether the next trix offers to restore the workspace.",
            "input_schema": {"type": "object", "properties": {"signal": {"type": "string", "enum": ["KILL", "TERM"]}}},
        }),
        json!({
            "name": "read_file",
            "description": "Read a file, or list a folder, inside the sandbox: relative paths start at the sandbox root (xdg/config/trix/config.toml, xdg/state/trix/workspace.json, work/demo, run/trix/server.log), and ~ is the sandbox HOME.",
            "input_schema": {"type": "object", "properties": {"path": {"type": "string"}, "max_bytes": {"type": "integer"}}, "required": ["path"]},
        }),
        json!({
            "name": "write_file",
            "description": "Write (or append to) a file inside the sandbox, e.g. edit xdg/config/trix/config.toml to test live reload and config errors, or create folders for agents. For trix's config.toml it waits for trix's reload message (\"config reloaded\" or the error, up to 2.5 s) and says what it was. Returns the screen afterwards.",
            "input_schema": {"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}, "append": {"type": "boolean"}}, "required": ["path", "content"]},
        }),
        json!({
            "name": "fake_agent",
            "description": "Make a fake agent do something now, from outside, by its name or number in the sidebar: turn (a normal short turn; text = its prompt), permission (asks to run rm -rf dist and waits: needs-you), question (asks which database: needs-you), long (~30 s of work), flood (3000 lines), unicode, silent (a turn with no hooks), crash (exit 1), exit, answer (answer an open dialog; key \"1\", \"3\", \"enter\" or \"esc\"), interrupt, output (print text, no hooks), title (set the terminal title to text), bell, context (set the context gauge: percent), hook (fire one raw hook: event, payload), status. You can also do all of this by typing into the agent's pane, as a person would.",
            "input_schema": {"type": "object", "properties": {
                "agent": {"type": "string", "description": "Its name (e.g. \"claude\" or a renamed one) or its number."},
                "action": {"type": "string", "enum": ["turn", "permission", "question", "long", "flood", "unicode", "silent", "crash", "exit", "answer", "interrupt", "output", "title", "bell", "context", "hook", "status", "help"]},
                "text": {"type": "string"}, "key": {"type": "string"}, "percent": {"type": "number"},
                "event": {"type": "string"}, "payload": {"type": "object"},
                "screenshot": shot(), "wait_ms": wait(),
            }, "required": ["agent", "action"]},
        }),
        json!({
            "name": "observations",
            "description": "Everything recorded so far, in full: agent state changes (from trix events), notifications, sounds, clipboard copies, bells and titles, the server log, and the hooks fake agents fired.",
            "input_schema": {"type": "object", "properties": {
                "what": {"type": "string", "enum": ["all", "events", "notifications", "sounds", "clipboard", "bells", "server_log", "fake_logs"]},
                "last": {"type": "integer", "description": "Lines per section (default 30)."},
            }},
        }),
        json!({
            "name": "report_issue",
            "description": "Record a bug or UX problem you found. The current screen (text and image) and every action since launch (a replayable repro script) are saved with it. Report each distinct problem once, as soon as you've confirmed it; reproduce it a second time first when you can.",
            "input_schema": {"type": "object", "properties": {
                "kind": {"type": "string", "enum": ["bug", "ux", "crash", "performance", "docs", "accessibility"], "description": "docs: the app disagrees with its documentation."},
                "severity": {"type": "string", "enum": ["critical", "high", "medium", "low"]},
                "confidence": {"type": "string", "enum": ["confirmed", "likely", "unsure"], "description": "confirmed: reproduced twice; unsure: might be a quirk of this test harness."},
                "title": {"type": "string", "description": "One line: what's wrong."},
                "steps": {"type": "array", "items": {"type": "string"}, "description": "Steps from launch, specific enough to follow by hand."},
                "expected": {"type": "string"}, "actual": {"type": "string"},
                "notes": {"type": "string", "description": "Why it matters, a guess at the cause, a workaround."},
                "terminal": term(),
            }, "required": ["kind", "severity", "title", "steps", "expected", "actual"]},
        }),
        json!({
            "name": "teardown",
            "description": "Stop trix and remove the sandbox. launch does this for you; call it at the end.",
            "input_schema": {"type": "object", "properties": {}},
        }),
    ]
}

/// Tools whose calls are part of a repro script.
fn is_action(name: &str) -> bool {
    // compare runs its own sandboxes and leaves this session alone.
    // A batch's steps are logged one by one, as they run.
    !matches!(name, "screen" | "find_text" | "observations" | "report_issue" | "read_file" | "teardown" | "compare" | "batch")
}

/// Several tool calls in one: each step's report without its screen, then
/// the screen once at the end.
fn batch(p: &mut Probe, a: &Value) -> anyhow::Result<ToolOut> {
    let steps = a.get("steps").and_then(Value::as_array).context("steps is required, e.g. [{\"tool\": \"press\", \"args\": {\"keys\": [\"alt+n\"]}}]")?;
    anyhow::ensure!(!steps.is_empty(), "steps is empty: give at least one tool call");
    for (i, step) in steps.iter().enumerate() {
        let tool = step.get("tool").and_then(Value::as_str).with_context(|| format!("step {} has no \"tool\"", i + 1))?;
        anyhow::ensure!(tool != "batch", "step {}: a batch can't hold another batch", i + 1);
    }
    let go_on = a.get("stop_on_error").and_then(Value::as_bool) == Some(false);
    let mut text = String::new();
    let mut failed = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let tool = step["tool"].as_str().unwrap_or("");
        let given = step.get("args").cloned().unwrap_or(json!({}));
        let mut args = given.clone();
        if let Some(o) = args.as_object_mut() {
            o.insert("screenshot".into(), json!("none"));
            if tool == "screen" {
                o.insert("format".into(), json!("none"));
            }
        }
        let out = call(p, tool, &args);
        text += &format!("── {}/{} {tool} {}\n", i + 1, steps.len(), clip(&given.to_string(), 160));
        for l in out.text.trim().lines() {
            text += &format!("   {l}\n");
        }
        if out.is_error {
            failed.push(i + 1);
            if !go_on {
                if i + 1 < steps.len() {
                    text += &format!("stopped: step {} failed, so the {} after it didn't run.\n", i + 1, steps.len() - i - 1);
                }
                break;
            }
        }
    }
    let shot = Shot::parse(a.get("screenshot"), Shot::Text);
    let mut out = match p.screen_out(a.get("terminal").and_then(Value::as_str), shot) {
        Ok(end) => {
            let mut end = end;
            end.text = format!("{text}── the screen after the batch\n{}", end.text);
            end
        }
        Err(e) => ToolOut::text(format!("{text}── no screen to show at the end: {e:#}")),
    };
    if !failed.is_empty() {
        out.is_error = true;
        out.text = format!("step(s) {failed:?} failed\n{}", out.text);
    }
    Ok(out)
}

pub fn call(p: &mut Probe, name: &str, args: &Value) -> ToolOut {
    let args = if args.is_null() { json!({}) } else { args.clone() };
    let r = match name {
        "launch" => p.launch(&args),
        "screen" => p.screen(&args),
        "press" => p.press(&args),
        "type_text" => p.type_text(&args),
        "click" => p.click(&args),
        "drag" => p.drag(&args),
        "scroll" => p.scroll(&args),
        "resize" => p.resize(&args),
        "sweep" => p.sweep(&args),
        "compare" => crate::compare::compare(p, &args),
        "batch" => batch(p, &args),
        "set_focus" => p.set_focus(&args),
        "wait_for" => p.wait_for(&args),
        "expect" => p.expect(&args),
        "sleep" => p.sleep(&args),
        "find_text" => p.find_text(&args),
        "open_terminal" => p.open_terminal(&args),
        "close_terminal" => p.close_terminal(&args),
        "run_trix" => p.run_trix(&args),
        "kill_server" => p.kill_server(&args),
        "read_file" => p.read_file(&args),
        "write_file" => p.write_file(&args),
        "fake_agent" => p.fake_agent(&args),
        "observations" => p.observations(&args),
        "report_issue" => p.report_issue(&args),
        "teardown" => {
            p.teardown();
            Ok(ToolOut::text("stopped trix and removed the sandbox"))
        }
        other => Err(anyhow::anyhow!("unknown tool '{other}'")),
    };
    if is_action(name) {
        // launch starts a new script; a failed launch leaves none.
        if name == "launch" {
            p.actions.clear();
        }
        p.actions.push(json!({"tool": name, "args": args}));
    }
    r.unwrap_or_else(|e| ToolOut::error(format!("error: {e:#}")))
}
