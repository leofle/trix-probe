//! A throwaway world for one trix under test: its own HOME, XDG folders,
//! runtime dir (so its sockets never meet the trix you use), a demo git repo
//! to start agents in, and shims on PATH:
//!
//! - fake agents (`claude`, `codex`, …) that run `trix-probe fake-agent`;
//! - loggers in place of `afplay`, `pw-play`, `notify-send`, `osascript`,
//!   `pbcopy`, `wl-copy`, so sounds, notifications and clipboard copies are
//!   recorded instead of reaching your desktop.
//!
//! Everything lives under `/tmp/tp-<id>` (short, because unix socket paths are
//! limited to ~104 bytes on macOS) and is removed on teardown.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    /// Scriptable fake agents stand in for every agent kind.
    Fake,
    /// Real agents from your PATH, with your real HOME (so they're logged in).
    Real,
}

pub struct SandboxOpts {
    pub agents: AgentMode,
    /// Replaces the default config.toml (a `[notify]` block is added if missing).
    pub config_toml: Option<String>,
    pub extra_env: BTreeMap<String, String>,
    pub keep: bool,
    /// Use this root instead of a fresh random one. `compare` gives both of
    /// its runs the same root, one after the other, so paths on screen match.
    pub root: Option<PathBuf>,
}

pub struct Sandbox {
    pub root: PathBuf,
    pub home: PathBuf,
    pub runtime: PathBuf,
    pub logs: PathBuf,
    pub fake_dir: PathBuf,
    pub work: PathBuf,
    pub env: BTreeMap<String, String>,
    pub trix: PathBuf,
    pub mode: AgentMode,
    pub keep: bool,
}

/// Agent kinds and the command trix runs for each.
pub const AGENT_COMMANDS: &[(&str, &str)] = &[
    ("claude", "claude"),
    ("codex", "codex"),
    ("grok", "grok"),
    ("opencode", "opencode"),
    ("cursor", "cursor-agent"),
    ("copilot", "copilot"),
    ("muse", "muse"),
];

/// Programs trix calls for sounds, notifications and the clipboard.
const LOGGED: &[&str] = &["afplay", "pw-play", "paplay", "notify-send", "osascript", "pbcopy", "wl-copy", "xclip", "xsel"];

impl Sandbox {
    pub fn create(trix: &Path, probe: &Path, opts: &SandboxOpts) -> Result<Sandbox> {
        let root = match &opts.root {
            Some(r) => {
                // Left over from a run of ours that didn't finish tearing down.
                if r.exists() && std::fs::read_to_string(r.join("owner.pid")).is_ok_and(|p| p.trim() == std::process::id().to_string()) {
                    let _ = std::fs::remove_dir_all(r);
                }
                std::fs::create_dir(r).with_context(|| format!("creating the sandbox {}", r.display()))?;
                r.clone()
            }
            None => unique_root()?,
        };
        let real_home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
        let home = match opts.agents {
            AgentMode::Fake => root.join("home"),
            AgentMode::Real => real_home.clone(),
        };
        // trix's own files always stay in the sandbox, even with a real HOME.
        let xdg = root.join("xdg");
        let config_dir = xdg.join("config");
        let state_dir = xdg.join("state");
        let runtime = root.join("run");
        let bin = root.join("bin");
        let logs = root.join("logs");
        let fake_dir = root.join("f");
        let work = root.join("work");
        for d in [&config_dir.join("trix"), &state_dir, &xdg.join("data"), &xdg.join("cache"), &bin, &logs, &fake_dir, &root.join("tmp"), &work] {
            std::fs::create_dir_all(d)?;
        }
        if opts.agents == AgentMode::Fake {
            std::fs::create_dir_all(&home)?;
        }
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(&runtime)?;
        }
        std::fs::write(root.join("owner.pid"), std::process::id().to_string())?;
        std::fs::write(root.join("trix.path"), trix.display().to_string())?;

        // Shims.
        let probe_q = sh_quote(&probe.display().to_string());
        let path_now = std::env::var("PATH").unwrap_or_default();
        for (kind, cmd) in AGENT_COMMANDS {
            let script = match opts.agents {
                AgentMode::Fake => Some(format!("#!/bin/sh\nexec {probe_q} fake-agent --kind {kind} -- \"$@\"\n")),
                // A real agent that isn't installed would make trix install it
                // (brew, npm…). Stop that: this pane just reports it.
                AgentMode::Real if which(cmd, &path_now).is_none() => Some(format!(
                    "#!/bin/sh\necho 'trix-probe: {cmd} is not installed, and the probe does not let trix install it.'\nexit 127\n"
                )),
                AgentMode::Real => None,
            };
            if let Some(s) = script {
                write_exec(&bin.join(cmd), &s)?;
            }
        }
        for prog in LOGGED {
            write_exec(&bin.join(prog), &format!("#!/bin/sh\nexec {probe_q} log-cmd {prog} \"$@\"\n"))?;
        }

