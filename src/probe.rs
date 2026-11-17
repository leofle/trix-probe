//! One testing session: a sandbox, the terminals open on it, and everything
//! the tester has seen and reported. The MCP server and the autonomous runner
//! both drive trix through this.

use crate::keys::{self, Button, Keyboard, Mods, MouseAct};
use crate::sandbox::{AgentMode, Sandbox, SandboxOpts};
use crate::term::{Palette, Snapshot, Terminal};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One thing `sweep` opens at every size.
struct SweepCase {
    name: String,
    keys: Vec<String>,
    text: Option<String>,
    /// Pressed afterwards to get back (default esc).
    reset: Vec<String>,
}

#[derive(Clone)]
pub struct ProbeConfig {
    /// The trix binary to test unless `launch` names another.
    pub trix: PathBuf,
    /// This program, for the shims in the sandbox.
    pub exe: PathBuf,
    /// Where reports and screenshots go.
    pub out: PathBuf,
    pub keep_sandboxes: bool,
    pub font_px: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shot {
    None,
    /// The screen as text: only the rows that changed since the model last
    /// saw it, when that's few (see `Probe::screen_text`).
    Text,
    /// The whole screen as text, always.
    Full,
    Png,
    Both,
}

impl Shot {
    pub fn parse(v: Option<&Value>, default: Shot) -> Shot {
        match v.and_then(Value::as_str) {
            Some("none") => Shot::None,
            Some("text") => Shot::Text,
            Some("full") => Shot::Full,
            Some("png") | Some("image") => Shot::Png,
            Some("both") => Shot::Both,
            _ => default,
        }
    }

    /// The same, but always including the whole screen as text.
    pub fn max_text(self) -> Shot {
        match self {
            Shot::None | Shot::Text | Shot::Full => Shot::Full,
            Shot::Png | Shot::Both => Shot::Both,
        }
    }
}

/// What a tool hands back to the model.
#[derive(Default)]
pub struct ToolOut {
    pub text: String,
    pub images: Vec<Vec<u8>>,
    pub is_error: bool,
    /// Screens a tool looked at along the way, labeled (`sweep`'s), for
    /// `compare` to set side by side. Empty: only the screen at the end counts.
    pub frames: Vec<(String, Snapshot)>,
}

impl ToolOut {
    pub fn text(s: impl Into<String>) -> ToolOut {
        ToolOut { text: s.into(), ..Default::default() }
    }
    pub fn error(s: impl Into<String>) -> ToolOut {
        ToolOut { text: s.into(), is_error: true, ..Default::default() }
    }
}

/// Follows `trix events --json` into logs/events.jsonl, restarting it when
/// it ends (the server restarted, or wasn't up yet).
struct EventsFollower {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl EventsFollower {
    fn start(sb: &Sandbox) -> EventsFollower {
        let stop = Arc::new(AtomicBool::new(false));
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let (stop2, child2) = (stop.clone(), child.clone());
        let mut cmd = sb.trix_command(&["events".into(), "--json".into()]);
        cmd.stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null());
        let log = sb.logs.join("events.jsonl");
        std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                if let Ok(mut c) = cmd.spawn() {
                    let out = c.stdout.take();
                    *child2.lock().unwrap() = Some(c);
                    if let Some(out) = out {
                        for line in BufReader::new(out).lines().map_while(Result::ok) {
                            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
                                let _ = writeln!(f, "{line}");
                            }
                        }
                    }
                    if let Some(mut c) = child2.lock().unwrap().take() {
                        let _ = c.wait();
                    }
                }
                std::thread::sleep(Duration::from_millis(400));
            }
        });
        EventsFollower { stop, child }
    }
}

impl Drop for EventsFollower {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

pub struct Probe {
    pub cfg: ProbeConfig,
    /// The trix this session runs: `launch`'s `trix`, else `cfg.trix`.
    pub trix: PathBuf,
    /// When set, every launch runs this trix whatever its `trix` says
    /// (`compare` and `replay --trix` pin the binary this way).
    pub trix_override: Option<PathBuf>,
    /// When set, every launch builds its sandbox at this path (see `SandboxOpts::root`),
    /// and fake agents take ids that are the same from run to run.
    pub fixed_root: Option<PathBuf>,
    /// Numbers the folders `sweep` and `compare` write into.
    pub runs: u32,
    pub sb: Option<Sandbox>,
    terms: BTreeMap<String, Terminal>,
    events: Option<EventsFollower>,
    palette: Palette,
    keyboard: Keyboard,
    default_size: (u16, u16),
    /// Every action since the last launch, for repro scripts.
    pub actions: Vec<Value>,
    issues: Vec<Value>,
    last_seen: Instant,
    offsets: BTreeMap<String, u64>,
    shots: u32,
    /// The screen of each terminal as the model last saw it, to send only
    /// what changed.
    looked: BTreeMap<String, Looked>,
    /// The layout problems last told for each terminal, to tell only news.
    layout_said: BTreeMap<String, Vec<String>>,
}

/// A screen the model has seen.
struct Looked {
    size: (usize, usize),
    /// Masked rows: the clock and spinners ticking isn't a change.
    rows: Vec<String>,
    styles: Vec<u64>,
    /// Partial screens sent since the last whole one.
    partial: u32,
}

/// After this many partial screens the next is whole again, in case the
/// client has dropped old results from its context (explore keeps 10).
const WHOLE_EVERY: u32 = 8;

impl Probe {
    pub fn new(cfg: ProbeConfig) -> Probe {
        Probe {
            trix: cfg.trix.clone(),
            cfg,
            trix_override: None,
            fixed_root: None,
            runs: 0,
            sb: None,
            terms: BTreeMap::new(),
            events: None,
            palette: Palette::dark(),
            keyboard: Keyboard::Legacy,
            default_size: (140, 40),
            actions: Vec::new(),
            issues: Vec::new(),
            last_seen: Instant::now(),
            offsets: BTreeMap::new(),
            shots: 0,
            looked: BTreeMap::new(),
            layout_said: BTreeMap::new(),
        }
    }

    pub fn issues(&self) -> &[Value] {
        &self.issues
    }

    fn sb(&self) -> Result<&Sandbox> {
        self.sb.as_ref().context("nothing is running: call launch first")
    }

    fn term(&self, name: Option<&str>) -> Result<&Terminal> {
        let name = name.unwrap_or("main");
        self.terms.get(name).with_context(|| {
            let open: Vec<&String> = self.terms.keys().collect();
            format!("no terminal named '{name}' (open: {open:?})")
        })
    }

    pub fn teardown(&mut self) {
        self.events = None;
        self.terms.clear();
        self.looked.clear();
        self.layout_said.clear();
        if let Some(sb) = self.sb.take() {
            sb.teardown();
            crate::cleanup::forget(&sb.root);
        }
    }

    // ---- launching ---------------------------------------------------

