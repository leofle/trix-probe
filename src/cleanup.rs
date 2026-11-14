//! Tear sandboxes down even when the probe is stopped by a signal (an MCP
//! client usually ends its servers with SIGTERM), so no trix server or fake
//! agent outlives the probe. Whatever still slips through is swept by
//! `sandbox::sweep_stale` the next time a probe starts.

use crate::sandbox::Sandbox;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

struct Entry {
    root: PathBuf,
    keep: bool,
    kill: std::process::Command,
}

static LIVE: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
static SIGNAL: AtomicI32 = AtomicI32::new(0);

pub fn remember(sb: &Sandbox) {
    let kill = sb.trix_command(&["kill".into(), "--all".into()]);
    LIVE.lock().unwrap().push(Entry { root: sb.root.clone(), keep: sb.keep, kill });
}

pub fn forget_all() {
    LIVE.lock().unwrap().clear();
}

fn run_all() {
    let entries: Vec<Entry> = std::mem::take(&mut *LIVE.lock().unwrap());
    for e in entries {
        let _ = crate::sandbox::run_with_timeout(e.kill, None, Duration::from_secs(4));
        if !e.keep {
            let _ = std::fs::remove_dir_all(&e.root);
        }
    }
}

extern "C" fn on_signal(sig: i32) {
    SIGNAL.store(sig, Ordering::SeqCst);
}

pub fn install_signal_handlers() {
    unsafe {
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::signal(s, on_signal as *const () as libc::sighandler_t);
        }
    }
    std::thread::spawn(|| {
        loop {
            let sig = SIGNAL.load(Ordering::SeqCst);
            if sig != 0 {
                run_all();
                std::process::exit(128 + sig);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
}
