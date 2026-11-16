//! Layout problems a person sees at a glance, found without knowing the
//! screen: text drawn over a box's border, and a dialog drawing outside its
//! box. (Text drawn over a dialog's hint line, or over other text, looks
//! like any other text, so it isn't looked for.)

use crate::term::Snapshot;

const CORNERS: [char; 4] = ['╭', '╮', '╰', '╯'];

/// Box drawing (─ │ ╭ …) and block elements (▌ ▁ █ …): lines and
/// decoration, not text.
fn is_line(c: char) -> bool {
    ('\u{2500}'..='\u{259F}').contains(&c)
}

fn is_text(c: char) -> bool {
    !c.is_whitespace() && !is_line(c)
}

/// Each row as one char per column: a wide character's spacer cell is '\0'.
fn grid(s: &Snapshot) -> Vec<Vec<char>> {
    s.cells.iter().map(|row| row.iter().map(|c| if c.spacer { '\0' } else { c.text.chars().next().unwrap_or(' ') }).collect()).collect()
}

/// The text around column `at` of a row, out to a line or two spaces, and
/// the column it starts at.
fn words_at(row: &[(usize, char)], at: usize) -> (usize, String) {
    let gap = |i: usize| row[i].1 == ' ' && ((i > 0 && row[i - 1].1 == ' ') || (i + 1 < row.len() && row[i + 1].1 == ' '));
    let part = |i: usize| !is_line(row[i].1) && !gap(i);
    let (mut a, mut b) = (at, at);
    while a > 0 && part(a - 1) {
        a -= 1;
    }
    while b + 1 < row.len() && part(b + 1) {
        b += 1;
    }
    while a < b && row[a].1 == ' ' {
        a += 1;
    }
    let s: String = row[a..=b].iter().map(|(_, c)| *c).collect();
    (row[a].0, crate::probe::clip(s.trim(), 40))
}

/// Text drawn over a border line. trix, and the agents in it, set a label
/// in a border apart with spaces ("╭ ✦ Keys ──", "─ ▼ more ─╯", "── idle
/// 5s ╯"), so text right against a run of ─ in a row that has a box corner
/// was drawn over the border: "╰──› hello all───╯".
pub fn over_borders(s: &Snapshot) -> Vec<String> {
    let mut out = Vec::new();
    for (r, line) in grid(s).iter().enumerate() {
        let row: Vec<(usize, char)> = line.iter().copied().enumerate().filter(|(_, c)| *c != '\0').collect();
        if !row.iter().any(|(_, c)| CORNERS.contains(c)) {
            continue;
        }
        let mut seen: Vec<String> = Vec::new();
        let mut first = None;
        let mut i = 0;
        while i < row.len() {
            if row[i].1 != '─' {
                i += 1;
                continue;
            }
            let start = i;
            while i < row.len() && row[i].1 == '─' {
                i += 1;
            }
            if i - start < 2 {
                continue;
            }
            for at in [start.checked_sub(1), (i < row.len()).then_some(i)].into_iter().flatten() {
                if is_text(row[at].1) {
                    let (col, words) = words_at(&row, at);
                    if !seen.contains(&words) {
                        first = Some(first.map_or(col, |f: usize| f.min(col)));
                        seen.push(words);
                    }
                }
            }
        }
        if let Some(col) = first {
            let words: Vec<String> = seen.iter().map(|w| format!("{w:?}")).collect();
            out.push(format!("row {r} col {col}: {} drawn over a border line", words.join(", ")));
        }
    }
    out
}

/// The row a problem from this module is on.
pub fn row_of(problem: &str) -> Option<usize> {
    problem.strip_prefix("row ")?.split(' ').next()?.parse().ok()
}

/// A box drawn with rounded corners: (top row, left col, bottom row, right col).
type Rect = (usize, usize, usize, usize);