    pub fn launch(&mut self, a: &Value) -> Result<ToolOut> {
        // Before the teardown, so a mistyped path leaves the session running.
        let trix = match (&self.trix_override, a.get("trix").and_then(Value::as_str).filter(|s| !s.trim().is_empty())) {
            (Some(t), _) => t.clone(),
            (None, Some(t)) => resolve_trix(t)?,
            (None, None) => self.cfg.trix.clone(),
        };
        self.teardown();
        self.actions.clear();
        self.offsets.clear();
        self.trix = trix;
        let agents = match a.get("agents").and_then(Value::as_str) {
            Some("real") => AgentMode::Real,
            _ => AgentMode::Fake,
        };
        let mut extra_env = BTreeMap::new();
        if let Some(env) = a.get("env").and_then(Value::as_object) {
            for (k, v) in env {
                extra_env.insert(k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
            }
        }
        if self.fixed_root.is_some() {
            extra_env.insert("TRIX_PROBE_SEEDED".into(), "1".into());
        }
        let opts = SandboxOpts {
            agents,
            config_toml: a.get("config_toml").and_then(Value::as_str).map(str::to_string),
            extra_env,
            keep: self.cfg.keep_sandboxes,
            root: self.fixed_root.clone(),
        };
        let sb = Sandbox::create(&self.trix, &self.cfg.exe, &opts)?;
        crate::cleanup::remember(&sb);
        self.palette = Palette::by_name(a.get("terminal_colors").and_then(Value::as_str).unwrap_or("dark"));
        self.keyboard = if a.get("keyboard").and_then(Value::as_str) == Some("kitty") { Keyboard::Kitty } else { Keyboard::Legacy };
        let cols = a.get("cols").and_then(Value::as_u64).unwrap_or(140).clamp(20, 400) as u16;
        let rows = a.get("rows").and_then(Value::as_u64).unwrap_or(40).clamp(5, 200) as u16;
        self.default_size = (cols, rows);
        let args = str_list(a.get("args"));
        let cwd = match a.get("cwd").and_then(Value::as_str) {
            Some(p) => sb.resolve(p)?,
            None => sb.demo_repo(),
        };
        let summary = format!(
            "sandbox {} · trix {} ({}) · {} agents · {}x{} · {} terminal colors · {} keyboard\nagents start in {} (a git repo with an uncommitted change); {} is also there.\n",
            sb.root.display(),
            self.trix.display(),
            self.trix_version(),
            if agents == AgentMode::Fake { "fake" } else { "REAL" },
            cols,
            rows,
            self.palette.name,
            if self.keyboard == Keyboard::Kitty { "kitty" } else { "legacy" },
            cwd.display(),
            sb.work.join("other").display(),
        );
        self.sb = Some(sb);
        self.spawn_terminal("main", &args, (cols, rows), Some(&cwd))?;
        // trix may take a moment to start its server.
        let t = self.term(None)?;
        t.settle_output(Duration::from_millis(400), Duration::from_millis(4000));
        if let Some(f) = keys::focus(true, t.mode()) {
            t.write(f);
        }
        self.events = Some(EventsFollower::start(self.sb()?));
        self.last_seen = Instant::now();
        let mut out = self.screen_out(None, Shot::parse(a.get("screenshot"), Shot::Both))?;
        out.text = format!("{summary}\n{}", out.text);
        Ok(out)
    }

    fn spawn_terminal(&mut self, name: &str, args: &[String], size: (u16, u16), cwd: Option<&Path>) -> Result<()> {
        let sb = self.sb()?;
        let cwd = cwd.map(Path::to_path_buf).unwrap_or_else(|| sb.demo_repo());
        let t = Terminal::spawn(&sb.trix, args, &cwd, &sb.env, size, self.palette.clone(), self.keyboard)?;
        self.terms.insert(name.to_string(), t);
        self.looked.remove(name);
        self.layout_said.remove(name);
        Ok(())
    }

    pub fn open_terminal(&mut self, a: &Value) -> Result<ToolOut> {
        let name = a.get("name").and_then(Value::as_str).context("name is required")?.to_string();
        if self.terms.get(&name).is_some_and(|t| t.exited().is_none()) {
            bail!("terminal '{name}' is still open: close_terminal it first, or pick another name");
        }
        let args = str_list(a.get("args"));
        let cols = a.get("cols").and_then(Value::as_u64).map(|c| c as u16).unwrap_or(self.default_size.0);
        let rows = a.get("rows").and_then(Value::as_u64).map(|r| r as u16).unwrap_or(self.default_size.1);
        self.spawn_terminal(&name, &args, (cols, rows), None)?;
        let t = self.term(Some(&name))?;
        t.settle(Duration::from_millis(300), Duration::from_millis(3000));
        if let Some(f) = keys::focus(true, t.mode()) {
            t.write(f);
        }
        self.screen_out(Some(&name), Shot::parse(a.get("screenshot"), Shot::Text))
    }

    pub fn close_terminal(&mut self, a: &Value) -> Result<ToolOut> {
        let name = a.get("name").and_then(Value::as_str).unwrap_or("main").to_string();
        let t = self.terms.get(&name).with_context(|| format!("no terminal named '{name}'"))?;
        let how = a.get("how").and_then(Value::as_str).unwrap_or("hangup");
        t.signal(if how == "kill" { libc::SIGKILL } else { libc::SIGHUP });
        let start = Instant::now();
        while t.exited().is_none() && start.elapsed() < Duration::from_millis(1000) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let exited = t.exited();
        self.terms.remove(&name);
        self.looked.remove(&name);
        self.layout_said.remove(&name);
        Ok(ToolOut::text(format!(
            "closed terminal '{name}' ({}){}.{}",
            if how == "kill" { "SIGKILL, like a crashed terminal" } else { "SIGHUP, like closing the window" },
            match exited {
                Some(Some(c)) => format!("; it exited with code {c}"),
                Some(None) => "; it was killed by the signal".into(),
                None => "; it had not exited yet".into(),
            },
            self.changes()
        )))
    }

    // ---- input -------------------------------------------------------

    fn after(&mut self, a: &Value, default: Shot) -> Result<ToolOut> {
        let name = a.get("terminal").and_then(Value::as_str);
        let wait = a.get("wait_ms").and_then(Value::as_u64).unwrap_or(1500);
        self.term(name)?.settle(Duration::from_millis(250), Duration::from_millis(wait));
        self.screen_out(name, Shot::parse(a.get("screenshot"), default))
    }

    pub fn press(&mut self, a: &Value) -> Result<ToolOut> {
        let keys_list = str_list(a.get("keys"));
        anyhow::ensure!(!keys_list.is_empty(), "keys is required, e.g. [\"alt+n\"]");
        let delay = a.get("delay_ms").and_then(Value::as_u64).unwrap_or(80);
        let t = self.term(a.get("terminal").and_then(Value::as_str))?;
        for (i, k) in keys_list.iter().enumerate() {
            let bytes = keys::encode(k, t.mode(), t.keyboard)?;
            t.write(&bytes);
            if i + 1 < keys_list.len() {
                std::thread::sleep(Duration::from_millis(delay));
            }
        }
        self.after(a, Shot::Text)
    }

    pub fn type_text(&mut self, a: &Value) -> Result<ToolOut> {
        let text = a.get("text").and_then(Value::as_str).context("text is required")?;
        let paste = a.get("paste").and_then(Value::as_bool).unwrap_or(false);
        let t = self.term(a.get("terminal").and_then(Value::as_str))?;
        let mode = t.mode();
        if paste {
            t.write(&keys::text(text, true, mode));
        } else {
            // A person types: one character at a time.
            for ch in text.chars() {
                t.write(&keys::text(&ch.to_string(), false, mode));
                std::thread::sleep(Duration::from_millis(8));
            }
        }
        if a.get("enter").and_then(Value::as_bool).unwrap_or(false) {
            std::thread::sleep(Duration::from_millis(60));
            t.write(b"\r");
        }
        self.after(a, Shot::Text)
    }

    /// (row, col) from either coordinates or text on screen.
    fn target(&self, a: &Value, prefix: &str) -> Result<(usize, usize)> {
        let t = self.term(a.get("terminal").and_then(Value::as_str))?;
        let key = |k: &str| format!("{prefix}{k}");
        if let Some(text) = a.get(key("text")).and_then(Value::as_str) {
            let snap = t.snapshot();
            let hits = snap.find(text);
            let nth = a.get(key("nth")).and_then(Value::as_u64).unwrap_or(0) as usize;
            let (r, c) = *hits.get(nth).with_context(|| {
                if hits.is_empty() { format!("{text:?} is not on screen") } else { format!("{text:?} is on screen {} times; nth {nth} is out of range", hits.len()) }
            })?;
            // Aim at the middle of the text.
            let width = text.chars().count();
            return Ok((r, (c + width / 2).min(snap.cols.saturating_sub(1))));
        }
        let r = a.get(key("row")).and_then(Value::as_u64).with_context(|| format!("give {prefix}row and {prefix}col, or {prefix}text"))?;
        let c = a.get(key("col")).and_then(Value::as_u64).with_context(|| format!("give {prefix}row and {prefix}col, or {prefix}text"))?;
        let (cols, rows) = t.size();
        anyhow::ensure!((r as u16) < rows && (c as u16) < cols, "({r},{c}) is off the {cols}x{rows} screen");
        Ok((r as usize, c as usize))
    }

    pub fn click(&mut self, a: &Value) -> Result<ToolOut> {
        let (r, c) = self.target(a, "")?;
        let button = match a.get("button").and_then(Value::as_str) {
            Some("right") => Button::Right,
            Some("middle") => Button::Middle,
            _ => Button::Left,
        };
        let mods = mods_of(a.get("modifiers"));
        let times = if a.get("double").and_then(Value::as_bool).unwrap_or(false) { 2 } else { 1 };
        let t = self.term(a.get("terminal").and_then(Value::as_str))?;
        let mode = t.mode();
        let Some(press) = keys::mouse(button, MouseAct::Press, r, c, mods, mode) else {
            let mut out = self.after(a, Shot::Text)?;
            out.text = format!("note: the program has mouse reporting off, so this click did nothing (a real terminal would start a text selection).\n{}", out.text);
            return Ok(out);
        };
        let release = keys::mouse(button, MouseAct::Release, r, c, mods, mode).unwrap_or_default();
        for i in 0..times {
            t.write(&press);
            std::thread::sleep(Duration::from_millis(40));
            t.write(&release);
            if i + 1 < times {
                std::thread::sleep(Duration::from_millis(90));
            }
        }
        let mut out = self.after(a, Shot::Text)?;
        out.text = format!("clicked ({r},{c}){}\n{}", if times == 2 { " twice" } else { "" }, out.text);
        Ok(out)
    }

    pub fn drag(&mut self, a: &Value) -> Result<ToolOut> {
        let (r0, c0) = self.target(a, "from_")?;
        let (r1, c1) = self.target(a, "to_")?;
        let t = self.term(a.get("terminal").and_then(Value::as_str))?;
        let mode = t.mode();
        let mods = Mods::default();
        let Some(press) = keys::mouse(Button::Left, MouseAct::Press, r0, c0, mods, mode) else {
            return Ok(ToolOut::text("the program has mouse reporting off: nothing to drag"));
        };
        t.write(&press);
        let steps = (r1.abs_diff(r0)).max(c1.abs_diff(c0)).clamp(1, 60);
        for i in 1..=steps {
            let r = r0 as f32 + (r1 as f32 - r0 as f32) * i as f32 / steps as f32;
            let c = c0 as f32 + (c1 as f32 - c0 as f32) * i as f32 / steps as f32;
            std::thread::sleep(Duration::from_millis(12));
            t.write(&keys::mouse(Button::Left, MouseAct::Drag, r.round() as usize, c.round() as usize, mods, mode).unwrap_or_default());
        }
        std::thread::sleep(Duration::from_millis(30));
        t.write(&keys::mouse(Button::Left, MouseAct::Release, r1, c1, mods, mode).unwrap_or_default());
        let mut out = self.after(a, Shot::Text)?;
        out.text = format!("dragged ({r0},{c0}) → ({r1},{c1})\n{}", out.text);
        Ok(out)
    }

    pub fn scroll(&mut self, a: &Value) -> Result<ToolOut> {
        let (r, c) = self.target(a, "")?;
        let up = a.get("direction").and_then(Value::as_str) != Some("down");
        let n = a.get("amount").and_then(Value::as_u64).unwrap_or(3).clamp(1, 100);
        let t = self.term(a.get("terminal").and_then(Value::as_str))?;
        let mode = t.mode();
        let b = if up { Button::WheelUp } else { Button::WheelDown };
        let Some(ev) = keys::mouse(b, MouseAct::Press, r, c, Mods::default(), mode) else {
            return Ok(ToolOut::text("the program has mouse reporting off: the wheel would scroll the terminal itself"));
        };
        for _ in 0..n {
            t.write(&ev);
            std::thread::sleep(Duration::from_millis(15));
        }
        self.after(a, Shot::Text)
    }

    pub fn resize(&mut self, a: &Value) -> Result<ToolOut> {
        let cols = a.get("cols").and_then(Value::as_u64).context("cols is required")?.clamp(10, 400) as u16;
        let rows = a.get("rows").and_then(Value::as_u64).context("rows is required")?.clamp(3, 200) as u16;
        self.term(a.get("terminal").and_then(Value::as_str))?.resize(cols, rows);
        self.after(a, Shot::Text)
    }

    pub fn set_focus(&mut self, a: &Value) -> Result<ToolOut> {
        let focused = a.get("focused").and_then(Value::as_bool).unwrap_or(true);
        let t = self.term(a.get("terminal").and_then(Value::as_str))?;
        match keys::focus(focused, t.mode()) {
            Some(b) => t.write(b),
            None => return Ok(ToolOut::text("the program hasn't asked for focus reports, so the terminal sends nothing")),
        }
        let mut out = self.after(a, Shot::None)?;
        out.text = format!("terminal is now {}{}", if focused { "focused" } else { "in the background (another window has focus)" }, out.text);
        Ok(out)
    }

    /// Poll the screen (or one row of it) until `text`/`regex` is there, or
    /// with `gone` until it isn't, for at most `timeout`. Returns whether
    /// that happened and how long it took, plus a label for what was sought.
    fn poll_screen(&self, a: &Value, gone: bool, timeout: Duration) -> Result<(bool, Duration, String)> {
        let text = a.get("text").and_then(Value::as_str);
        let re = a.get("regex").and_then(Value::as_str).map(regex::Regex::new).transpose()?;
        anyhow::ensure!(text.is_some() || re.is_some(), "give text or regex");
        let row = a.get("row").and_then(Value::as_u64).map(|r| r as usize);
        let name = a.get("terminal").and_then(Value::as_str);
        let start = Instant::now();
        let met = loop {
            let snap = self.term(name)?.snapshot();
            let hay = match row {
                Some(r) => {
                    anyhow::ensure!(r < snap.rows, "row {r} is off the screen (it has {} rows)", snap.rows);
                    snap.row_text(r)
                }
                None => snap.plain(),
            };
            let present = text.is_some_and(|t| hay.contains(t)) || re.as_ref().is_some_and(|r| r.is_match(&hay));
            if present != gone {
                break true;
            }
            if start.elapsed() > timeout {
                break false;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let mut what = text.map(|t| format!("{t:?}")).or(a.get("regex").and_then(Value::as_str).map(|r| format!("/{r}/"))).unwrap_or_default();
        if let Some(r) = row {
            what += &format!(" on row {r}");
        }
        Ok((met, start.elapsed(), what))
    }

    pub fn wait_for(&mut self, a: &Value) -> Result<ToolOut> {
        let gone = a.get("gone").and_then(Value::as_bool).unwrap_or(false);
        let timeout = Duration::from_millis(a.get("timeout_ms").and_then(Value::as_u64).unwrap_or(10_000).min(120_000));
        let (met, took, what) = self.poll_screen(a, gone, timeout)?;
        let head = if met {
            format!("{what} {} after {:.1}s", if gone { "disappeared" } else { "appeared" }, took.as_secs_f32())
        } else {
            format!("timed out after {:.1}s: {what} {}", timeout.as_secs_f32(), if gone { "is still there" } else { "never appeared" })
        };
        let mut out = self.after(a, Shot::Text)?;
        out.text = format!("{head}\n{}", out.text);
        // A wait that never ends the way it should fails the step, so a
        // replay of it fails too.
        out.is_error = !met;
        Ok(out)
    }

    /// An assertion: text (or a regex) is on screen, or with absent: true
    /// isn't. It gives the screen up to timeout_ms to get there, and fails
    /// the step with the screen shown when it doesn't.
    pub fn expect(&mut self, a: &Value) -> Result<ToolOut> {
        let absent = a.get("absent").and_then(Value::as_bool).unwrap_or(false);
        let timeout = Duration::from_millis(a.get("timeout_ms").and_then(Value::as_u64).unwrap_or(3_000).min(120_000));
        let (met, _, what) = self.poll_screen(a, absent, timeout)?;
        let why = a.get("why").and_then(Value::as_str).map(|w| format!(" ({w})")).unwrap_or_default();
        if met {
            let mut out = self.screen_out(a.get("terminal").and_then(Value::as_str), Shot::parse(a.get("screenshot"), Shot::None))?;
            out.text = format!("✓ {what} {}{why}\n{}", if absent { "is absent" } else { "is on screen" }, out.text);
            return Ok(out);
        }
        // Always show the whole screen on a failure: it's what the reader needs.
        self.looked.remove(a.get("terminal").and_then(Value::as_str).unwrap_or("main"));
        let mut out = self.screen_out(a.get("terminal").and_then(Value::as_str), Shot::parse(a.get("screenshot"), Shot::Text).max_text())?;
        out.text = format!("✗ expected {what} {}{why}, after {:.1}s\n{}", if absent { "to be absent" } else { "on screen" }, timeout.as_secs_f32(), out.text);
        out.is_error = true;
        Ok(out)
    }

    pub fn sleep(&mut self, a: &Value) -> Result<ToolOut> {
        let ms = a.get("ms").and_then(Value::as_u64).unwrap_or(1000).min(120_000);
        std::thread::sleep(Duration::from_millis(ms));
        self.after(&json!({"terminal": a.get("terminal"), "screenshot": a.get("screenshot"), "wait_ms": 0}), Shot::Text)
    }

    pub fn find_text(&mut self, a: &Value) -> Result<ToolOut> {
        let text = a.get("text").and_then(Value::as_str).context("text is required")?;
        let snap = self.term(a.get("terminal").and_then(Value::as_str))?.snapshot();
        let hits = snap.find(text);
        if hits.is_empty() {
            return Ok(ToolOut::text(format!("{text:?} is not on screen")));
        }
        let list: Vec<String> = hits.iter().map(|(r, c)| format!("row {r} col {c}: {}", snap.row_text(*r).trim())).collect();
        Ok(ToolOut::text(format!("{} match(es):\n{}", hits.len(), list.join("\n"))))
    }

    // ---- sweeping ----------------------------------------------------

    fn press_keys(&self, name: Option<&str>, list: &[String]) -> Result<()> {
        let t = self.term(name)?;
        for (i, k) in list.iter().enumerate() {
            t.write(&keys::encode(k, t.mode(), t.keyboard)?);
            if i + 1 < list.len() {
                std::thread::sleep(Duration::from_millis(80));
            }
        }
        Ok(())
    }

    fn open_case(&self, name: Option<&str>, case: &SweepCase, wait: Duration) -> Result<()> {
        self.press_keys(name, &case.keys)?;
        let t = self.term(name)?;
        if let Some(text) = &case.text {
            for ch in text.chars() {
                t.write(&keys::text(&ch.to_string(), false, t.mode()));
                std::thread::sleep(Duration::from_millis(8));
            }
        }
        t.settle(Duration::from_millis(250), wait);
        Ok(())
    }

    fn close_case(&self, name: Option<&str>, case: &SweepCase, wait: Duration) -> Result<()> {
        if !case.reset.is_empty() {
            self.press_keys(name, &case.reset)?;
            self.term(name)?.settle(Duration::from_millis(250), wait);
        }
        Ok(())
    }

    /// Every case at every size, in this session: resize, open the case,
    /// look, close it again. One call instead of a loop of resize, press and
    /// screen, and much faster than a fresh launch per screen.
    pub fn sweep(&mut self, a: &Value) -> Result<ToolOut> {
        let name = a.get("terminal").and_then(Value::as_str).map(str::to_string);
        let name = name.as_deref();
        let sizes: Vec<(u16, u16)> = a
            .get("sizes")
            .and_then(Value::as_array)
            .context("sizes is required, e.g. [\"80x24\", \"80x8\", \"60x6\"]")?
            .iter()
            .map(parse_size)
            .collect::<Result<_>>()?;
        anyhow::ensure!(!sizes.is_empty(), "sizes is empty: give at least one, e.g. [\"80x24\"]");
        let mut cases = Vec::new();
        for (i, c) in a.get("cases").and_then(Value::as_array).cloned().unwrap_or_default().iter().enumerate() {
            let keys = str_list(c.get("keys"));
            let text = c.get("text").and_then(Value::as_str).map(str::to_string);
            let reset = match c.get("reset") {
                Some(r) => str_list(Some(r)),
                None if keys.is_empty() && text.is_none() => Vec::new(),
                None => vec!["esc".to_string()],
            };
            let label = c.get("name").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| if keys.is_empty() { format!("case {}", i + 1) } else { keys.join(" ") });
            cases.push(SweepCase { name: label, keys, text, reset });
        }
        if cases.is_empty() {
            cases.push(SweepCase { name: "screen".into(), keys: Vec::new(), text: None, reset: Vec::new() });
        }
        let open_once = a.get("resize_while_open").and_then(Value::as_bool).unwrap_or(false);
        let wait = Duration::from_millis(a.get("wait_ms").and_then(Value::as_u64).unwrap_or(800));
        let shot = Shot::parse(a.get("screenshot"), Shot::Text);
        let start_size = self.term(name)?.size();

        let mut groups: Vec<Vec<(String, Snapshot)>> = Vec::new();
        // Layout problems of each screen, in the order of groups' screens.
        let mut layout: Vec<Vec<String>> = Vec::new();
        for case in &cases {
            let mut tiles = Vec::new();
            if open_once {
                let (c, r) = sizes[0];
                self.term(name)?.resize(c, r);
                self.term(name)?.settle(Duration::from_millis(250), wait);
                self.open_case(name, case, wait)?;
            }
            for &(c, r) in &sizes {
                let t = self.term(name)?;
                t.resize(c, r);
                t.settle(Duration::from_millis(250), wait);
                // The screen before the case opens, to see what it drew outside its box.
                let opens = !case.keys.is_empty() || case.text.is_some();
                let base = (opens && !open_once).then(|| t.snapshot());
                if !open_once {
                    self.open_case(name, case, wait)?;
                }
                let snap = self.term(name)?.snapshot();
                let mut found = crate::layout::over_borders(&snap);
                if let Some(b) = &base {
                    // A row already flagged for text over a border needs no second line.
                    let rows: Vec<Option<usize>> = found.iter().map(|p| crate::layout::row_of(p)).collect();
                    found.extend(crate::layout::outside_dialog(b, &snap).into_iter().filter(|p| !rows.contains(&crate::layout::row_of(p))));
                }
                layout.push(found);
                tiles.push((format!("{} @ {c}x{r}", case.name), snap));
                if !open_once {
                    self.close_case(name, case, wait)?;
                }
            }
            if open_once {
                self.close_case(name, case, wait)?;
            }
            groups.push(tiles);
        }
        let t = self.term(name)?;
        t.resize(start_size.0, start_size.1);
        t.settle(Duration::from_millis(250), wait);

        // A contact sheet per case, on disk and, when asked, in the result.
        self.runs += 1;
        let dir = self.cfg.out.join("sweeps").join(format!("{:03}", self.runs));
        std::fs::create_dir_all(&dir)?;
        let mut out = ToolOut::default();
        let mut saved = 0;
        for (i, (case, tiles)) in cases.iter().zip(&groups).enumerate() {
            let refs: Vec<(&str, &Snapshot)> = tiles.iter().map(|(l, s)| (l.as_str(), s)).collect();
            if let Ok(png) = crate::render::sheet(&refs, self.cfg.font_px) {
                let slug: String = case.name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
                std::fs::write(dir.join(format!("{:02}-{}.png", i + 1, slug.trim_matches('-'))), &png)?;
                saved += 1;
                if matches!(shot, Shot::Png | Shot::Both) {
                    out.images.push(png);
                }
            }
        }
        let n: usize = groups.iter().map(Vec::len).sum();
        let mut text = format!(
            "swept {} case(s) × {} size(s) = {n} screens{}; back at {}x{}. Contact sheets ({saved}) in {}\n",
            cases.len(),
            sizes.len(),
            if open_once { ", opening each case once and resizing it through every size" } else { "" },
            start_size.0,
            start_size.1,
            dir.display()
        );
        let bad: Vec<&str> = groups.iter().flatten().zip(&layout).filter(|(_, f)| !f.is_empty()).map(|((l, _), _)| l.as_str()).collect();
        if bad.is_empty() {
            text += "Layout: no text drawn over a border or outside a dialog.\n";
        } else {
            text += &format!("Layout: ⚠ problems on {} of {n} screens: {}.\n", bad.len(), bad.join(", "));
        }
        for ((label, snap), found) in groups.iter().flatten().zip(&layout) {
            let warn: String = found.iter().map(|p| format!("⚠ layout: {p}\n")).collect();
            match shot {
                Shot::Text | Shot::Full | Shot::Both => text += &format!("\n── {label}\n{}{warn}", snap.to_text()),
                Shot::Png => text += &format!("── {label}: in the images\n{warn}"),
                Shot::None if !found.is_empty() => text += &format!("── {label}\n{warn}"),
                Shot::None => {}
            }
        }
        out.is_error = a.get("check_layout").and_then(Value::as_bool).unwrap_or(false) && !bad.is_empty();
        text += &self.changes();
        out.text = text;
        out.frames = groups.into_iter().flatten().collect();
        Ok(out)
    }

    // ---- looking -----------------------------------------------------

    pub fn screen(&mut self, a: &Value) -> Result<ToolOut> {
        let shot = match Shot::parse(a.get("format").or(a.get("screenshot")), Shot::Both) {
            Shot::Text => Shot::Full,
            s => s,
        };
        self.screen_out(a.get("terminal").and_then(Value::as_str), shot)
    }

    pub fn snapshot(&self, name: Option<&str>) -> Result<Snapshot> {
        Ok(self.term(name)?.snapshot())
    }

    pub fn png(&self, name: Option<&str>) -> Result<Vec<u8>> {
        crate::render::png(&self.snapshot(name)?, self.cfg.font_px)
    }

    /// The screen of terminal `name` as `shot` asks, and what happened since the last action.
    pub fn screen_out(&mut self, name: Option<&str>, shot: Shot) -> Result<ToolOut> {
        let t = self.term(name)?;
        let snap = t.snapshot();
        let mut out = ToolOut::default();
        let label = name.unwrap_or("main");
        let mut text = String::new();
        if let Some(code) = t.exited() {
            text += &format!("⚠ the program in terminal '{label}' has exited ({}). Its last screen:\n", code.map_or("killed by a signal".into(), |c| format!("code {c}")));
        }
        match shot {
            Shot::Full => text += &self.screen_text(label, &snap, true),
            Shot::Text | Shot::Both => text += &self.screen_text(label, &snap, false),
            Shot::Png => {
                text += &format!("[{}x{} screen of '{label}' as an image]\n", snap.cols, snap.rows);
                self.screen_text(label, &snap, true);
            }
            Shot::None => {}
        }
        text += &self.layout_note(label, &snap);
        if matches!(shot, Shot::Png | Shot::Both) {
            match crate::render::png(&snap, self.cfg.font_px) {
                Ok(png) => out.images.push(png),
                Err(e) => text += &format!("(no image: {e})\n"),
            }
        }
        let changes = self.changes();
        out.text = format!("{text}{changes}");
        Ok(out)
    }

    /// Text drawn over a border (see `layout::over_borders`), told when it
    /// changed since the last look at this terminal: new, or gone.
    fn layout_note(&mut self, label: &str, snap: &Snapshot) -> String {
        let now = crate::layout::over_borders(snap);
        let masked = |v: &[String]| v.iter().map(|p| crate::mask::masked(p)).collect::<Vec<_>>();
        let before = self.layout_said.get(label).cloned().unwrap_or_default();
        if masked(&now) == masked(&before) {
            return String::new();
        }
        self.layout_said.insert(label.to_string(), now.clone());
        if now.is_empty() {
            return "✓ layout: the text drawn over borders before is gone\n".into();
        }
        now.iter().map(|p| format!("⚠ layout: {p}\n")).collect()
    }

    /// The screen as text for the model, and a note that it has seen it.
    /// Whole the first time, after a resize, every `WHOLE_EVERY` looks, or
    /// when most rows changed; otherwise only the rows whose text changed.
    /// The clock, durations and spinners ticking don't count as changes.
    fn screen_text(&mut self, label: &str, snap: &Snapshot, whole: bool) -> String {
        let rows = crate::mask::rows(snap);
        let styles: Vec<u64> = (0..snap.rows).map(|r| snap.row_style(r)).collect();
        let size = (snap.cols, snap.rows);
        let prev = self.looked.get(label).filter(|p| p.size == size);
        let changed: Vec<usize> = match prev {
            Some(p) => (0..snap.rows).filter(|&r| rows[r] != p.rows[r]).collect(),
            None => (0..snap.rows).collect(),
        };
        let whole = whole || prev.is_none_or(|p| p.partial >= WHOLE_EVERY) || changed.len() * 2 > snap.rows;
        let text = if whole {
            snap.to_text()
        } else {
            let p = prev.unwrap();
            let restyled: Vec<usize> = (0..snap.rows).filter(|&r| !changed.contains(&r) && styles[r] != p.styles[r]).collect();
            let mut t = if changed.is_empty() {
                format!("{}\nThe text on screen is as you last saw it (the clock, timers and spinners aside).\n", snap.header())
            } else {
                format!(
                    "{}\n{} of {} rows changed since you last saw the screen; only those are shown, the rest are as they were (screenshot \"full\" shows all).\n{}\n{}",
                    snap.header(),
                    changed.len(),
                    snap.rows,
                    snap.ruler(),
                    snap.rows_text(&changed)
                )
            };
            if !restyled.is_empty() {
                t += &format!("Colors or styles changed, text the same, on row(s) {} (png shows them).\n", crate::compare::ranges(&restyled));
            }
            t
        };
        let partial = if whole { 0 } else { prev.map_or(0, |p| p.partial) + 1 };
        self.looked.insert(label.to_string(), Looked { size, rows, styles, partial });
        text
    }

    // ---- what happened -----------------------------------------------

    /// New lines of a log file since we last looked.
    fn new_lines(&mut self, key: &str, path: &Path) -> Vec<String> {
        let Ok(mut f) = std::fs::File::open(path) else { return Vec::new() };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let pos = self.offsets.get(key).copied().unwrap_or(0).min(len);
        if f.seek(SeekFrom::Start(pos)).is_err() {
            return Vec::new();
        }
        let mut s = String::new();
        let _ = f.read_to_string(&mut s);
        // Keep a partial last line for next time.
        let complete = s.rfind('\n').map_or(0, |i| i + 1);
        self.offsets.insert(key.into(), pos + complete as u64);
        s[..complete].lines().map(str::to_string).collect()
    }

    /// What a person would have noticed since the last action, in a few lines.
    pub fn changes(&mut self) -> String {
        let Some(sb) = &self.sb else { return String::new() };
        let (events, commands, server) = (sb.logs.join("events.jsonl"), sb.logs.join("commands.jsonl"), sb.server_log());
        let mut lines = Vec::new();
        let since = self.last_seen;
        for (name, t) in &self.terms {
            let notices = t.notices_since(since);
            let bells = notices.iter().filter(|n| n.kind == "bell").count();
            if bells > 0 {
                lines.push(format!("🔔 terminal '{name}' rang the bell{}", if bells > 1 { format!(" ×{bells}") } else { String::new() }));
            }
            for n in notices.iter().filter(|n| n.kind != "bell") {
                match n.kind {
                    "title" => lines.push(format!("🏷  terminal '{name}' title → {:?}", n.detail)),
                    "clipboard" => lines.push(format!("📋 terminal '{name}' got an OSC 52 copy: {:?}", clip(&n.detail, 120))),
                    "exit" => lines.push(format!("⏹  the program in terminal '{name}' exited ({})", n.detail)),
                    _ => {}
                }
            }
        }
        for l in self.new_lines("events", &events) {
            if let Some(s) = describe_event(&l) {
                lines.push(s);
            }
        }
        for l in self.new_lines("commands", &commands) {
            if let Some(s) = describe_command(&l) {
                lines.push(s);
            }
        }
        for l in self.new_lines("server", &server) {
            let low = l.to_lowercase();
            if low.contains("panic") || low.contains("error") || low.contains("warn") || low.contains("fail") {
                lines.push(format!("📜 server log: {}", clip(&l, 200)));
            }
        }
        self.last_seen = Instant::now();
        if lines.is_empty() {
            return String::new();
        }
        if lines.len() > 25 {
            let more = lines.len() - 25;
            lines.truncate(25);
            lines.push(format!("… and {more} more (see observations)"));
        }
        format!("\nsince the last action:\n  {}\n", lines.join("\n  "))
    }

    pub fn observations(&mut self, a: &Value) -> Result<ToolOut> {
        let sb = self.sb()?;
        let what = a.get("what").and_then(Value::as_str).unwrap_or("all");
        let last = a.get("last").and_then(Value::as_u64).unwrap_or(30) as usize;
        let tail = |p: &Path, f: &dyn Fn(&str) -> Option<String>| -> Vec<String> {
            let s = std::fs::read_to_string(p).unwrap_or_default();
            let v: Vec<String> = s.lines().filter_map(f).collect();
            v[v.len().saturating_sub(last)..].to_vec()
        };
        let mut out = String::new();
        let mut section = |title: &str, v: Vec<String>| {
            out += &format!("## {title}\n");
            if v.is_empty() {
                out += "(nothing)\n";
            }
            for l in v {
                out += &format!("{l}\n");
            }
            out += "\n";
        };
        let cmds = sb.logs.join("commands.jsonl");
        let kind_of = |l: &str| serde_json::from_str::<Value>(l).ok().and_then(|v| v.get("prog").and_then(Value::as_str).map(str::to_string)).unwrap_or_default();
        if matches!(what, "all" | "events") {
            section("agent events (trix events --json)", tail(&sb.logs.join("events.jsonl"), &|l| describe_event(l).or(Some(l.to_string()))));
        }
        if matches!(what, "all" | "notifications") {
            section("notifications", tail(&cmds, &|l| {
                let k = kind_of(l);
                (k == "notify-command" || k == "notify-send" || k == "osascript").then(|| describe_command(l)).flatten()
            }));
        }
        if matches!(what, "all" | "sounds") {
            section("sounds", tail(&cmds, &|l| matches!(kind_of(l).as_str(), "afplay" | "pw-play" | "paplay").then(|| describe_command(l)).flatten()));
        }
        if matches!(what, "all" | "clipboard") {
            let mut v = tail(&cmds, &|l| matches!(kind_of(l).as_str(), "pbcopy" | "wl-copy" | "xclip" | "xsel").then(|| describe_command(l)).flatten());
            for (name, t) in &self.terms {
                for n in t.notices_since(Instant::now() - Duration::from_secs(86_400)) {
                    if n.kind == "clipboard" {
                        v.push(format!("OSC 52 in terminal '{name}': {:?}", clip(&n.detail, 300)));
                    }
                }
            }
            section("clipboard", v);
        }
        if matches!(what, "all" | "bells") {
            let mut v = Vec::new();
            for (name, t) in &self.terms {
                let all = t.notices_since(Instant::now() - Duration::from_secs(86_400));
                let bells = all.iter().filter(|n| n.kind == "bell").count();
                v.push(format!("terminal '{name}': {bells} bell(s); title now {:?}; running {}", t.title(), t.command));
            }
            section("terminals", v);
        }
        if matches!(what, "all" | "server_log") {
            section("server log (last lines)", tail(&sb.server_log(), &|l| Some(l.to_string())));
        }
        if matches!(what, "all" | "fake_logs") {
            let mut v = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&sb.fake_dir) {
                let mut logs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "log")).collect();
                logs.sort();
                for p in logs {
                    let id = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    for l in tail(&p, &|l| Some(l.to_string())) {
                        let short = serde_json::from_str::<Value>(&l).ok().map(|v| {
                            // `ran: false`: a Codex hook nobody trusted yet, or an event trix gave no hook.
                            let ran = if v["ran"] == false { " (not run)" } else { "" };
                            format!("agent id {id}: hook {}{ran} {}", v["hook"].as_str().unwrap_or("?"), clip(&v["payload"].to_string(), 120))
                        });
                        v.push(short.unwrap_or(l));
                    }
                }
            }
            section("hooks the fake agents fired", v);
        }
        Ok(ToolOut::text(out))
    }