        // config.toml: notifications go to a logger, so the probe sees them.
        let notify_block = format!(
            "[notify]\n# trix-probe: every notification is logged for the tester.\ncommand = [{}, \"log-cmd\", \"notify-command\"]\n",
            toml_str(&probe.display().to_string())
        );
        let config = match &opts.config_toml {
            Some(c) if c.contains("[notify]") => c.clone(),
            Some(c) => format!("{c}\n\n{notify_block}"),
            None => format!("# Written by trix-probe for this sandbox.\n\n{notify_block}"),
        };
        std::fs::write(config_dir.join("trix/config.toml"), config)?;

        let mut env = BTreeMap::new();
        let mut put = |k: &str, v: String| {
            env.insert(k.to_string(), v);
        };
        put("HOME", home.display().to_string());
        put("XDG_CONFIG_HOME", config_dir.display().to_string());
        put("XDG_STATE_HOME", state_dir.display().to_string());
        put("XDG_DATA_HOME", xdg.join("data").display().to_string());
        put("XDG_CACHE_HOME", xdg.join("cache").display().to_string());
        put("XDG_RUNTIME_DIR", runtime.display().to_string());
        put("TMPDIR", root.join("tmp").display().to_string());
        put("PATH", format!("{}:{}", bin.display(), path_now));
        put("TERM", "xterm-256color".into());
        put("COLORTERM", "truecolor".into());
        put("TERM_PROGRAM", "trix-probe".into());
        // Desktop notifications would pop up on your screen: they're logged
        // through config.toml's notify command instead.
        put("TRIX_NO_NOTIFY", "1".into());
        put("TRIX_PROBE_ROOT", root.display().to_string());
        put("TRIX_PROBE_EXE", probe.display().to_string());
        if std::env::var("LANG").map(|l| l.is_empty()).unwrap_or(true) {
            put("LANG", "en_US.UTF-8".into());
        }
        if opts.agents == AgentMode::Fake {
            let shell = ["/bin/zsh", "/bin/bash", "/bin/sh"].into_iter().find(|s| Path::new(s).exists()).unwrap_or("/bin/sh");
            put("SHELL", shell.into());
            // A plain, predictable prompt for shell panes.
            std::fs::write(home.join(".zshrc"), "PROMPT='%~ %# '\n")?;
            std::fs::write(home.join(".bashrc"), "PS1='\\w \\$ '\n")?;
            put("GIT_CONFIG_NOSYSTEM", "1".into());
        }
        for (k, v) in &opts.extra_env {
            put(k, v.clone());
        }

