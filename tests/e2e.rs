//! End to end against a real trix: a fake agent asks for permission while
//! you're looking at another pane, trix shows needs-you and notifies, you
//! jump there with alt+a and approve. Skipped when no trix is found (set
//! TRIX_PROBE_TRIX, or put trix on PATH).

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
fn needs_you_round_trip() {
    let Some(trix) = trix() else {
        eprintln!("skipped: no trix found");
        return;
    };
    let dir = std::env::temp_dir().join(format!("trix-probe-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let repro = dir.join("repro.json");
    std::fs::write(
        &repro,
        r#"{"actions":[
            {"tool":"launch","args":{"args":["claude","shell"],"cols":120,"rows":32}},
            {"tool":"press","args":{"keys":["alt+1"]}},
            {"tool":"type_text","args":{"text":"hello","enter":true}},
            {"tool":"wait_for","args":{"text":"fake reply","timeout_ms":20000}},
            {"tool":"press","args":{"keys":["alt+2"]}},
            {"tool":"fake_agent","args":{"agent":"claude","action":"permission","wait_ms":3000}},
            {"tool":"wait_for","args":{"text":"needs you","timeout_ms":10000}},
            {"tool":"press","args":{"keys":["alt+a"]}},
            {"tool":"press","args":{"keys":["1"],"wait_ms":3000}},
            {"tool":"wait_for","args":{"text":"Removed dist","timeout_ms":10000}}
        ]}"#,
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_trix-probe"))
        .args(["replay", repro.to_str().unwrap(), "--trix", &trix, "--out", dir.join("out").to_str().unwrap()])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success(), "replay failed:\n{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    for expect in [
        "agent 'claude': idle → working",
        "working → done",
        "→ needs-you (approve Bash rm -rf dist)",
        "notification (needs-you)",
        "needs-you → working",
        "\"Removed dist\" appeared",
    ] {
        assert!(stdout.contains(expect), "missing {expect:?} in:\n{stdout}");
    }
    assert!(!stdout.contains("timed out"), "a wait timed out:\n{stdout}");
}