    // ---- trix itself -------------------------------------------------

    pub fn run_trix(&mut self, a: &Value) -> Result<ToolOut> {
        let args = str_list(a.get("args"));
        let stdin = a.get("stdin").and_then(Value::as_str);
        let timeout = Duration::from_millis(a.get("timeout_ms").and_then(Value::as_u64).unwrap_or(15_000).min(120_000));
        let r = self.sb()?.run_trix(&args, stdin, timeout)?;
        let mut s = format!(
            "$ trix {}\nexit code: {}{}\n",
            args.join(" "),
            r.code.map_or("none (killed)".into(), |c| c.to_string()),
            if r.timed_out { format!(" — timed out after {:.0}s and was killed", timeout.as_secs_f32()) } else { String::new() }
        );
        if !r.stdout.is_empty() {
            s += &format!("--- stdout ---\n{}\n", clip(&r.stdout, 20_000));
        }
        if !r.stderr.is_empty() {
            s += &format!("--- stderr ---\n{}\n", clip(&r.stderr, 8_000));
        }
        // Let the screen catch up with what the command did.
        if let Ok(t) = self.term(None) {
            t.settle(Duration::from_millis(200), Duration::from_millis(800));
        }
        s += &self.changes();
        Ok(ToolOut::text(s))
    }

