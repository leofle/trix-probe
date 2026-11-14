//! `compare`: the same steps on two trix binaries, one after the other, and
//! where their screens and observations differ. It's how you check a branch
//! against main: what got fixed, and what changed that shouldn't have.
//!
//! Both runs get the same sandbox path, so paths on screen match. What
//! changes from run to run anyway (the clock, "idle 3s", spinners,
//! sparklines, the fake agents' session ids, pids) is masked before
//! comparing, keeping each line's width.

use crate::probe::{Probe, Shot, ToolOut, clip, resolve_trix, version_of};
use crate::term::Snapshot;
use anyhow::{Context, Result};
use regex::Regex;
use serde_json::{Value, json};

/// What a step left behind on one side.
struct Seen {
    notes: Vec<String>,
    frames: Vec<(String, Snapshot)>,
}

enum Mask {
    /// Digits become '#'.
    Digits,
    /// Every character becomes '#'.
    All,
    /// Every character becomes this one.
    Char(char),
}

fn default_masks() -> Vec<(Regex, Mask)> {
    let re = |s: &str| Regex::new(s).unwrap();
    vec![
        (re(r"\b\d{1,2}:\d{2}(:\d{2})?\b"), Mask::Digits),
        (re(r"\b\d+(\.\d+)?(ms|s|m|h|d)\b"), Mask::Digits),
        (re(r"(session|pid) \[?[0-9a-f]+\]?"), Mask::All),
        (re(r"[\x{2800}-\x{28FF}]"), Mask::Char('⠿')),
        (re(r"[\x{2581}-\x{2588}]"), Mask::Char('▁')),
    ]
}

fn mask(line: &str, masks: &[(Regex, Mask)]) -> String {
    let mut s = line.to_string();
    for (re, how) in masks {
        s = re
            .replace_all(&s, |c: &regex::Captures| {
                let m = &c[0];
                match how {
                    Mask::Digits => m.chars().map(|ch| if ch.is_ascii_digit() { '#' } else { ch }).collect::<String>(),
                    Mask::All => "#".repeat(m.chars().count()),
                    Mask::Char(x) => x.to_string().repeat(m.chars().count()),
                }
            })
            .into_owned();
    }
    s
}

/// The screen as lines to compare: a line for size, cursor and title, then the rows.
fn lines_of(s: &Snapshot, masks: &[(Regex, Mask)]) -> Vec<String> {
    let cur = if s.cursor_visible { format!("cursor row {} col {}", s.cursor.0, s.cursor.1) } else { "cursor hidden".into() };
    let mut v = vec![mask(&format!("{}x{} · {cur} · title {:?}", s.cols, s.rows, s.title.as_deref().unwrap_or("")), masks)];
    v.extend((0..s.rows).map(|r| mask(&s.row_text(r), masks)));
    v
}

fn load_steps(p: &Probe, a: &Value) -> Result<Vec<Value>> {
    let steps = if let Some(s) = a.get("steps") {
        s.as_array().context("steps is a list of {\"tool\": …, \"args\": {…}}, starting with launch")?.clone()
    } else if let Some(path) = a.get("repro").and_then(Value::as_str) {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?).with_context(|| format!("{path} isn't JSON"))?;
        v.get("actions").and_then(Value::as_array).cloned().with_context(|| format!("{path} has no \"actions\""))?
    } else {
        p.actions.clone()
    };
    anyhow::ensure!(!steps.is_empty(), "nothing to compare: give steps, or a repro, or launch and do something in this session first");
    anyhow::ensure!(steps[0].get("tool").and_then(Value::as_str) == Some("launch"), "the steps must start with launch (each side starts its own trix)");
    Ok(steps)
}

fn describe(step: &Value) -> String {
    let tool = step.get("tool").and_then(Value::as_str).unwrap_or("?");
    let mut args = step.get("args").cloned().unwrap_or(json!({}));
    if let Some(o) = args.as_object_mut() {
        for k in ["screenshot", "trix"] {
            o.remove(k);
        }
    }
    let a = args.to_string();
    if a == "{}" { tool.to_string() } else { format!("{tool} {}", clip(&a, 100)) }
}

