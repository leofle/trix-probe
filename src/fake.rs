//! A fake coding agent, started by trix in place of `claude` (and the other
//! kinds) inside the sandbox. It looks and behaves enough like Claude Code
//! for trix to treat it as one:
//!
//! - it reads the `--settings` overlay trix passes and runs trix's real hook
//!   commands with Claude-shaped payloads (`SessionStart`, `UserPromptSubmit`,
//!   `PreToolUse`, `PermissionRequest`, `Notification`, `PostToolUse`, `Stop`,
//!   `SessionEnd`), so trix's whole hook path is exercised;
//! - it writes a conversation log with token usage, which trix's context
//!   gauge reads;
//! - it draws a prompt box, a spinner while it works, tool lines, and
//!   permission and question dialogs that wait for a key.
//!
//! What a turn does depends on words in the prompt (type `help` in the pane):
//! `permission`, `question`, `long`, `flood`, `unicode`, `silent`, `crash`,
//! `exit`, `context 85`, `title …`, `bell`. Anything else is a short normal
//! turn: Read, Grep, Edit (a real edit, inside the sandbox only), Bash.
//!
//! The probe can also drive it from outside through a unix socket,
//! `$TRIX_PROBE_ROOT/f/<TRIX_AGENT_ID>.sock`: one JSON object per line, e.g.
//! `{"do":"permission"}` or `{"do":"answer","key":"1"}`.

use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Answer {
    Key(char),
    Enter,
    Esc,
}

struct Interrupted;

#[derive(Default)]
struct St {
    buf: String,
    busy: bool,
    pending: Option<mpsc::Sender<Answer>>,
    ctrlc_at: Option<Instant>,
    prompt_shown: bool,
    pasting: bool,
}

struct Agent {
    kind: String,
    name: String,
    hooks: HashMap<String, Vec<String>>,
    session: String,
    transcript: Option<PathBuf>,
    cwd: PathBuf,
    /// Context use in tenths of a percent.
    context: AtomicU32,
    turns: AtomicU32,
    out: Mutex<std::io::Stdout>,
    st: Mutex<St>,
    interrupt: AtomicBool,
    log: Option<PathBuf>,
    sock: Option<PathBuf>,
    registry: Option<PathBuf>,
    orig_termios: Option<libc::termios>,
    sandbox: Option<PathBuf>,
}

pub fn run(kind: String, args: Vec<String>) -> Result<()> {
    let mut settings = None;
    let mut resume = None;
    let mut prompt = None;
    let mut positional_only = false;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if positional_only {
            prompt = Some(a);
            continue;
        }
        match a.as_str() {
            "--" => positional_only = true,
            "--settings" => settings = it.next(),
            "--resume" | "-r" | "--session" | "--session-id" => resume = it.next(),
            "resume" => resume = it.next(), // codex / muse style
            "-c" | "--plugin-dir" | "--model" | "--append-system-prompt" | "--permission-mode" => {
                it.next();
            }
            s if s.starts_with("--session-id=") => resume = Some(s["--session-id=".len()..].to_string()),
            s if s.starts_with("--interactive=") => prompt = Some(s["--interactive=".len()..].to_string()),
            s if s.starts_with('-') => {}
            _ => prompt = Some(a),
        }
    }

    let hooks = match (&settings, kind.as_str()) {
        (Some(p), "claude") => read_hooks(Path::new(p)),
        _ => HashMap::new(),
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let session = resume.clone().unwrap_or_else(new_uuid);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let transcript = (kind == "claude").then(|| home.map(|h| h.join(".claude/projects").join(cwd.display().to_string().replace(['/', '.'], "-")).join(format!("{session}.jsonl")))).flatten();
    let root = std::env::var_os("TRIX_PROBE_ROOT").map(PathBuf::from);
    let id = std::env::var("TRIX_AGENT_ID").ok();
    let (sock, log, registry) = match (&root, &id) {
        (Some(r), Some(id)) => (Some(r.join(format!("f/{id}.sock"))), Some(r.join(format!("f/{id}.log"))), Some(r.join(format!("f/{id}.json")))),
        _ => (None, None, None),
    };

    let agent = Arc::new(Agent {
        kind: kind.clone(),
        name: std::env::var("TRIX_AGENT_NAME").unwrap_or_else(|_| kind.clone()),
        hooks,
        session,
        transcript,
        cwd,
        context: AtomicU32::new(60),
        turns: AtomicU32::new(0),
        out: Mutex::new(std::io::stdout()),
        st: Mutex::new(St::default()),
        interrupt: AtomicBool::new(false),
        log,
        sock,
        registry,
        orig_termios: raw_mode(),
        sandbox: root,
    });

    if let (Some(reg), Some(id)) = (&agent.registry, &id) {
        let _ = std::fs::write(
            reg,
            json!({"id": id, "name": agent.name, "kind": agent.kind, "pid": std::process::id(), "session": agent.session, "sock": agent.sock}).to_string(),
        );
    }
    if let Some(sock) = agent.sock.clone() {
        let a = agent.clone();
        std::thread::spawn(move || a.serve(&sock));
    }

    // Redraw on resize, as Claude does: a box drawn for the old width wraps
    // into garbage otherwise, and that would look like trix's fault.
    unsafe {
        libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t);
    }
    {
        let a = agent.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(50));
                if WINCH.swap(false, Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(60));
                    WINCH.store(false, Ordering::SeqCst);
                    a.on_resize();
                }
            }
        });
    }

    agent.banner(resume.is_some());
    agent.hook("SessionStart", json!({"source": if resume.is_some() { "resume" } else { "startup" }}));
    match prompt.filter(|p| !p.trim().is_empty()) {
        Some(p) => agent.clone().start_turn(p),
        None => agent.draw_prompt(),
    }
    agent.read_stdin();
    agent.quit(0);
}