    pub fn kill_server(&mut self, a: &Value) -> Result<ToolOut> {
        let sig = if a.get("signal").and_then(Value::as_str) == Some("TERM") { libc::SIGTERM } else { libc::SIGKILL };
        let sb = self.sb()?;
        // The server is the `trix server` process with the sandbox's runtime dir.
        let ps = std::process::Command::new("ps").args(["-axo", "pid=,command="]).output()?;
        let list = String::from_utf8_lossy(&ps.stdout).into_owned();
        let trix = sb.trix.display().to_string();
        let before = self.term(None).map(Terminal::fingerprint).ok();
        let mut killed = Vec::new();
        for line in list.lines() {
            let line = line.trim();
            let Some((pid, cmd)) = line.split_once(' ') else { continue };
            if cmd.trim() == format!("{trix} server") || cmd.trim().ends_with("trix server") {
                let pid: i32 = pid.trim().parse().unwrap_or(0);
                if pid > 0 && process_in_sandbox(pid, &sb.root) {
                    unsafe {
                        libc::kill(pid, sig);
                    }
                    killed.push(pid);
                }
            }
        }
        if killed.is_empty() {
            return Ok(ToolOut::error("no trix server for this sandbox was found"));
        }
        // Wait for the server to be gone, then for the client to show it.
        let start = Instant::now();
        let alive = || killed.iter().any(|&pid| unsafe { libc::kill(pid, 0) } == 0);
        while alive() && start.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let still = if alive() { " It was still running 2 s later." } else { "" };
        if let (Some(f), Ok(t)) = (before, self.term(None)) {
            t.wait_change(f, Duration::from_millis(1500));
        }
        let mut out = self.after(&json!({"wait_ms": 1500, "screenshot": a.get("screenshot")}), Shot::Text)?;
        out.text = format!("sent {} to the trix server (pid {killed:?}), like a crash.{still}\n{}", if sig == libc::SIGKILL { "SIGKILL" } else { "SIGTERM" }, out.text);
        Ok(out)
    }