        let sb = Sandbox {
            root,
            home,
            runtime,
            logs,
            fake_dir,
            work,
            env,
            trix: trix.to_path_buf(),
            mode: opts.agents,
            keep: opts.keep,
        };
        sb.make_demo_repo()?;
        Ok(sb)
    }

    /// The folder agents start in: a small git repo with one commit and an
    /// uncommitted change, so branch and diff have something to show.
    pub fn demo_repo(&self) -> PathBuf {
        self.work.join("demo")
    }

    fn make_demo_repo(&self) -> Result<()> {
        let repo = self.demo_repo();
        std::fs::create_dir_all(repo.join("src"))?;
        std::fs::create_dir_all(self.work.join("other"))?;
        std::fs::write(repo.join("README.md"), "# demo\n\nA tiny project for trix-probe's agents to work in.\n")?;
        std::fs::write(
            repo.join("src/main.rs"),
            "fn main() {\n    println!(\"hello from the demo\");\n}\n\nfn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )?;
        let git = |args: &[&str]| -> bool {
            Command::new("git")
                .args(["-c", "user.name=trix-probe", "-c", "user.email=probe@example.invalid", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
                .args(args)
                .current_dir(&repo)
                .env_clear()
                .envs(&self.env)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        if git(&["init", "-q"]) && git(&["add", "-A"]) && git(&["commit", "-q", "-m", "demo: first commit"]) {
            std::fs::write(repo.join("src/main.rs"), "fn main() {\n    println!(\"hello from the demo, changed\");\n}\n\nfn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\nfn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n")?;
        }
        Ok(())
    }

    /// A `Command` for `trix <args>` in this sandbox (not in a terminal).
    pub fn trix_command(&self, args: &[String]) -> Command {
        let mut c = Command::new(&self.trix);
        c.args(args).current_dir(self.demo_repo()).env_clear().envs(&self.env);
        c
    }

    /// Run `trix <args>` and collect its output, giving up after `timeout`.
    pub fn run_trix(&self, args: &[String], stdin: Option<&str>, timeout: Duration) -> Result<CmdOutput> {
        run_with_timeout(self.trix_command(args), stdin, timeout)
    }

    pub fn server_log(&self) -> PathBuf {
        self.runtime.join("trix/server.log")
    }

    /// Resolve a path the tester gave: relative paths start at the sandbox
    /// root, `~` is the sandbox HOME. Anything outside the sandbox is refused.
    /// Whether `p` (from `resolve`) is one of trix's own config files, which
    /// it reloads when they change.
    pub fn is_trix_config(&self, p: &Path) -> bool {
        p.starts_with(normalize(&self.root.join("xdg/config/trix")))
    }

    pub fn resolve(&self, p: &str) -> Result<PathBuf> {
        let path = if let Some(rest) = p.strip_prefix("~/") {
            self.home.join(rest)
        } else if p == "~" {
            self.home.clone()
        } else if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            self.root.join(p)
        };
        let norm = normalize(&path);
        let inside = norm.starts_with(normalize(&self.root)) || (self.mode == AgentMode::Fake && norm.starts_with(normalize(&self.home)));
        anyhow::ensure!(inside, "{} is outside the sandbox ({})", norm.display(), self.root.display());
        Ok(norm)
    }

    pub fn teardown(&self) {
        // Stop the server and every agent in it; then the files.
        let _ = self.run_trix(&["kill".into(), "--all".into()], None, Duration::from_secs(5));
        kill_fakes(&self.fake_dir);
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

/// Kill the fake agents still registered in `dir`, in case trix left any.
fn kill_fakes(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        if e.path().extension().is_some_and(|x| x == "json")
            && let Ok(s) = std::fs::read_to_string(e.path())
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&s)
            && let Some(pid) = v.get("pid").and_then(|p| p.as_i64())
        {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
}

/// Remove sandboxes whose probe died without cleaning up.
pub fn sweep_stale() {
    let Ok(entries) = std::fs::read_dir("/tmp") else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("tp-") {
            continue;
        }
        let root = e.path();
        let Some(pid) = std::fs::read_to_string(root.join("owner.pid")).ok().and_then(|s| s.trim().parse::<i32>().ok()) else { continue };
        if alive(pid) {
            continue;
        }
        if let Ok(trix) = std::fs::read_to_string(root.join("trix.path")) {
            let mut c = Command::new(trix.trim());
            c.args(["kill", "--all"])
                .env_clear()
                .env("PATH", std::env::var("PATH").unwrap_or_default())
                .env("HOME", root.join("home"))
                .env("XDG_RUNTIME_DIR", root.join("run"))
                .env("XDG_STATE_HOME", root.join("xdg/state"))
                .env("XDG_CONFIG_HOME", root.join("xdg/config"));
            let _ = run_with_timeout(c, None, Duration::from_secs(5));
        }
        kill_fakes(&root.join("f"));
        let _ = std::fs::remove_dir_all(&root);
    }
}

pub fn alive(pid: i32) -> bool {
    let ok = unsafe { libc::kill(pid, 0) == 0 };
    ok || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// A sandbox path no one is using, reserved and then released, for
/// `SandboxOpts::root`.
pub fn fresh_root() -> Result<PathBuf> {
    let r = unique_root()?;
    std::fs::remove_dir(&r)?;
    Ok(r)
}

fn unique_root() -> Result<PathBuf> {
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    for i in 0..1000u128 {
        let n = (seed.wrapping_mul(6364136223846793005).wrapping_add(i * 1442695040888963407) ^ std::process::id() as u128) % (36u128.pow(6));
        let root = PathBuf::from(format!("/tmp/tp-{}", base36(n, 6)));
        match std::fs::create_dir(&root) {
            Ok(()) => return Ok(root),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context("creating the sandbox in /tmp"),
        }
    }
    anyhow::bail!("could not find a free sandbox name in /tmp")
}

fn base36(mut n: u128, width: usize) -> String {
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut s = vec![b'0'; width];
    for i in (0..width).rev() {
        s[i] = digits[(n % 36) as usize];
        n /= 36;
    }
    String::from_utf8(s).unwrap()
}

fn write_exec(path: &Path, body: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

pub fn which(cmd: &str, path: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    path.split(':').filter(|d| !d.is_empty()).map(|d| Path::new(d).join(cmd)).find(|p| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    // /tmp is /private/tmp on macOS: compare real paths. A file about to be
    // written doesn't exist yet, so resolve the part of it that does.
    let mut base = out.clone();
    let mut rest = Vec::new();
    loop {
        if let Ok(mut real) = std::fs::canonicalize(&base) {
            real.extend(rest.iter().rev());
            return real;
        }
        match base.file_name() {
            Some(n) => rest.push(n.to_os_string()),
            None => return out,
        }
        if !base.pop() {
            return out;
        }
    }
}

#[derive(Debug)]
pub struct CmdOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

pub fn run_with_timeout(mut cmd: Command, stdin: Option<&str>, timeout: Duration) -> Result<CmdOutput> {
    use std::io::{Read, Write};
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("starting command")?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let input = input.to_string();
        std::thread::spawn(move || {
            let _ = pipe.write_all(input.as_bytes());
        });
    }
    let mut out_pipe = child.stdout.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    let out_t = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = out_pipe.read_to_end(&mut s);
        s
    });
    let err_t = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = err_pipe.read_to_end(&mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Some(s);
        }
        if Instant::now() > deadline {
            timed_out = true;
            let _ = child.kill();
            break child.wait().ok();
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = String::from_utf8_lossy(&out_t.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_t.join().unwrap_or_default()).into_owned();
    Ok(CmdOutput { code: status.and_then(|s| s.code()), stdout, stderr, timed_out })
}