/// The boxes on screen whose four corners are all there.
pub fn boxes(s: &Snapshot) -> Vec<Rect> {
    let g = grid(s);
    let mut out = Vec::new();
    for (r1, row) in g.iter().enumerate() {
        for (c1, _) in row.iter().enumerate().filter(|(_, c)| **c == '╭') {
            let Some(c2) = (c1 + 1..row.len()).find(|&c| row[c] == '╮') else { continue };
            let Some(r2) = (r1 + 1..g.len()).find(|&r| g[r][c1] == '╰') else { continue };
            if g[r2][c2] == '╯' {
                out.push((r1, c1, r2, c2));
            }
        }
    }
    out
}

/// What a dialog drew outside its box: text that changed between `before`
/// (the screen just before it opened) and `after`, outside the biggest
/// dialog that's new in `after`. A dialog is a box with a title set apart
/// from its corner, as trix titles them ("╭ ✦ Keys ──"); a pane's box
/// ("╭─ ⚲ claude") that moved is a new layout, not a dialog. The bottom row
/// is left out: trix's bottom bar shows the dialog's keys by design. The
/// clock, timers and spinners are masked, as everywhere.
pub fn outside_dialog(before: &Snapshot, after: &Snapshot) -> Vec<String> {
    if (before.cols, before.rows) != (after.cols, after.rows) {
        return Vec::new();
    }
    let old = boxes(before);
    let (gb, ga) = (grid(before), grid(after));
    let titled = |&(r1, c1, _, c2): &Rect| c1 + 2 < c2 && ga[r1][c1 + 1] == ' ' && is_text(ga[r1][c1 + 2]);
    let Some((r1, c1, r2, c2)) = boxes(after).into_iter().filter(|b| !old.contains(b) && titled(b)).max_by_key(|&(r1, c1, r2, c2)| (r2 - r1) * (c2 - c1)) else {
        return Vec::new();
    };
    let text = |g: &[Vec<char>], r: usize, cols: std::ops::Range<usize>| -> String { g[r][cols].iter().map(|&c| if c == '\0' { ' ' } else { c }).collect() };
    let mut out = Vec::new();
    for r in 0..after.rows.saturating_sub(1) {
        // Beside the box on its rows, the whole row elsewhere.
        let parts = if (r1..=r2).contains(&r) { [0..c1, c2 + 1..after.cols] } else { [0..after.cols, 0..0] };
        for part in parts.into_iter().filter(|p| !p.is_empty()) {
            let (b, a) = (text(&gb, r, part.clone()), text(&ga, r, part.clone()));
            let (mb, ma) = (crate::mask::masked(&b), crate::mask::masked(&a));
            // A timer gaining a digit isn't a change, nor the line shortening
            // to make room for it: "──── idle 9s ╯" → "─── idle 12s ╯".
            let squeeze = |s: &str| s.split('#').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("#").replace([' ', '─'], "");
            if squeeze(&mb) == squeeze(&ma) {
                continue;
            }
            // Where they differ, by column (masks keep each line's width).
            let (bc, ac): (Vec<char>, Vec<char>) = (mb.chars().collect(), ma.chars().collect());
            let first = (0..ac.len()).find(|&i| bc.get(i) != ac.get(i)).unwrap_or(0);
            let last = (0..ac.len()).rev().find(|&i| bc.get(i) != ac.get(i)).unwrap_or(ac.len().saturating_sub(1));
            let what: String = a.chars().skip(first).take(last + 1 - first).collect();
            let what = what.trim();
            let shown = if what.is_empty() { "text was cleared".to_string() } else { format!("{:?} appeared", crate::probe::clip(what, 40)) };
            out.push(format!("row {r} col {}: {shown} outside the dialog (rows {r1}–{r2}, cols {c1}–{c2})", part.start + first));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Cell;
    use alacritty_terminal::vte::ansi::{CursorShape, Rgb};

    /// A screen of these lines, all padded to 64 columns, so that a before
    /// and an after have the same size (or `outside_dialog` looks at nothing).
    fn screen(lines: &[&str]) -> Snapshot {
        let black = Rgb { r: 0, g: 0, b: 0 };
        let cols = 64;
        assert!(lines.iter().all(|l| l.chars().count() <= cols));
        let cells = lines
            .iter()
            .map(|l| {
                let mut row: Vec<Cell> = l.chars().map(|c| Cell { text: c.to_string(), fg: black, bg: black, bold: false, underline: false, strike: false, hidden: false, wide: false, spacer: false }).collect();
                row.resize(cols, Cell { text: " ".into(), fg: black, bg: black, bold: false, underline: false, strike: false, hidden: false, wide: false, spacer: false });
                row
            })
            .collect();
        Snapshot { cols, rows: lines.len(), cells, cursor: (0, 0), cursor_visible: false, cursor_shape: CursorShape::Block, title: None, fg: black, bg: black }
    }

    #[test]
    fn labels_set_apart_are_fine() {
        let s = screen(&[
            " ╭ ✦ New agent ──────────────────╮",
            " │  › Agent     claude   codex   │",
            " ╰─────────────────────── ▼ more ─╯",
            " ╰──────────────────────── idle 5s ╯",
        ]);
        assert!(over_borders(&s).is_empty(), "{:?}", over_borders(&s));
    }

    #[test]
    fn text_over_a_border_is_found() {
        // trix 1.0.3's Broadcast at 80x7 and New agent at 80x10.
        let s = screen(&[
            "  + ╰──› hello all──────────────────────╯  │",
            "         Prompt    optional first message──────── idle 6s ╯",
        ]);
        let p = over_borders(&s);
        assert_eq!(p.len(), 2, "{p:?}");
        assert_eq!(p[0], "row 0 col 7: \"› hello all\" drawn over a border line");
        assert_eq!(p[1], "row 1 col 19: \"optional first message\" drawn over a border line");
    }

    #[test]
    fn a_dialog_drawing_below_itself_is_found() {
        let before = screen(&[
            " ◆ trix   ○ 2 idle      12:01",
            "                             ",
            "                             ",
            "                             ",
            "   ╰────────────── idle 3s ╯ ",
            "  alt+n  new                 ",
        ]);
        let after = screen(&[
            " ◆ trix   ○ 2 idle      12:02",
            " ╭ ✦ Broadcast ──────────╮   ",
            " │  To   every agent     │   ",
            " ╰───────────────────────╯   ",
            "   › hello all ─── idle 4s ╯ ",
            "  esc  cancel                ",
        ]);
        let p = outside_dialog(&before, &after);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].starts_with("row 4 col 3: \"› hello all\" appeared outside the dialog (rows 1–3"), "{p:?}");
    }

    #[test]
    fn a_timer_gaining_a_digit_is_no_change() {
        let before = screen(&["", "            ──── idle 9s ╯", "  alt+n  new"]);
        let after = screen(&[" ╭ ✦ Keys ─╮", " ╰─────────╯─── idle 12s ╯", "  esc  close"]);
        assert_eq!(boxes(&after), vec![(0, 1, 1, 11)]);
        assert!(outside_dialog(&before, &after).is_empty(), "{:?}", outside_dialog(&before, &after));
    }

    #[test]
    fn a_pane_that_moves_is_no_dialog() {
        // alt+m in trix 1.2.1 narrows the sidebar, and the pane's box with it.
        let before = screen(&["  AGENTS  2     ╭─ ⚲ shell ──╮", "   1  ○ claude  │            │", "                ╰────────────╯", "  alt+n  new                  "]);
        let after = screen(&["  AGENTS  2 ╭─ ⚲ shell ──────╮", "  1 ○ claude│                │", "            ╰────────────────╯", "  alt+n  new                  "]);
        assert_eq!(boxes(&after).len(), 1);
        assert!(outside_dialog(&before, &after).is_empty(), "{:?}", outside_dialog(&before, &after));
    }

    #[test]
    fn a_dialog_that_stays_inside_is_fine() {
        let before = screen(&[" ◆ trix   ○ 2 idle      12:01", "                             ", "  alt+n  new                 "]);
        let after = screen(&[" ╭ ✦ Keys ───╮idle      12:02", " ╰───────────╯               ", "  esc  close                 "]);
        assert_eq!(boxes(&after), vec![(0, 1, 1, 13)]);
        assert!(outside_dialog(&before, &after).is_empty());
    }
}