static WINCH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: i32) {
    WINCH.store(true, Ordering::SeqCst);
}

fn read_hooks(path: &Path) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else { return out };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return out };
    if let Some(hooks) = v.get("hooks").and_then(Value::as_object) {
        for (event, groups) in hooks {
            for g in groups.as_array().into_iter().flatten() {
                for h in g.get("hooks").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(cmd) = h.get("command").and_then(Value::as_str) {
                        out.entry(event.clone()).or_default().push(cmd.to_string());
                    }
                }
            }
        }
    }
    out
}

fn new_uuid() -> String {
    let mut b = [0u8; 16];
    if std::env::var_os("TRIX_PROBE_SEEDED").is_some() {
        // `compare`: the same agent gets the same id in both runs, so its
        // screen matches. trix numbers agents the same way each time.
        let seed = format!("{}/{}", std::env::var("TRIX_AGENT_ID").unwrap_or_default(), std::env::var("TRIX_AGENT_NAME").unwrap_or_default());
        let mut h: u64 = 0xcbf29ce484222325;
        for (i, x) in b.iter_mut().enumerate() {
            for byte in seed.bytes().chain([i as u8]) {
                h = (h ^ byte as u64).wrapping_mul(0x100000001b3);
            }
            *x = (h >> 24) as u8;
        }
    } else if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn raw_mode() -> Option<libc::termios> {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) != 0 {
            return None;
        }
        let orig = t;
        libc::cfmakeraw(&mut t);
        libc::tcsetattr(0, libc::TCSANOW, &t);
        Some(orig)
    }
}

fn term_width() -> usize {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 { ws.ws_col as usize } else { 80 }
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis())
}

enum Key {
    Char(char),
    Enter,
    Newline,
    Backspace,
    Esc,
    CtrlC,
    CtrlD,
    CtrlU,
}

impl Agent {
    // ---- output ------------------------------------------------------

    fn emit(&self, s: &str) {
        let mut o = self.out.lock().unwrap();
        let _ = o.write_all(s.as_bytes());
        let _ = o.flush();
    }

    fn box_width(&self) -> usize {
        term_width().clamp(20, 100)
    }

    fn banner(&self, resumed: bool) {
        let w = self.box_width();
        let inner = w - 2;
        let line = |s: &str| {
            let s: String = s.chars().take(inner - 2).collect();
            let pad = inner - 1 - s.chars().count();
            format!("│ {s}{}│\r\n", " ".repeat(pad))
        };
        let hooks = if self.hooks.is_empty() { "no hooks: trix reads the screen" } else { "hooks on" };
        let mut s = format!("╭{}╮\r\n", "─".repeat(inner));
        s += &line(&format!("✻ fake {} (trix-probe) · {}", self.kind, hooks));
        s += &line(&format!("  cwd: {}", self.cwd.display()));
        s += &line(&format!("  session {}{}", &self.session[..8.min(self.session.len())], if resumed { " (resumed)" } else { "" }));
        s += &line("  type 'help' for what a prompt can make me do");
        s += &format!("╰{}╯\r\n\r\n", "─".repeat(inner));
        self.emit(&s);
    }

