//! trix-probe: lets an LLM use trix the way a person would, to find bugs and
//! UX problems. See README.md.

mod cleanup;
mod compare;
mod explore;
mod fake;
mod keys;
mod mcp;
mod probe;
mod render;
mod sandbox;
mod term;
mod tools;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use probe::{Probe, ProbeConfig};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "trix-probe", version, about = "Lets an LLM use trix like a person would, to find bugs and UX problems")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Clone)]
struct Common {
    /// The trix binary to test (default: $TRIX_PROBE_TRIX, then `trix` on PATH).
    #[arg(long)]
    trix: Option<PathBuf>,
    /// Where reports and screenshots go (default: ~/.local/share/trix-probe/runs/<time>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Keep each sandbox in /tmp after the session, for poking at by hand.
    #[arg(long)]
    keep: bool,
    /// Font size for screenshots, in pixels.
    #[arg(long, default_value_t = 16.0)]
    font_px: f32,
}

#[derive(Subcommand)]
#[allow(clippy::enum_variant_names)]
enum Cmd {
    /// Serve the probe's tools over MCP on stdio (for Claude Code and other MCP clients).
    Mcp(Common),
    /// Let Claude test trix on its own and write a report.
    Explore {
        #[command(flatten)]
        common: Common,
        /// What to concentrate on; repeat for several (e.g. --focus settings --focus "needs-you flow").
        #[arg(long)]
        focus: Vec<String>,
        /// The most tool calls the tester may make.
        #[arg(long, default_value_t = 120)]
        max_steps: usize,
        /// Stop (after a chance to report) once the estimated cost passes this many dollars.
        #[arg(long, default_value_t = 5.0)]
        max_usd: f64,
        #[arg(long, default_value = "claude-opus-5-5")]
        model: String,
        /// low, medium, high, xhigh or max.
        #[arg(long, default_value = "high")]
        effort: String,
        /// trix's documentation (its README.md), so the tester can check behavior against it.
        #[arg(long)]
        docs: Option<PathBuf>,
        /// Don't ask the API to retry a declined turn on a fallback model.
        #[arg(long)]
        no_fallbacks: bool,
    },
    /// Run a repro.json from a report again and show where it ends up. Exits 1 if any step fails
    /// (an action errors, a wait_for times out, an expect does not hold).
    Replay {
        repro: PathBuf,
        #[command(flatten)]
        common: Common,
        /// Use the trix binary recorded in the repro rather than --trix or PATH.
        #[arg(long)]
        recorded_trix: bool,
    },
    /// Run the same steps on two trix binaries and show where their screens differ.
    Compare {
        /// A repro.json, or a file of the same shape: {"actions": [{"tool": "launch", "args": {…}}, …]}.
        steps: PathBuf,
        /// The baseline trix (default: --trix, $TRIX_PROBE_TRIX, then `trix` on PATH).
        #[arg(long)]
        before: Option<PathBuf>,
        /// The trix to set against it.
        #[arg(long)]
        after: PathBuf,
        /// A regex to mask on both sides before comparing; repeat for several.
        #[arg(long)]
        ignore: Vec<String>,
        /// Also print both whole screens of each that differs.
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        common: Common,
    },
    /// Launch trix, press some keys, print the screen and save a PNG: a quick look by hand.
    Try {
        #[command(flatten)]
        common: Common,
        /// Keys to press, in order (e.g. --keys alt+n --keys esc).
        #[arg(long)]
        keys: Vec<String>,
        /// Text to type after the keys.
        #[arg(long)]
        text: Option<String>,
        #[arg(long, default_value_t = 140)]
        cols: u16,
        #[arg(long, default_value_t = 40)]
        rows: u16,
        /// dark or light terminal colors.
        #[arg(long, default_value = "dark")]
        colors: String,
        /// Wait this long before the final screenshot, in ms.
        #[arg(long, default_value_t = 1500)]
        wait: u64,
        /// Where to save the PNG.
        #[arg(long, short = 'o', default_value = "trix-probe.png")]
        png: PathBuf,
        /// Arguments for trix (after --).
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Check that everything the probe needs is in place.
    Doctor(Common),
    /// (internal) The fake agent the sandbox's shims run.
    #[command(hide = true)]
    FakeAgent {
        #[arg(long)]
        kind: String,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// (internal) Record a call to a program trix runs (afplay, pbcopy, …).
    #[command(hide = true)]
    LogCmd {
        prog: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let r = match cli.cmd {
        Cmd::FakeAgent { kind, args } => fake::run(kind, args),
        Cmd::LogCmd { prog, args } => {
            probe::log_cmd(&prog, &args);
            Ok(())
        }
        Cmd::Mcp(c) => setup(&c, "mcp").and_then(mcp::serve),
        Cmd::Explore { common, focus, max_steps, max_usd, model, effort, docs, no_fallbacks } => setup(&common, "explore").and_then(|p| {
            explore::run(p, explore::ExploreOpts { model, effort, focus, max_steps, max_usd, docs, fallbacks: !no_fallbacks })
        }),
        Cmd::Replay { repro, common, recorded_trix } => replay(repro, common, recorded_trix),
        Cmd::Compare { steps, before, after, ignore, all, common } => compare_cmd(steps, before, after, ignore, all, common),
        Cmd::Try { common, keys, text, cols, rows, colors, wait, png, args } => try_it(common, keys, text, (cols, rows), colors, wait, png, args),
        Cmd::Doctor(c) => doctor(c),
    };
    if let Err(e) = r {
        eprintln!("trix-probe: {e:#}");
        std::process::exit(1);
    }
}

/// Clear what would leak the terminal we run in into the trix under test:
/// trix's own pane variables (we may well be inside trix), multiplexers, and
/// terminal identities that change how programs behave.
fn scrub_env() {
    let drop_prefix = ["TMUX", "ZELLIJ", "GHOSTTY", "KITTY", "ITERM", "WEZTERM", "ALACRITTY", "CLAUDE_CODE", "VSCODE"];
    let drop_exact = ["VTE_VERSION", "TERM_PROGRAM_VERSION", "TERM_SESSION_ID", "COLORFGBG", "WINDOWID", "STY", "CLAUDECODE", "OPENCODE_CONFIG_CONTENT", "OPENCODE_CLI_CONFIG_CONTENT", "GROK_THEME", "NO_COLOR"];
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy().into_owned();
        let trix_var = k.starts_with("TRIX") && !k.starts_with("TRIX_PROBE");
        if trix_var || drop_prefix.iter().any(|p| k.starts_with(p)) || drop_exact.contains(&k.as_str()) {
            // Safe: called first thing, before any thread starts.
            unsafe { std::env::remove_var(&k) };
        }
    }
}

fn find_trix(c: &Common) -> Result<PathBuf> {
    let p = c
        .trix
        .clone()
        .or_else(|| std::env::var_os("TRIX_PROBE_TRIX").map(PathBuf::from))
        .or_else(|| sandbox::which("trix", &std::env::var("PATH").unwrap_or_default()))
        .context("can't find trix: pass --trix /path/to/trix (or set TRIX_PROBE_TRIX)")?;
    let p = std::fs::canonicalize(&p).with_context(|| format!("trix binary {} not found", p.display()))?;
    Ok(p)
}

fn timestamp() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!("{:04}{:02}{:02}-{:02}{:02}{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

fn default_out(kind: &str) -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    base.join("trix-probe/runs").join(format!("{}-{kind}", timestamp()))
}

fn setup(c: &Common, kind: &str) -> Result<Probe> {
    // Before any thread exists (remove_var isn't thread-safe).
    scrub_env();
    let trix = find_trix(c)?;
    sandbox::sweep_stale();
    cleanup::install_signal_handlers();
    let out = c.out.clone().unwrap_or_else(|| default_out(kind));
    Ok(Probe::new(ProbeConfig {
        trix,
        exe: std::env::current_exe()?.canonicalize()?,
        out,
        keep_sandboxes: c.keep,
        font_px: c.font_px,
    }))
}

fn replay(repro: PathBuf, mut common: Common, recorded: bool) -> Result<()> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&repro)?).context("reading the repro")?;
    if recorded && let Some(t) = v.get("trix").and_then(Value::as_str) {
        common.trix = Some(PathBuf::from(t));
    }
    let mut p = setup(&common, "replay")?;
    if !recorded {
        // The binary given here wins over any a recorded launch names.
        p.trix_override = Some(p.cfg.trix.clone());
    }
    let actions = v.get("actions").and_then(Value::as_array).cloned().unwrap_or_default();
    anyhow::ensure!(!actions.is_empty(), "no actions in {}", repro.display());
    let mut failed = Vec::new();
    for (i, a) in actions.iter().enumerate() {
        let name = a["tool"].as_str().unwrap_or("");
        let mut args = a.get("args").cloned().unwrap_or(json!({}));
        // Text only while replaying; the final screen is saved as a PNG.
        if args.is_object() {
            args["screenshot"] = json!("none");
        }
        println!("── [{}/{}] {name} {}", i + 1, actions.len(), probe::clip(&args.to_string(), 200));
        let out = tools::call(&mut p, name, &args);
        let text = out.text.trim();
        if out.is_error {
            println!("✗ step {} failed", i + 1);
            failed.push(format!("{}: {name} — {}", i + 1, text.lines().next().unwrap_or("")));
            // Show the failure in full: an expect's screen is the evidence.
            if !text.is_empty() {
                println!("{}", probe::clip(text, 6000));
            }
            if name == "launch" {
                break; // nothing after it can run
            }
        } else if !text.is_empty() {
            println!("{}", probe::clip(text, 1500));
        }
    }
    if p.snapshot(None).is_ok() {
        println!("── final screen");
        println!("{}", p.snapshot(None)?.to_text());
        let png = p.save_screenshot(None)?;
        println!("saved {}", png.display());
    }
    p.teardown();
    if failed.is_empty() {
        println!("── all {} steps passed", actions.len());
        Ok(())
    } else {
        println!("── {} of {} steps failed:", failed.len(), actions.len());
        for f in &failed {
            println!("  {f}");
        }
        std::process::exit(1);
    }
}

