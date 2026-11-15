//! Regression suite: replays every tests/regressions/*.json against a real
//! trix and fails if any step fails (an `expect` that doesn't hold, a
//! `wait_for` that times out, an action that errors). Skipped when no trix
//! is found (set TRIX_PROBE_TRIX, or put trix on PATH). TRIX_PROBE_ONLY=<substring>
//! runs just the files whose names contain it.
//!
//! To add one: reproduce the bug with trix-probe, add `expect` steps for
//! what should be on screen, and save it here as {"title", "notes", "actions"}.
//!
//! A fix that isn't in a trix release yet gets "fixed_in": the first release
//! with it (e.g. "1.0.8"). Such a file is skipped against an older trix, so
//! testing the latest release stays green, unless TRIX_PROBE_UNRELEASED=1:
//! set that for a build from source (master, a branch), which still reports
//! the last release's version but has the fix.

use std::path::PathBuf;
use std::process::Command;

/// "trix 1.0.7" → [1, 0, 7]
fn version(s: &str) -> Option<Vec<u64>> {
    let v = s.split_whitespace().map(|w| w.trim_start_matches('v')).find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    v.split('.').map(|p| p.parse().ok()).collect()
}

fn trix_version(trix: &str) -> Option<Vec<u64>> {
    let out = Command::new(trix).arg("--version").output().ok()?;
    version(&String::from_utf8_lossy(&out.stdout))
}

#[test]
fn versions_parse_and_order() {
    assert_eq!(version("trix 1.0.7"), Some(vec![1, 0, 7]));
    assert_eq!(version("v1.0.8"), Some(vec![1, 0, 8]));
    assert!(version("trix 1.0.7") < version("1.0.8"));
    assert!(version("trix 1.1.0") > version("1.0.8"));
    assert!(version("trix 1.0.10") > version("1.0.9"));
}

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

    let unreleased = std::env::var("TRIX_PROBE_UNRELEASED").is_ok_and(|v| v == "1");
    let have = trix_version(&trix);
    let out_root = std::env::temp_dir().join(format!("trix-probe-regressions-{}", std::process::id()));
    let mut failures = Vec::new();
    let mut ran = 0;
    // One at a time: each starts its own trix, and timing matters.
    for f in &files {
        let name = f.file_stem().unwrap().to_string_lossy().to_string();
        let spec: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap_or_else(|e| panic!("{name}: not valid JSON: {e}"));
        if let Some(need) = spec.get("fixed_in").and_then(|v| v.as_str()) {
            let need_v = version(need).unwrap_or_else(|| panic!("{name}: fixed_in {need:?} isn't a version"));
            if !unreleased && have.as_ref().is_none_or(|h| *h < need_v) {
                eprintln!("skip  {name}: fixed in trix {need}, this is {} (TRIX_PROBE_UNRELEASED=1 runs it)", have.as_ref().map_or("an unknown version".into(), |h| h.iter().map(u64::to_string).collect::<Vec<_>>().join(".")));
                continue;
            }
        }
        ran += 1;
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
    eprintln!("{ran} of {} regressions ran", files.len());
    assert!(failures.is_empty(), "{} of {ran} regressions failed against {trix}:\n\n{}", failures.len(), failures.join("\n"));
}
