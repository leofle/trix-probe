//! What changes on screen by itself (the clock, "idle 3s", spinners,
//! sparklines, the fake agents' session ids, pids), masked so it doesn't
//! count as a change. `compare` masks it before setting two runs side by
//! side, `settle` so a spinner doesn't keep the screen from settling, and
//! the screen diffs so a ticking clock isn't a changed row. Masks keep each
//! line's width.

use crate::term::Snapshot;
use regex::Regex;
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;

pub enum Mask {
    /// Digits become '#'.
    Digits,
    /// Every character becomes '#'.
    All,
    /// Every character becomes this one.
    Char(char),
    /// Only the first character becomes this one.
    First(char),
}

pub fn default_masks() -> Vec<(Regex, Mask)> {
    let re = |s: &str| Regex::new(s).unwrap();
    vec![
        (re(r"\b\d{1,2}:\d{2}(:\d{2})?\b"), Mask::Digits),
        (re(r"\b\d+(\.\d+)?(ms|s|m|h|d)\b"), Mask::Digits),
        (re(r"(session|pid) \[?[0-9a-f]+\]?"), Mask::All),
        (re(r"[\x{2800}-\x{28FF}]"), Mask::Char('⠿')),
        (re(r"[\x{2581}-\x{2588}]"), Mask::Char('▁')),
        // Claude Code's spinner (and the fake's): "✶ Thinking… (3s · esc to interrupt)".
        (re(r"[·✢✳✶✻✽] [^ …][^…]{0,40}…"), Mask::First('✻')),
    ]
}

static DEFAULT: LazyLock<Vec<(Regex, Mask)>> = LazyLock::new(default_masks);

pub fn mask(line: &str, masks: &[(Regex, Mask)]) -> String {
    let mut s = line.to_string();
    for (re, how) in masks {
        s = re
            .replace_all(&s, |c: &regex::Captures| {
                let m = &c[0];
                match how {
                    Mask::Digits => m.chars().map(|ch| if ch.is_ascii_digit() { '#' } else { ch }).collect::<String>(),
                    Mask::All => "#".repeat(m.chars().count()),
                    Mask::Char(x) => x.to_string().repeat(m.chars().count()),
                    Mask::First(x) => std::iter::once(*x).chain(m.chars().skip(1)).collect(),
                }
            })
            .into_owned();
    }
    s
}

/// A line with the default masks applied.
pub fn masked(line: &str) -> String {
    mask(line, &DEFAULT)
}

/// Each row of the screen with the default masks applied.
pub fn rows(s: &Snapshot) -> Vec<String> {
    (0..s.rows).map(|r| mask(&s.row_text(r), &DEFAULT)).collect()
}

/// A hash of the screen as a person reads it: size, cursor, title and the
/// masked rows. Equal hashes: nothing changed but the clock and spinners.
pub fn fingerprint(s: &Snapshot) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (s.cols, s.rows, s.cursor, s.cursor_visible, &s.title).hash(&mut h);
    rows(s).hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_frames_mask_alike() {
        let a = mask("✶ Thinking… (3s · esc to interrupt)", &DEFAULT);
        let b = mask("✢ Thinking… (4s · esc to interrupt)", &DEFAULT);
        assert_eq!(a, b);
        assert!(a.starts_with('✻'));
    }

    #[test]
    fn separators_are_not_spinners() {
        assert_eq!(mask("focus  ·  16:57", &DEFAULT), "focus  ·  ##:##");
    }
}