    // ---- files -------------------------------------------------------

    pub fn read_file(&mut self, a: &Value) -> Result<ToolOut> {
        let p = a.get("path").and_then(Value::as_str).context("path is required")?;
        let path = self.sb()?.resolve(p)?;
        if path.is_dir() {
            let mut names: Vec<String> = std::fs::read_dir(&path)?
                .flatten()
                .map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if e.path().is_dir() { format!("{n}/") } else { n }
                })
                .collect();
            names.sort();
            return Ok(ToolOut::text(format!("{}:\n{}", path.display(), names.join("\n"))));
        }
        let max = a.get("max_bytes").and_then(Value::as_u64).unwrap_or(20_000) as usize;
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let s = String::from_utf8_lossy(&bytes[..bytes.len().min(max)]).into_owned();
        Ok(ToolOut::text(format!("{} ({} bytes{}):\n{s}", path.display(), bytes.len(), if bytes.len() > max { ", cut short" } else { "" })))
    }

    pub fn write_file(&mut self, a: &Value) -> Result<ToolOut> {
        let p = a.get("path").and_then(Value::as_str).context("path is required")?;
        let content = a.get("content").and_then(Value::as_str).context("content is required")?;
        let path = self.sb()?.resolve(p)?;
        let config = self.sb()?.is_trix_config(&path);
        // The reload messages already on screen, so an old one doesn't count.
        let before = self.term(None).map(|t| reload_toasts(&t.snapshot())).ok();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if a.get("append").and_then(Value::as_bool).unwrap_or(false) {
            std::fs::OpenOptions::new().create(true).append(true).open(&path)?.write_all(content.as_bytes())?;
        } else {
            std::fs::write(&path, content)?;
        }
        // trix looks at its config about once a second and says what it made
        // of a change in a toast: wait for a new one. Anything else that
        // changes on screen meanwhile (an agent finishing) isn't it.
        let mut note = String::new();
        if config && let (Some(before), Ok(t)) = (before, self.term(None)) {
            let start = Instant::now();
            let mut said = None;
            while said.is_none() && start.elapsed() < Duration::from_millis(2500) {
                said = reload_toasts(&t.snapshot()).into_iter().find(|x| !before.contains(x)).map(|(_, toast)| toast);
                if said.is_none() {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            note = match said {
                Some(toast) => format!("; trix said {toast:?} {:.1}s later", start.elapsed().as_secs_f32()),
                None if !before.is_empty() => "; trix showed no new reload message within 2.5 s (the one from before was still up, and a repeat of it looks the same)".into(),
                None => "; trix showed no reload message within 2.5 s (it shows none when the settings come out the same)".into(),
            };
        }
        let mut out = self.after(&json!({"wait_ms": 500, "screenshot": a.get("screenshot")}), Shot::Text)?;
        out.text = format!("wrote {} ({} bytes){note}\n{}", path.display(), content.len(), out.text);
        Ok(out)
    }

    // ---- fake agents -------------------------------------------------

    pub fn fake_agent(&mut self, a: &Value) -> Result<ToolOut> {
        let sb = self.sb()?;
        anyhow::ensure!(sb.mode == AgentMode::Fake, "fake_agent only works with launch(agents: \"fake\")");
        let who = a.get("agent").map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).context("agent is required (its name or number)")?;
        let action = a.get("action").and_then(Value::as_str).context("action is required")?;
        let id = self.resolve_agent_id(&who)?;
        let sock = self.sb()?.fake_dir.join(format!("{id}.sock"));
        let mut cmd = json!({"do": action});
        for k in ["text", "key", "percent", "event", "payload"] {
            if let Some(v) = a.get(k) {
                cmd[k] = v.clone();
            }
        }
        let reply = ask_fake(&sock, &cmd).with_context(|| format!("agent '{who}' (id {id}) isn't a fake agent that's still running"))?;
        let mut out = self.after(a, Shot::Text)?;
        out.text = format!("fake agent '{who}' (id {id}) → {reply}\n{}", out.text);
        Ok(out)
    }

    fn resolve_agent_id(&self, who: &str) -> Result<String> {
        let sb = self.sb()?;
        let r = sb.run_trix(&["ls".into(), "--json".into()], None, Duration::from_secs(5))?;
        let v: Value = serde_json::from_str(&r.stdout).unwrap_or(Value::Null);
        let mut agents = Vec::new();
        collect_agents(&v, &mut agents);
        for ag in &agents {
            let name = ag.get("name").and_then(Value::as_str).unwrap_or("");
            let number = ag.get("number").and_then(Value::as_u64).map(|n| n.to_string()).unwrap_or_default();
            if (name == who || number == who)
                && let Some(id) = ag.get("id").and_then(Value::as_u64)
            {
                return Ok(id.to_string());
            }
        }
        // Fall back to the fakes' own registry (original names).
        if let Ok(entries) = std::fs::read_dir(&sb.fake_dir) {
            for e in entries.flatten() {
                if e.path().extension().is_some_and(|x| x == "json")
                    && let Ok(s) = std::fs::read_to_string(e.path())
                    && let Ok(reg) = serde_json::from_str::<Value>(&s)
                    && (reg["name"].as_str() == Some(who) || reg["id"].as_str() == Some(who))
                {
                    return Ok(reg["id"].as_str().unwrap_or("").to_string());
                }
            }
        }
        let names: Vec<String> = agents.iter().map(|a| format!("{} ({})", a["name"].as_str().unwrap_or("?"), a["number"])).collect();
        bail!("no agent '{who}'; trix has: {}", if names.is_empty() { "none".into() } else { names.join(", ") })
    }

    // ---- reporting ---------------------------------------------------

    pub fn report_issue(&mut self, a: &Value) -> Result<ToolOut> {
        let title = a.get("title").and_then(Value::as_str).context("title is required")?.to_string();
        let n = self.issues.len() + 1;
        let slug: String = title.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect::<String>().split('-').filter(|s| !s.is_empty()).take(7).collect::<Vec<_>>().join("-");
        let dir = self.cfg.out.join("issues").join(format!("{n:03}-{slug}"));
        std::fs::create_dir_all(&dir)?;
        let term = a.get("terminal").and_then(Value::as_str);
        let mut files = Vec::new();
        if let Ok(snap) = self.snapshot(term) {
            std::fs::write(dir.join("screen.txt"), snap.to_text())?;
            files.push("screen.txt");
            if let Ok(png) = crate::render::png(&snap, self.cfg.font_px) {
                std::fs::write(dir.join("screen.png"), png)?;
                files.push("screen.png");
            }
        }
        let repro = json!({"trix": self.trix.display().to_string(), "actions": self.actions});
        std::fs::write(dir.join("repro.json"), serde_json::to_string_pretty(&repro)?)?;
        files.push("repro.json");
        let mut issue = a.clone();
        issue["id"] = json!(n);
        issue["dir"] = json!(dir.display().to_string());
        issue["files"] = json!(files);
        issue["trix"] = json!(self.trix.display().to_string());
        issue["trix_version"] = json!(self.trix_version());
        std::fs::write(dir.join("issue.md"), issue_markdown(&issue))?;
        self.issues.push(issue);
        self.write_report()?;
        Ok(ToolOut::text(format!(
            "recorded issue #{n}: {title}\n{} (screen.png, screen.txt, and repro.json: the {} actions since launch, replayable with `trix-probe replay`).",
            dir.display(),
            self.actions.len()
        )))
    }

    /// `trix --version` of the trix this session runs.
    pub fn trix_version(&self) -> String {
        version_of(&self.trix)
    }

    pub fn write_report(&self) -> Result<()> {
        std::fs::create_dir_all(&self.cfg.out)?;
        std::fs::write(self.cfg.out.join("issues.json"), serde_json::to_string_pretty(&self.issues)?)?;
        // Every binary an issue was found in (a session can launch several).
        let mut binaries: Vec<(String, String)> = Vec::new();
        for i in &self.issues {
            let b = (i["trix"].as_str().unwrap_or("").to_string(), i["trix_version"].as_str().unwrap_or("unknown").to_string());
            if !b.0.is_empty() && !binaries.contains(&b) {
                binaries.push(b);
            }
        }
        if binaries.is_empty() {
            binaries.push((self.trix.display().to_string(), self.trix_version()));
        }
        let list: Vec<String> = binaries.iter().map(|(p, v)| format!("`{p}` ({v})")).collect();
        let mut md = format!("# trix-probe report\n\ntrix: {}\n\n", list.join(", "));
        if self.issues.is_empty() {
            md += "No issues reported.\n";
        } else {
            let rank = |s: &str| match s {
                "critical" => 0,
                "high" => 1,
                "medium" => 2,
                _ => 3,
            };
            let mut sorted: Vec<&Value> = self.issues.iter().collect();
            sorted.sort_by_key(|i| (rank(i["severity"].as_str().unwrap_or("low")), i["id"].as_u64().unwrap_or(0)));
            md += "| # | severity | kind | title |\n| --- | --- | --- | --- |\n";
            for i in &sorted {
                md += &format!("| {} | {} | {} | {} |\n", i["id"], i["severity"].as_str().unwrap_or("?"), i["kind"].as_str().unwrap_or("?"), i["title"].as_str().unwrap_or("").replace('|', "\\|"));
            }
            md += "\n";
            for i in sorted {
                let dir = PathBuf::from(i["dir"].as_str().unwrap_or(""));
                let rel = dir.strip_prefix(&self.cfg.out).map(|p| p.display().to_string()).unwrap_or_else(|_| dir.display().to_string());
                md += &issue_markdown(i).replacen("screen.png", &format!("{rel}/screen.png"), 1);
                md += "\n---\n\n";
            }
        }
        std::fs::write(self.cfg.out.join("report.md"), md)?;
        Ok(())
    }

    pub fn save_screenshot(&mut self, name: Option<&str>) -> Result<PathBuf> {
        self.shots += 1;
        let dir = self.cfg.out.join("screens");
        std::fs::create_dir_all(&dir)?;
        let p = dir.join(format!("{:04}.png", self.shots));
        std::fs::write(&p, self.png(name)?)?;
        Ok(p)
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// trix's toasts after it reloads config.toml, as (row, toast text): "✓
/// config reloaded", "✓ config OK again", "✕ config: <error> — kept the
/// previous settings".
fn reload_toasts(s: &Snapshot) -> Vec<(usize, String)> {
    static TOAST: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"[✓✕] config( reloaded| OK again|:)").unwrap());
    (0..s.rows)
        .filter_map(|r| {
            let row = s.row_text(r);
            TOAST.find(&row).map(|m| (r, row[m.start()..].trim().to_string()))
        })
        .collect()
}

