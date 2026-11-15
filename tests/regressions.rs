//! Regression suite: replays every tests/regressions/*.json against a real
//! trix and fails if any step fails (an `expect` that doesn't hold, a
//! `wait_for` that times out, an action that errors). Skipped when no trix
//! is found (set TRIX_PROBE_TRIX, or put trix on PATH). TRIX_PROBE_ONLY=<substring>
//! runs just the files whose names contain it.
//!
//! To add one: reproduce the bug with trix-probe, add `expect` steps for
//! what should be on screen, and save it here as {"title", "notes", "actions"}.

use std::path::PathBuf;
use std::process::Command;

fn trix() -> Option<String> {
    if let Ok(t) = std::env::var("TRIX_PROBE_TRIX") {
        return Some(t);
    }
    let out = Command::new("sh").args(["-c", "command -v trix"]).output().ok()?;
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!p.is_empty()).then_some(p)
}

#[test]
fn regressions() {
    let Some(trix) = trix() else {
        eprintln!("skipped: no trix found");
        return;
    };
    let only = std::env::var("TRIX_PROBE_ONLY").ok();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/regressions");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter(|p| only.as_deref().is_none_or(|o| p.file_name().unwrap().to_string_lossy().contains(o)))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no regression files in {}", dir.display());

    let out_root = std::env::temp_dir().join(format!("trix-probe-regressions-{}", std::process::id()));
    let mut failures = Vec::new();
    // One at a time: each starts its own trix, and timing matters.
    for f in &files {
        let name = f.file_stem().unwrap().to_string_lossy().to_string();
        let out = Command::new(env!("CARGO_BIN_EXE_trix-probe"))
            .args(["replay", f.to_str().unwrap(), "--trix", &trix, "--out", out_root.join(&name).to_str().unwrap()])
            .output()
            .unwrap();
        if out.status.success() {
            eprintln!("ok    {name}");
        } else {
            eprintln!("FAIL  {name}");
            failures.push(format!(
                "══ {name} ══\n{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&out_root);
    assert!(failures.is_empty(), "{} of {} regressions failed against {trix}:\n\n{}", failures.len(), files.len(), failures.join("\n"));
}