fn run_side(q: &mut Probe, steps: &[Value]) -> Vec<Seen> {
    let mut seen = Vec::new();
    for step in steps {
        let tool = step.get("tool").and_then(Value::as_str).unwrap_or("");
        if matches!(tool, "compare" | "report_issue" | "teardown") {
            seen.push(Seen { notes: vec![format!("({tool} skipped)")], frames: Vec::new() });
            continue;
        }
        let mut args = step.get("args").cloned().unwrap_or(json!({}));
        if let Some(o) = args.as_object_mut() {
            o.insert("screenshot".into(), json!("none"));
            if tool == "screen" {
                o.insert("format".into(), json!("none"));
            }
        }
        let out = crate::tools::call(q, tool, &args);
        let mut notes: Vec<String> = out.text.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).map(str::to_string).collect();
        if tool == "launch" {
            // The sandbox line names the binary; the header already does.
            notes.retain(|l| !l.starts_with("sandbox ") && !l.starts_with("agents start in "));
        }
        if out.is_error {
            notes.insert(0, "error".into());
        }
        let frames = if !out.frames.is_empty() {
            out.frames
        } else {
            let term = if tool == "open_terminal" { args.get("name") } else { args.get("terminal") }.and_then(Value::as_str);
            q.snapshot(term).map(|s| vec![(String::new(), s)]).unwrap_or_default()
        };
        seen.push(Seen { notes, frames });
    }
    q.teardown();
    seen
}

pub fn compare(p: &mut Probe, a: &Value) -> Result<ToolOut> {
    let before = match a.get("before").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
        Some(s) => resolve_trix(s)?,
        None => p.cfg.trix.clone(),
    };
    let after = resolve_trix(a.get("after").and_then(Value::as_str).context("after is required: the trix binary to set against before")?)?;
    let steps = load_steps(p, a)?;
    let mut masks = if a.get("raw").and_then(Value::as_bool).unwrap_or(false) { Vec::new() } else { default_masks() };
    for pat in crate::probe::str_list(a.get("ignore")) {
        masks.push((Regex::new(&pat).with_context(|| format!("ignore pattern {pat:?}"))?, Mask::All));
    }
    let show_all = a.get("show").and_then(Value::as_str) == Some("all");
    let shot = Shot::parse(a.get("screenshot"), Shot::Text);

    p.runs += 1;
    let dir = p.cfg.out.join("compare").join(format!("{:03}", p.runs));
    std::fs::create_dir_all(&dir)?;
    let root = crate::sandbox::fresh_root()?;
    let mut sides = Vec::new();
    for (label, trix) in [("before", &before), ("after", &after)] {
        let mut cfg = p.cfg.clone();
        cfg.trix = trix.clone();
        cfg.out = dir.join(label);
        cfg.keep_sandboxes = false;
        let mut q = Probe::new(cfg);
        q.trix_override = Some(trix.clone());
        q.fixed_root = Some(root.clone());
        sides.push(run_side(&mut q, &steps));
    }
    let (b, f) = (&sides[0], &sides[1]);
    // The binaries and each side's output folder differ by design: don't count them.
    let swaps = [
        (before.display().to_string(), "<trix>"),
        (after.display().to_string(), "<trix>"),
        (dir.join("before").display().to_string(), "<out>"),
        (dir.join("after").display().to_string(), "<out>"),
    ];
    let note = |l: &str| mask(&swaps.iter().fold(l.to_string(), |s, (from, to)| s.replace(from.as_str(), to)), &masks);

    let mut body = String::new();
    let mut same_steps = Vec::new();
    let (mut screens, mut differ, mut sheets) = (0usize, 0usize, 0usize);
    let mut out = ToolOut::default();
    for (i, step) in steps.iter().enumerate() {
        let (sb, sf) = (&b[i], &f[i]);
        let mut part = String::new();
        let n = sb.frames.len().max(sf.frames.len());
        for k in 0..n {
            screens += 1;
            let (fb, ff) = (sb.frames.get(k), sf.frames.get(k));
            let label = fb.or(ff).map(|(l, _)| l.clone()).filter(|l| !l.is_empty()).map(|l| format!(" · {l}")).unwrap_or_default();
            let (Some((_, xb)), Some((_, xf))) = (fb, ff) else {
                differ += 1;
                part += &format!("  screen{label}: only {} has it\n", if fb.is_some() { "before" } else { "after" });
                continue;
            };
            let (lb, lf) = (lines_of(xb, &masks), lines_of(xf, &masks));
            let rows: Vec<usize> = (0..lb.len().max(lf.len())).filter(|&r| lb.get(r) != lf.get(r)).collect();
            if rows.is_empty() {
                continue;
            }
            differ += 1;
            part += &format!("  screen{label}: {} of {} lines differ\n", rows.len(), lb.len().max(lf.len()));
            for r in rows {
                let name = if r == 0 { "  ·".to_string() } else { format!("{:>3}", r - 1) };
                part += &format!("  {name} before│{}\n  {name}  after│{}\n", lb.get(r).map_or("(none)", String::as_str), lf.get(r).map_or("(none)", String::as_str));
            }
            if show_all {
                part += &format!("  ── before{label}\n{}  ── after{label}\n{}", xb.to_text(), xf.to_text());
            }
            if let Ok(png) = crate::render::sheet(&[(&format!("before{label}"), xb), (&format!("after{label}"), xf)], p.cfg.font_px) {
                sheets += 1;
                let _ = std::fs::write(dir.join(format!("step{:02}-{k:02}.png", i + 1)), &png);
                if matches!(shot, Shot::Png | Shot::Both) && out.images.len() < 4 {
                    out.images.push(png);
                }
            }
        }
        let (nb, nf): (Vec<String>, Vec<String>) = (sb.notes.iter().map(|l| note(l)).collect(), sf.notes.iter().map(|l| note(l)).collect());
        let only_b: Vec<&String> = nb.iter().filter(|l| !nf.contains(l)).collect();
        let only_f: Vec<&String> = nf.iter().filter(|l| !nb.contains(l)).collect();
        if !only_b.is_empty() || !only_f.is_empty() {
            part += "  what was noticed differs (its timing can shift a step between runs):\n";
            for l in only_b {
                part += &format!("    before only: {}\n", l.trim());
            }
            for l in only_f {
                part += &format!("     after only: {}\n", l.trim());
            }
        }
        if part.is_empty() {
            same_steps.push(i + 1);
        } else {
            body += &format!("\n── step {}/{} {}\n{part}", i + 1, steps.len(), describe(step));
        }
    }
    let mut text = format!(
        "compare\n  before: {} ({})\n   after: {} ({})\n{} steps, {screens} screens: {differ} differ. {}\n",
        before.display(),
        version_of(&before),
        after.display(),
        version_of(&after),
        steps.len(),
        if sheets > 0 { format!("Before/after images of each in {}", dir.display()) } else { String::new() }
    );
    if !a.get("raw").and_then(Value::as_bool).unwrap_or(false) {
        text += "Masked on both sides: the clock, durations, spinners, sparklines, fake-agent session ids and pids (raw: true keeps them).\n";
    }
    if same_steps.is_empty() {
        text += "No step looked the same on both.\n";
    } else {
        text += &format!("Same on both: step{} {}.\n", if same_steps.len() > 1 { "s" } else { "" }, ranges(&same_steps));
    }
    text += &body;
    out.text = text;
    Ok(out)
}