fn issue_markdown(i: &Value) -> String {
    let s = |k: &str| i.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut md = format!("## #{} {}\n\n**{}** · {} · confidence: {}\n\n", i["id"], s("title"), s("severity"), s("kind"), if s("confidence").is_empty() { "unstated".into() } else { s("confidence") });
    if let Some(steps) = i.get("steps").and_then(Value::as_array) {
        md += "**Steps**\n\n";
        for (n, st) in steps.iter().enumerate() {
            md += &format!("{}. {}\n", n + 1, st.as_str().unwrap_or(&st.to_string()));
        }
        md += "\n";
    }
    for (k, label) in [("expected", "Expected"), ("actual", "Actual"), ("notes", "Notes")] {
        if !s(k).is_empty() {
            md += &format!("**{label}:** {}\n\n", s(k));
        }
    }
    md += &format!("{}{} · ", s("trix_version"),if s("trix").is_empty() { String::new() } else { format!(" (`{}`)", s("trix")) });
    md += "![screen](screen.png)\n";
    md
}

fn ask_fake(sock: &Path, cmd: &Value) -> Result<String> {
    let mut s = std::os::unix::net::UnixStream::connect(sock)?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(s, "{cmd}")?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn collect_agents(v: &Value, out: &mut Vec<Value>) {
    match v {
        Value::Array(a) => a.iter().for_each(|x| collect_agents(x, out)),
        Value::Object(o) => {
            if let Some(Value::Array(agents)) = o.get("agents") {
                out.extend(agents.iter().cloned());
            }
            if let Some(s) = o.get("sessions") {
                collect_agents(s, out);
            }
        }
        _ => {}
    }
}

fn process_in_sandbox(pid: i32, root: &Path) -> bool {
    // `ps eww` shows the environment on macOS and Linux.
    let out = std::process::Command::new("ps").args(["eww", "-p", &pid.to_string()]).output();
    out.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&root.display().to_string()))
}