#[allow(clippy::too_many_arguments)]
fn try_it(common: Common, keys: Vec<String>, text: Option<String>, (cols, rows): (u16, u16), colors: String, wait: u64, png: PathBuf, args: Vec<String>) -> Result<()> {
    let mut p = setup(&common, "try")?;
    let out = tools::call(&mut p, "launch", &json!({"args": args, "cols": cols, "rows": rows, "terminal_colors": colors, "screenshot": "none"}));
    anyhow::ensure!(!out.is_error, "{}", out.text);
    if !keys.is_empty() {
        let out = tools::call(&mut p, "press", &json!({"keys": keys, "screenshot": "none"}));
        anyhow::ensure!(!out.is_error, "{}", out.text);
    }
    if let Some(t) = text {
        tools::call(&mut p, "type_text", &json!({"text": t, "screenshot": "none"}));
    }
    std::thread::sleep(std::time::Duration::from_millis(wait));
    let snap = p.snapshot(None)?;
    print!("{}", snap.to_text());
    print!("{}", p.changes());
    std::fs::write(&png, render::png(&snap, common.font_px)?)?;
    println!("saved {}", png.display());
    p.teardown();
    Ok(())
}

fn compare_cmd(steps: PathBuf, before: Option<PathBuf>, after: PathBuf, ignore: Vec<String>, all: bool, mut common: Common) -> Result<()> {
    if before.is_some() {
        common.trix = before;
    }
    let mut p = setup(&common, "compare")?;
    let args = json!({
        "repro": steps.display().to_string(),
        "before": p.cfg.trix.display().to_string(),
        "after": after.display().to_string(),
        "ignore": ignore,
        "show": if all { "all" } else { "diff" },
    });
    let out = tools::call(&mut p, "compare", &args);
    println!("{}", out.text.trim_end());
    if !out.is_error {
        println!("saved in {}", compare::out_dir(&p).display());
    }
    anyhow::ensure!(!out.is_error, "compare failed");
    Ok(())
}