    fn on_resize(&self) {
        let mut st = self.st.lock().unwrap();
        if st.busy {
            return; // the prompt is redrawn when the turn ends
        }
        self.emit("\x1b[2J\x1b[H");
        st.prompt_shown = false;
        drop(st);
        self.banner(false);
        self.draw_prompt();
    }

    /// The input box, with the cursor after the text.
    fn draw_prompt(&self) {
        let mut st = self.st.lock().unwrap();
        self.draw_prompt_locked(&mut st);
    }

    fn draw_prompt_locked(&self, st: &mut St) {
        let w = self.box_width();
        let inner = w - 2;
        let room = inner.saturating_sub(4);
        let shown: String = st.buf.replace('\n', "↵");
        let count = shown.chars().count();
        let tail: String = shown.chars().skip(count.saturating_sub(room)).collect();
        let pad = room.saturating_sub(tail.chars().count());
        let mut s = String::new();
        if st.prompt_shown {
            s += "\r\x1b[1A\x1b[J";
        } else {
            s += "\r\x1b[2K";
        }
        s += &format!("╭{}╮\r\n", "─".repeat(inner));
        s += &format!("│ > {tail}{} │\r\n", " ".repeat(pad));
        s += &format!("╰{}╯", "─".repeat(inner));
        s += &format!("\x1b[1A\r\x1b[{}C", 4 + tail.chars().count());
        st.prompt_shown = true;
        self.emit(&s);
    }

    fn erase_prompt_locked(&self, st: &mut St) {
        if st.prompt_shown {
            self.emit("\r\x1b[1A\x1b[J");
            st.prompt_shown = false;
        }
    }

    /// One line of the conversation, above the spinner or the prompt.
    fn say(&self, line: &str) {
        let mut st = self.st.lock().unwrap();
        let had_prompt = st.prompt_shown;
        self.erase_prompt_locked(&mut st);
        let mut s = String::from("\r\x1b[2K");
        for l in line.split('\n') {
            s += l;
            s += "\r\n";
        }
        self.emit(&s);
        if had_prompt && !st.busy {
            self.draw_prompt_locked(&mut st);
        }
    }

    fn spinner(&self, label: &str, started: Instant) {
        const FRAMES: [char; 6] = ['·', '✢', '✳', '✶', '✻', '✽'];
        let i = (started.elapsed().as_millis() / 120) as usize % FRAMES.len();
        self.emit(&format!("\r\x1b[2K\x1b[38;5;209m{} {label}…\x1b[0m \x1b[2m({}s · esc to interrupt)\x1b[0m", FRAMES[i], started.elapsed().as_secs()));
    }

    /// Spend `ms` working, with the spinner going. Esc stops it.
    fn work(&self, ms: u64, label: &str) -> Result<(), Interrupted> {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(ms) {
            if self.interrupt.load(Ordering::Relaxed) {
                return Err(Interrupted);
            }
            self.spinner(label, start);
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }

    // ---- hooks and logs ----------------------------------------------

    fn log(&self, v: Value) {
        if let Some(p) = &self.log
            && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p)
        {
            let _ = writeln!(f, "{v}");
        }
    }