/// A trix binary named by a tool call: `~/` is your home, a relative path
/// starts where trix-probe was started.
pub fn resolve_trix(s: &str) -> Result<PathBuf> {
    let s = s.trim();
    let p = match s.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(s),
    };
    let p = std::fs::canonicalize(&p).with_context(|| format!("trix binary {} not found", p.display()))?;
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(&p)?;
    anyhow::ensure!(meta.is_file() && meta.permissions().mode() & 0o111 != 0, "{} is not an executable file", p.display());
    Ok(p)
}

/// `trix --version` of a binary, or "unknown".
pub fn version_of(trix: &Path) -> String {
    std::process::Command::new(trix)
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// A terminal size as "80x24", [80, 24] or {"cols": 80, "rows": 24}.
fn parse_size(v: &Value) -> Result<(u16, u16)> {
    let pair = match v {
        Value::String(s) => {
            let (c, r) = s.trim().split_once(['x', 'X', '×']).with_context(|| format!("size {s:?} isn't COLSxROWS, e.g. \"80x24\""))?;
            (c.trim().parse::<u64>().ok(), r.trim().parse::<u64>().ok())
        }
        Value::Array(a) if a.len() == 2 => (a[0].as_u64(), a[1].as_u64()),
        Value::Object(o) => (o.get("cols").and_then(Value::as_u64), o.get("rows").and_then(Value::as_u64)),
        _ => (None, None),
    };
    match pair {
        (Some(c), Some(r)) => Ok((c.clamp(10, 400) as u16, r.clamp(3, 200) as u16)),
        _ => bail!("size {v} isn't COLSxROWS, e.g. \"80x24\""),
    }
}

pub fn str_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect(),
        Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn mods_of(v: Option<&Value>) -> Mods {
    let mut m = Mods::default();
    for s in str_list(v) {
        match s.as_str() {
            "shift" => m.shift = true,
            "alt" | "option" => m.alt = true,
            "ctrl" | "control" => m.ctrl = true,
            _ => {}
        }
    }
    m
}

pub fn clip(s: &str, max: usize) -> String {
    let s = s.trim_end();
    if s.chars().count() <= max { s.to_string() } else { format!("{}… ({} chars)", s.chars().take(max).collect::<String>(), s.chars().count()) }
}

fn describe_event(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    let ev = v.get("event")?.as_str()?;
    let name = v.get("name").and_then(Value::as_str).unwrap_or("?");
    let state = v.get("state").and_then(Value::as_str).unwrap_or("");
    let act = v.get("activity").and_then(Value::as_str).filter(|s| !s.is_empty()).map(|s| format!(" ({s})")).unwrap_or_default();
    Some(match ev {
        "present" => return None,
        "state" => format!("◆ agent '{name}': {} → {state}{act}", v.get("prev").and_then(Value::as_str).unwrap_or("?")),
        "spawned" => format!("◆ agent '{name}' spawned ({}) {state}", v.get("kind").and_then(Value::as_str).unwrap_or("?")),
        "closed" => format!("◆ agent '{name}' closed"),
        "renamed" => format!("◆ agent renamed to '{name}'"),
        "restarted" => format!("◆ agent '{name}' restarted"),
        other => format!("◆ {other} '{name}' {state}{act}"),
    })
}

fn describe_command(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    let prog = v.get("prog")?.as_str()?;
    let args: Vec<String> = v.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let stdin = v.get("stdin").and_then(Value::as_str).unwrap_or("");
    let env = |k: &str| v.get("env").and_then(|e| e.get(k)).and_then(Value::as_str).unwrap_or("").to_string();
    Some(match prog {
        "afplay" | "pw-play" | "paplay" => {
            let file = args.iter().rfind(|a| !a.starts_with('-')).map(|a| Path::new(a).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or(a.clone())).unwrap_or_else(|| "stdin".into());
            format!("♪ sound played via {prog}: {file}")
        }
        "notify-command" => format!("📣 notification ({}): {} — {}", env("TRIX_NOTIFY_EVENT"), env("TRIX_NOTIFY_TITLE"), env("TRIX_NOTIFY_BODY")),
        "notify-send" | "osascript" => format!("📣 desktop notification via {prog}: {}", clip(&args.join(" "), 160)),
        "pbcopy" | "wl-copy" | "xclip" | "xsel" => format!("📋 copied via {prog}: {:?}", clip(stdin, 120)),
        other => format!("{other} {}", args.join(" ")),
    })
}

/// `trix-probe log-cmd <prog> [args…]`: what the shims call.
pub fn log_cmd(prog: &str, args: &[String]) {
    let mut stdin = String::new();
    // Players and copiers read stdin; the notify command gets JSON on it.
    if prog != "afplay" && prog != "osascript" && prog != "notify-send" {
        let _ = std::io::stdin().take(1 << 20).read_to_string(&mut stdin);
    }
    let env: serde_json::Map<String, Value> = std::env::vars().filter(|(k, _)| k.starts_with("TRIX_NOTIFY") || k == "TRIX_AGENT_NAME").map(|(k, v)| (k, Value::String(v))).collect();
    let line = json!({
        "at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64),
        "prog": prog, "args": args, "stdin": clip(&stdin, 4000), "env": env,
    });
    if let Some(root) = std::env::var_os("TRIX_PROBE_ROOT")
        && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(PathBuf::from(root).join("logs/commands.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}