fn doctor(c: Common) -> Result<()> {
    scrub_env();
    let ok = |b: bool| if b { "ok  " } else { "FAIL" };
    let trix = find_trix(&c);
    match &trix {
        Ok(t) => {
            let v = std::process::Command::new(t).arg("--version").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
            println!("{} trix: {} ({})", ok(true), t.display(), if v.is_empty() { "version unknown".into() } else { v });
        }
        Err(e) => println!("{} trix: {e}", ok(false)),
    }
    let snap_font = render::png(
        &term::Snapshot {
            cols: 1,
            rows: 1,
            cells: vec![vec![term::Cell { text: "A".into(), fg: term::Palette::dark().fg, bg: term::Palette::dark().bg, bold: false, underline: false, strike: false, hidden: false, wide: false, spacer: false }]],
            cursor: (0, 0),
            cursor_visible: false,
            cursor_shape: alacritty_terminal::vte::ansi::CursorShape::Block,
            title: None,
            fg: term::Palette::dark().fg,
            bg: term::Palette::dark().bg,
        },
        16.0,
    );
    println!("{} screenshot font{}", ok(snap_font.is_ok()), snap_font.err().map(|e| format!(": {e}")).unwrap_or_default());
    let git = std::process::Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success());
    println!("{} git (for the demo repo){}", ok(git), if git { "" } else { ": not found, agents start in a plain folder" });
    let tmp = std::fs::metadata("/tmp").is_ok_and(|m| m.is_dir());
    println!("{} /tmp for sandboxes", ok(tmp));
    let key = std::env::var("ANTHROPIC_API_KEY").is_ok_and(|k| !k.is_empty());
    println!("{} Claude API key (only `explore` needs one){}", if key { "ok  " } else { "--  " }, if key { "" } else { ": ANTHROPIC_API_KEY unset; `ant auth login` also works" });
    if let Ok(t) = trix {
        let mut p = Probe::new(ProbeConfig { trix: t, exe: std::env::current_exe()?.canonicalize()?, out: std::env::temp_dir().join("trix-probe-doctor"), keep_sandboxes: false, font_px: c.font_px });
        let out = tools::call(&mut p, "launch", &json!({"screenshot": "text", "cols": 100, "rows": 30}));
        let started = !out.is_error && !out.text.contains("has exited");
        println!("{} trix starts in a sandbox{}", ok(started), if started { String::new() } else { format!(":\n{}", out.text) });
        p.teardown();
    }
    Ok(())
}