    fn hook(&self, event: &str, extra: Value) {
        self.log(json!({"at": now_ms() as u64, "hook": event, "payload": extra}));
        let Some(cmds) = self.hooks.get(event) else { return };
        let mut payload = json!({
            "session_id": self.session,
            "transcript_path": self.transcript.as_ref().map(|p| p.display().to_string()),
            "cwd": self.cwd.display().to_string(),
            "hook_event_name": event,
            "permission_mode": "default",
        });
        if let (Some(obj), Some(more)) = (payload.as_object_mut(), extra.as_object()) {
            for (k, v) in more {
                obj.insert(k.clone(), v.clone());
            }
        }
        let body = payload.to_string();
        for cmd in cmds {
            let child = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(&self.cwd)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            if let Ok(mut child) = child {
                if let Some(mut i) = child.stdin.take() {
                    let _ = i.write_all(body.as_bytes());
                }
                // Claude waits for its hooks (trix's have a 5 s timeout).
                let start = Instant::now();
                while start.elapsed() < Duration::from_secs(5) {
                    if let Ok(Some(_)) = child.try_wait() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// Append a turn's token usage to the conversation log.
    fn record_usage(&self) {
        let Some(path) = &self.transcript else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let used = (self.context.load(Ordering::Relaxed) as u64 * 200_000 / 1000).max(500);
        let line = json!({
            "type": "assistant",
            "isSidechain": false,
            "sessionId": self.session,
            "message": {"model": "claude-opus-5-5", "role": "assistant", "usage": {
                "input_tokens": 3, "cache_creation_input_tokens": 200, "cache_read_input_tokens": used - 3 - 200 - 100, "output_tokens": 100
            }}
        });
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }

    // ---- turns -------------------------------------------------------

    fn start_turn(self: Arc<Self>, prompt: String) {
        {
            let mut st = self.st.lock().unwrap();
            if st.busy {
                return;
            }
            st.busy = true;
            self.erase_prompt_locked(&mut st);
        }
        self.interrupt.store(false, Ordering::Relaxed);
        std::thread::spawn(move || {
            let shown = prompt.replace('\n', "\r\n  ");
            self.emit(&format!("\r\x1b[2K\x1b[2m>\x1b[0m {shown}\r\n"));
            let outcome = self.turn(&prompt);
            match outcome {
                Ok(true) => {
                    self.record_usage();
                    self.hook("Stop", json!({"stop_hook_active": false}));
                }
                Ok(false) => {}
                Err(Interrupted) => {
                    self.emit("\r\x1b[2K");
                    self.say("  \x1b[31m⎿  Interrupted by user\x1b[0m · What should Claude do instead?");
                }
            }
            self.turns.fetch_add(1, Ordering::Relaxed);
            self.emit("\r\x1b[2K\r\n");
            let mut st = self.st.lock().unwrap();
            st.busy = false;
            st.pending = None;
            self.draw_prompt_locked(&mut st);
        });
    }

    /// One turn. Ok(true): finished normally (Stop fires); Ok(false): ended
    /// without Stop (a rejected permission, like Claude).
    fn turn(&self, prompt: &str) -> Result<bool, Interrupted> {
        let p = prompt.to_lowercase();
        let has = |w: &str| p.split(|c: char| !c.is_alphanumeric() && c != '/').any(|t| t == w);
        let silent = has("silent");
        if !silent {
            self.hook("UserPromptSubmit", json!({"prompt": prompt}));
        }
        let ctx = self.context.load(Ordering::Relaxed);
        self.context.store((ctx + 30 + (prompt.len() as u32 / 5).min(50)).min(990), Ordering::Relaxed);

        if has("help") {
            self.work(400, "Thinking")?;
            self.say(HELP);
            return Ok(true);
        }
        if has("crash") {
            self.say("\x1b[31mError: fake crash (asked for by the prompt)\x1b[0m");
            self.quit(1);
        }
        if has("exit") || has("/exit") || has("quit") {
            self.say("Bye.");
            self.quit(0);
        }
        if let Some(n) = p.split_whitespace().skip_while(|w| *w != "context").nth(1).and_then(|n| n.trim_end_matches('%').parse::<u32>().ok()) {
            self.context.store((n * 10).min(1000), Ordering::Relaxed);
            self.work(600, "Thinking")?;
            self.say(&format!("⏺ Context set to {n}%."));
            return Ok(true);
        }
        if let Some(rest) = prompt.strip_prefix("title ") {
            self.emit(&format!("\x1b]0;{rest}\x07"));
            self.work(300, "Thinking")?;
            self.say(&format!("⏺ Title set to {rest:?}."));
            return Ok(true);
        }
        if has("bell") {
            self.work(300, "Thinking")?;
            self.emit("\x07");
            self.say("⏺ Rang the bell.");
            return Ok(true);
        }
        if has("permission") || has("approve") || has("rm") {
            return self.permission_turn();
        }
        if has("question") || has("ask") {
            return self.question_turn();
        }
        if has("flood") {
            return self.flood_turn();
        }
        if has("unicode") || has("emoji") {
            self.work(500, "Thinking")?;
            self.say(UNICODE);
            return Ok(true);
        }
        if has("long") || has("slow") {
            for i in 0..6 {
                self.tool("Bash", json!({"command": format!("sleep 5 # step {}", i + 1)}), &["(no output)"], 5000)?;
            }
            self.say("⏺ The long task is done.");
            return Ok(true);
        }
        // A normal turn.
        self.work(700, "Thinking")?;
        self.tool("Read", json!({"file_path": self.cwd.join("src/main.rs").display().to_string()}), &["Read 9 lines"], 600)?;
        self.tool("Grep", json!({"pattern": "fn add"}), &["Found 1 match"], 400)?;
        self.edit_demo();
        self.tool("Edit", json!({"file_path": self.cwd.join("src/main.rs").display().to_string(), "old_string": "a + b", "new_string": "a + b"}), &["Updated src/main.rs with 1 addition"], 800)?;
        self.tool("Bash", json!({"command": "cargo test"}), &["running 3 tests", "test tests::adds ... ok", "test tests::subs ... ok", "test result: ok. 3 passed"], 1500)?;
        let reply = format!("⏺ Done: {} (fake reply, turn {})", one_line(prompt, 60), self.turns.load(Ordering::Relaxed) + 1);
        self.say(&reply);
        Ok(true)
    }

    /// A real (tiny) edit in the demo repo, so trix's diff count moves. Only
    /// inside the probe's sandbox.
    fn edit_demo(&self) {
        let Some(root) = &self.sandbox else { return };
        let root = std::fs::canonicalize(root).unwrap_or(root.clone());
        let cwd = std::fs::canonicalize(&self.cwd).unwrap_or(self.cwd.clone());
        if !cwd.starts_with(&root) {
            return;
        }
        let f = cwd.join("src/main.rs");
        if let Ok(mut s) = std::fs::read_to_string(&f) {
            s.push_str(&format!("// edited by fake {} turn {}\n", self.kind, self.turns.load(Ordering::Relaxed) + 1));
            let _ = std::fs::write(&f, s);
        }
    }

    fn tool(&self, name: &str, input: Value, result: &[&str], ms: u64) -> Result<(), Interrupted> {
        let detail = tool_detail(name, &input);
        self.hook("PreToolUse", json!({"tool_name": name, "tool_input": input}));
        self.say(&format!("\x1b[32m⏺\x1b[0m \x1b[1m{name}\x1b[0m({detail})"));
        self.work(ms, "Working")?;
        for (i, r) in result.iter().enumerate() {
            self.say(&format!("  {}  {r}", if i == 0 { "⎿" } else { " " }));
        }
        self.hook("PostToolUse", json!({"tool_name": name, "tool_input": input, "tool_response": {"output": result.join("\n")}}));
        Ok(())
    }

    /// Wait for one key while a dialog is up.
    fn ask(&self) -> Result<Answer, Interrupted> {
        let (tx, rx) = mpsc::channel();
        self.st.lock().unwrap().pending = Some(tx);
        let answer = rx.recv().map_err(|_| Interrupted);
        self.st.lock().unwrap().pending = None;
        answer
    }

    fn dialog(&self, title: &str, body: &[String], options: &[&str]) {
        let w = self.box_width();
        let inner = w - 2;
        let row = |s: &str| {
            let s: String = s.chars().take(inner - 2).collect();
            format!("│ {s}{}│", " ".repeat(inner - 1 - s.chars().count()))
        };
        let mut lines = vec![format!("╭{}╮", "─".repeat(inner)), row(title)];
        for b in body {
            lines.push(row(&format!("  {b}")));
        }
        lines.push(row(""));
        for (i, o) in options.iter().enumerate() {
            lines.push(row(&format!("{} {}. {o}", if i == 0 { "❯" } else { " " }, i + 1)));
        }
        lines.push(format!("╰{}╯", "─".repeat(inner)));
        self.emit("\r\x1b[2K");
        self.say(&lines.join("\n"));
    }

    fn permission_turn(&self) -> Result<bool, Interrupted> {
        self.work(800, "Thinking")?;
        let input = json!({"command": "rm -rf dist", "description": "Remove the build output"});
        self.hook("PreToolUse", json!({"tool_name": "Bash", "tool_input": input}));
        self.say("\x1b[32m⏺\x1b[0m \x1b[1mBash\x1b[0m(rm -rf dist)");
        self.hook("PermissionRequest", json!({"tool_name": "Bash", "tool_input": input}));
        self.hook("Notification", json!({"message": "Claude needs your permission to use Bash", "notification_type": "permission_prompt"}));
        self.dialog("Bash command", &["rm -rf dist".into(), "Remove the build output".into(), String::new(), "Do you want to proceed?".into()], &["Yes", "Yes, and don't ask again for rm commands", "No, and tell Claude what to do differently (esc)"]);
        loop {
            match self.ask()? {
                Answer::Key('1') | Answer::Key('y') | Answer::Enter | Answer::Key('2') => {
                    self.work(600, "Working")?;
                    self.say("  ⎿  (no output)");
                    self.hook("PostToolUse", json!({"tool_name": "Bash", "tool_input": input, "tool_response": {"output": ""}}));
                    self.say("⏺ Removed dist/.");
                    return Ok(true);
                }
                Answer::Key('3') | Answer::Key('n') | Answer::Esc => {
                    self.say("  \x1b[31m⎿  User rejected the command\x1b[0m");
                    return Ok(false);
                }
                _ => {}
            }
        }
    }

    fn question_turn(&self) -> Result<bool, Interrupted> {
        self.work(600, "Thinking")?;
        let q = "Which database should the demo use?";
        let input = json!({"questions": [{"question": q, "header": "Database", "multiSelect": false,
            "options": [{"label": "Postgres", "description": "A server"}, {"label": "SQLite", "description": "A file"}]}]});
        self.hook("PreToolUse", json!({"tool_name": "AskUserQuestion", "tool_input": input}));
        self.dialog(q, &[], &["Postgres", "SQLite", "Type something else"]);
        let pick = loop {
            match self.ask()? {
                Answer::Key(c @ '1'..='3') => break c,
                Answer::Enter => break '1',
                Answer::Esc => {
                    self.say("  \x1b[31m⎿  User declined to answer\x1b[0m");
                    return Ok(false);
                }
                _ => {}
            }
        };
        let label = ["Postgres", "SQLite", "something else"][(pick as u8 - b'1') as usize];
        self.hook("PostToolUse", json!({"tool_name": "AskUserQuestion", "tool_input": input, "tool_response": {"answers": {q: label}}}));
        self.say(&format!("  ⎿  {label}"));
        self.work(800, "Working")?;
        self.say(&format!("⏺ Going with {label}."));
        Ok(true)
    }

    fn flood_turn(&self) -> Result<bool, Interrupted> {
        let input = json!({"command": "seq 1 3000"});
        self.hook("PreToolUse", json!({"tool_name": "Bash", "tool_input": input}));
        self.say("\x1b[32m⏺\x1b[0m \x1b[1mBash\x1b[0m(seq 1 3000)");
        let mut chunk = String::new();
        for i in 1..=3000 {
            chunk += &format!("  {i:>5}  lorem ipsum dolor sit amet, consectetur adipiscing elit\r\n");
            if i % 100 == 0 {
                if self.interrupt.load(Ordering::Relaxed) {
                    return Err(Interrupted);
                }
                self.emit(&chunk);
                chunk.clear();
            }
        }
        self.hook("PostToolUse", json!({"tool_name": "Bash", "tool_input": input, "tool_response": {"output": "3000 lines"}}));
        self.say("⏺ Printed 3000 lines.");
        Ok(true)
    }

    // ---- input -------------------------------------------------------

    fn read_stdin(self: &Arc<Self>) {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            let n = match stdin.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            for key in self.decode(&buf[..n]) {
                self.on_key(key);
            }
        }
    }

    fn decode(&self, b: &[u8]) -> Vec<Key> {
        let mut keys = Vec::new();
        let mut i = 0;
        let pasting = |a: &Agent| a.st.lock().unwrap().pasting;
        while i < b.len() {
            match b[i] {
                0x1b if i + 1 == b.len() => {
                    keys.push(Key::Esc);
                    i += 1;
                }
                0x1b if b[i + 1] == b'[' => {
                    let mut j = i + 2;
                    while j < b.len() && !(0x40..=0x7e).contains(&b[j]) {
                        j += 1;
                    }
                    let seq = String::from_utf8_lossy(&b[i + 2..=j.min(b.len() - 1)]).into_owned();
                    match seq.as_str() {
                        "200~" => self.st.lock().unwrap().pasting = true,
                        "201~" => self.st.lock().unwrap().pasting = false,
                        "13;2u" => keys.push(Key::Newline),
                        "27u" => keys.push(Key::Esc),
                        "99;5u" => keys.push(Key::CtrlC),
                        _ => {}
                    }
                    i = j + 1;
                }
                0x1b if b[i + 1] == b'\r' => {
                    keys.push(Key::Newline);
                    i += 2;
                }
                0x1b if b[i + 1] == b'O' => i += 3,
                0x1b => i += 2, // alt+key: nothing in this fake
                b'\r' | b'\n' => {
                    keys.push(if pasting(self) { Key::Newline } else { Key::Enter });
                    i += 1;
                }
                0x7f | 0x08 => {
                    keys.push(Key::Backspace);
                    i += 1;
                }
                0x03 => {
                    keys.push(Key::CtrlC);
                    i += 1;
                }
                0x04 => {
                    keys.push(Key::CtrlD);
                    i += 1;
                }
                0x15 => {
                    keys.push(Key::CtrlU);
                    i += 1;
                }
                c if c < 0x20 => i += 1,
                c => {
                    let len = match c {
                        0xc0..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf7 => 4,
                        _ => 1,
                    };
                    let end = (i + len).min(b.len());
                    for ch in String::from_utf8_lossy(&b[i..end]).chars() {
                        keys.push(Key::Char(ch));
                    }
                    i = end;
                }
            }
        }
        keys
    }

    fn on_key(self: &Arc<Self>, key: Key) {
        let mut st = self.st.lock().unwrap();
        if let Some(tx) = &st.pending {
            let a = match key {
                Key::Char(c) => Some(Answer::Key(c)),
                Key::Enter => Some(Answer::Enter),
                Key::Esc => Some(Answer::Esc),
                _ => None,
            };
            if let Some(a) = a {
                let _ = tx.send(a);
            }
            return;
        }
        if st.busy {
            match key {
                Key::Esc => self.interrupt.store(true, Ordering::Relaxed),
                Key::Char(c) => st.buf.push(c), // typed ahead, shown after the turn
                _ => {}
            }
            return;
        }
        match key {
            Key::Char(c) => st.buf.push(c),
            Key::Newline => st.buf.push('\n'),
            Key::Backspace => {
                st.buf.pop();
            }
            Key::CtrlU => st.buf.clear(),
            Key::Esc => {}
            Key::Enter => {
                let prompt = std::mem::take(&mut st.buf);
                if prompt.trim().is_empty() {
                    return;
                }
                drop(st);
                self.clone().start_turn(prompt);
                return;
            }
            Key::CtrlC => {
                if !st.buf.is_empty() {
                    st.buf.clear();
                } else if st.ctrlc_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    drop(st);
                    self.quit(0);
                } else {
                    st.ctrlc_at = Some(Instant::now());
                    self.erase_prompt_locked(&mut st);
                    self.emit("\r\x1b[2K\x1b[2mPress Ctrl-C again to exit\x1b[0m\r\n");
                }
            }
            Key::CtrlD if st.buf.is_empty() => {
                drop(st);
                self.quit(0);
            }
            Key::CtrlD => {}
        }
        self.draw_prompt_locked(&mut st);
    }

    // ---- control socket ----------------------------------------------

    fn serve(self: Arc<Self>, path: &Path) {
        let _ = std::fs::remove_file(path);
        let Ok(listener) = UnixListener::bind(path) else { return };
        for stream in listener.incoming().flatten() {
            let mut line = String::new();
            let mut reader = BufReader::new(&stream);
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(cmd) => self.clone().command(&cmd),
                Err(e) => json!({"ok": false, "error": format!("bad json: {e}")}),
            };
            let mut w = &stream;
            let _ = writeln!(w, "{reply}");
        }
    }

    fn command(self: Arc<Self>, cmd: &Value) -> Value {
        let what = cmd.get("do").and_then(Value::as_str).unwrap_or("");
        let text = cmd.get("text").and_then(Value::as_str).unwrap_or("").to_string();
        let busy = self.st.lock().unwrap().busy;
        let pending = self.st.lock().unwrap().pending.is_some();
        let start = |prompt: String| -> Value {
            if busy {
                return json!({"ok": false, "error": "busy: a turn is running (use interrupt, or answer a dialog)"});
            }
            self.clone().start_turn(prompt);
            json!({"ok": true})
        };
        match what {
            "status" => json!({"ok": true, "kind": self.kind, "name": self.name, "busy": busy, "dialog_open": pending,
                "session": self.session, "context_percent": self.context.load(Ordering::Relaxed) as f32 / 10.0,
                "turns": self.turns.load(Ordering::Relaxed), "hooks": !self.hooks.is_empty()}),
            "turn" => start(if text.is_empty() { "make a small change".into() } else { text }),
            "permission" => start("(probe) ask permission to rm -rf dist".into()),
            "question" => start("(probe) ask a question".into()),
            "long" => start("(probe) long task".into()),
            "flood" => start("(probe) flood the screen".into()),
            "unicode" => start("(probe) unicode".into()),
            "silent" => start(format!("(probe) silent {text}")),
            "help" => start("help".into()),
            "crash" => {
                self.say("\x1b[31mError: fake crash (asked for by the probe)\x1b[0m");
                self.quit(1)
            }
            "exit" => self.quit(0),
            "answer" => {
                let key = cmd.get("key").and_then(Value::as_str).unwrap_or("enter");
                let a = match key {
                    "enter" => Answer::Enter,
                    "esc" => Answer::Esc,
                    k => Answer::Key(k.chars().next().unwrap_or('1')),
                };
                match &self.st.lock().unwrap().pending {
                    Some(tx) => {
                        let _ = tx.send(a);
                        json!({"ok": true})
                    }
                    None => json!({"ok": false, "error": "no dialog is open"}),
                }
            }
            "interrupt" => {
                self.interrupt.store(true, Ordering::Relaxed);
                json!({"ok": true})
            }
            "output" => {
                self.say(&text);
                json!({"ok": true})
            }
            "title" => {
                self.emit(&format!("\x1b]0;{text}\x07"));
                json!({"ok": true})
            }
            "bell" => {
                self.emit("\x07");
                json!({"ok": true})
            }
            "context" => {
                let p = cmd.get("percent").and_then(Value::as_f64).unwrap_or(50.0);
                self.context.store((p * 10.0).clamp(0.0, 1000.0) as u32, Ordering::Relaxed);
                self.record_usage();
                json!({"ok": true})
            }
            "hook" => {
                let event = cmd.get("event").and_then(Value::as_str).unwrap_or("Stop");
                self.hook(event, cmd.get("payload").cloned().unwrap_or(json!({})));
                json!({"ok": true, "hooks": !self.hooks.is_empty()})
            }
            other => json!({"ok": false, "error": format!("unknown command '{other}'")}),
        }
    }

    fn quit(&self, code: i32) -> ! {
        self.hook("SessionEnd", json!({"reason": if code == 0 { "prompt_input_exit" } else { "other" }}));
        if let Some(t) = &self.orig_termios {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, t);
            }
        }
        self.emit("\r\n");
        for p in [&self.sock, &self.registry].into_iter().flatten() {
            let _ = std::fs::remove_file(p);
        }
        std::process::exit(code);
    }
}