/// 1, 2, 3, 5 → "1–3, 5".
fn ranges(v: &[usize]) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let mut j = i;
        while j + 1 < v.len() && v[j + 1] == v[j] + 1 {
            j += 1;
        }
        parts.push(if i == j { v[i].to_string() } else { format!("{}–{}", v[i], v[j]) });
        i = j + 1;
    }
    parts.join(", ")
}

/// Where a run's files went, for the CLI.
pub fn out_dir(p: &Probe) -> std::path::PathBuf {
    p.cfg.out.join("compare").join(format!("{:03}", p.runs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_keep_width() {
        let m = default_masks();
        for s in ["◆ trix v1.0.3   ○ 2 idle   focus  ·  14:20", "╰──── idle 12s ╯", "⠹ claude ▁▁▃█", "session 67a66a79"] {
            assert_eq!(mask(s, &m).chars().count(), s.chars().count(), "{s}");
        }
        assert_eq!(mask("idle 2s", &m), mask("idle 7s", &m));
        assert_eq!(mask("14:20", &m), mask("09:59", &m));
        assert_ne!(mask("→ 1 agent", &m), mask("→ 0 agents", &m));
    }

    #[test]
    fn ranges_join_runs() {
        assert_eq!(ranges(&[1, 2, 3, 5, 7, 8]), "1–3, 5, 7–8");
        assert_eq!(ranges(&[4]), "4");
    }

    #[test]
    fn version_keeps_its_digits() {
        // "v1.0.3" isn't a duration or a clock: a version bump must show.
        let m = default_masks();
        assert_ne!(mask("trix v1.0.3", &m), mask("trix v1.0.4", &m));
    }
}