fn tool_detail(name: &str, input: &Value) -> String {
    let g = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    match name {
        "Bash" => g("command").to_string(),
        "Read" | "Edit" | "Write" => Path::new(g("file_path")).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default(),
        "Grep" => format!("\"{}\"", g("pattern")),
        _ => String::new(),
    }
}

fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max { format!("{}…", flat.chars().take(max - 1).collect::<String>()) } else { flat }
}

const HELP: &str = "⏺ I'm a fake agent for testing trix. Words in a prompt pick what I do:
    permission   ask to run `rm -rf dist`, wait for 1/2/3 (needs-you)
    question     ask which database, wait for 1/2/3 (needs-you)
    long         work for ~30 s, one tool every 5 s
    flood        print 3000 lines fast
    unicode      print wide, combining, RTL, emoji and colored text
    silent       a turn that fires no hooks (trix has to guess)
    context 85   set the context gauge to 85%
    title <text> set the terminal title
    bell         ring the bell
    crash        exit with code 1 at once
    exit         exit normally
  Anything else: a short turn with Read, Grep, Edit and Bash. Esc interrupts.";

const UNICODE: &str = "⏺ Unicode check:
  wide:      你好世界 · 日本語のテキスト · 한국어
  emoji:     🚀 ✅ 🔥 👩‍💻 🇺🇸
  combining: é (e + U+0301) · ñ · Z̤͔ͧ̑̓
  rtl:       שלום עולם · مرحبا بالعالم
  symbols:   ⚲ ▲ ✓ ○ • ⠹ ◆ ▂▃▅▇ ━━ ╭─╮
  colors:    \x1b[31mred\x1b[0m \x1b[1;32mbold green\x1b[0m \x1b[38;5;208m256-orange\x1b[0m \x1b[38;2;120;200;255mtruecolor\x1b[0m \x1b[7minverse\x1b[0m \x1b[4munderline\x1b[0m
  link:      \x1b]8;;https://example.com\x1b\\a hyperlink\x1b]8;;\x1b\\
  tab:\tafter a tab
  long:      Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. Duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla pariatur.";
