//! A `Snapshot` as a PNG, in the terminal's real colors, so a model can
//! judge contrast, alignment and theming, not just the text.
//!
//! Glyphs come from a monospace system font (Menlo on macOS, DejaVu Sans Mono
//! or Noto on Linux; `TRIX_PROBE_FONT` overrides), with symbol fonts as
//! fallbacks. Box drawing, block elements, braille and the Powerline/Nerd Font
//! caps are drawn by hand so trix's borders, sparklines and spinners come out
//! crisp at any size. A glyph no font has is drawn as an outlined box; color
//! emoji are among those (system emoji fonts are bitmaps).

use crate::term::Snapshot;
use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use alacritty_terminal::vte::ansi::{CursorShape, Rgb};
use anyhow::{Context, Result};
use std::sync::OnceLock;

struct Fonts {
    list: Vec<FontVec>,
}

const CANDIDATES: &[(&str, u32)] = &[
    // macOS
    ("/System/Library/Fonts/Menlo.ttc", 0),
    ("/System/Library/Fonts/SFNSMono.ttf", 0),
    ("/System/Library/Fonts/Monaco.ttf", 0),
    // Linux
    ("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", 0),
    ("/usr/share/fonts/TTF/DejaVuSansMono.ttf", 0),
    ("/usr/share/fonts/dejavu-sans-mono-fonts/DejaVuSansMono.ttf", 0),
    ("/usr/share/fonts/noto/NotoSansMono-Regular.ttf", 0),
    ("/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf", 0),
    ("/usr/share/fonts/liberation/LiberationMono-Regular.ttf", 0),
    // Fallbacks for symbols the mono font lacks.
    ("/System/Library/Fonts/Apple Symbols.ttf", 0),
    ("/System/Library/Fonts/Supplemental/Arial Unicode.ttf", 0),
    ("/Library/Fonts/Arial Unicode.ttf", 0),
    ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 0),
    ("/usr/share/fonts/TTF/DejaVuSans.ttf", 0),
    ("/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf", 0),
    ("/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf", 0),
    ("/usr/share/fonts/noto/NotoSansSymbols-Regular.ttf", 0),
];

fn fonts() -> &'static Result<Fonts, String> {
    static FONTS: OnceLock<Result<Fonts, String>> = OnceLock::new();
    FONTS.get_or_init(|| {
        let mut list = Vec::new();
        let mut paths: Vec<(String, u32)> = Vec::new();
        if let Ok(p) = std::env::var("TRIX_PROBE_FONT") {
            for one in p.split(':').filter(|s| !s.is_empty()) {
                paths.push((one.to_string(), 0));
            }
        }
        paths.extend(CANDIDATES.iter().map(|(p, i)| (p.to_string(), *i)));
        for (p, idx) in paths {
            if let Ok(data) = std::fs::read(&p)
                && let Ok(f) = FontVec::try_from_vec_and_index(data, idx)
            {
                list.push(f);
            }
        }
        if list.is_empty() {
            Err("no usable font found; set TRIX_PROBE_FONT to a .ttf/.ttc monospace font".into())
        } else {
            Ok(Fonts { list })
        }
    })
}

pub struct Metrics {
    pub cell_w: u32,
    pub cell_h: u32,
    ascent: f32,
    px: f32,
}

fn metrics(px: f32) -> Result<Metrics> {
    let fonts = fonts().as_ref().map_err(|e| anyhow::anyhow!(e.clone()))?;
    let f = fonts.list[0].as_scaled(PxScale::from(px));
    let adv = f.h_advance(f.glyph_id('M'));
    let line = f.ascent() - f.descent() + f.line_gap();
    Ok(Metrics { cell_w: adv.round().max(1.0) as u32, cell_h: line.ceil().max(1.0) as u32, ascent: f.ascent(), px })
}

struct Canvas {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

impl Canvas {
    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, c: Rgb) {
        let (x0, y0) = (x.max(0), y.max(0));
        let (x1, y1) = ((x + w).min(self.w as i32), (y + h).min(self.h as i32));
        for yy in y0..y1 {
            for xx in x0..x1 {
                let i = ((yy as u32 * self.w + xx as u32) * 3) as usize;
                self.px[i] = c.r;
                self.px[i + 1] = c.g;
                self.px[i + 2] = c.b;
            }
        }
    }

    fn blend(&mut self, x: i32, y: i32, c: Rgb, a: f32) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 || a <= 0.0 {
            return;
        }
        let a = a.min(1.0);
        let i = ((y as u32 * self.w + x as u32) * 3) as usize;
        let mix = |old: u8, new: u8| (old as f32 * (1.0 - a) + new as f32 * a).round() as u8;
        self.px[i] = mix(self.px[i], c.r);
        self.px[i + 1] = mix(self.px[i + 1], c.g);
        self.px[i + 2] = mix(self.px[i + 2], c.b);
    }
}

/// Render the snapshot to PNG bytes. `px` is the font size (16 is a good
/// default: a 140x40 screen comes out around 1350x760).
pub fn png(snap: &Snapshot, px: f32) -> Result<Vec<u8>> {
    let m = metrics(px)?;
    let fonts = fonts().as_ref().map_err(|e| anyhow::anyhow!(e.clone()))?;
    let (w, h) = (snap.cols as u32 * m.cell_w, snap.rows as u32 * m.cell_h);
    let mut cv = Canvas { w, h, px: vec![0; (w * h * 3) as usize] };
    cv.fill(0, 0, w as i32, h as i32, snap.bg);

    for (r, row) in snap.cells.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            let x = (c as u32 * m.cell_w) as i32;
            let y = (r as u32 * m.cell_h) as i32;
            let cw = if cell.wide { 2 * m.cell_w } else { m.cell_w } as i32;
            if !cell.spacer {
                cv.fill(x, y, cw, m.cell_h as i32, cell.bg);
            }
        }
    }
    for (r, row) in snap.cells.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            if cell.spacer || cell.hidden {
                continue;
            }
            let x = (c as u32 * m.cell_w) as i32;
            let y = (r as u32 * m.cell_h) as i32;
            let cw = if cell.wide { 2 * m.cell_w } else { m.cell_w } as i32;
            let ch = cell.text.chars().next().unwrap_or(' ');
            if ch != ' ' && !special(&mut cv, ch, x, y, cw, m.cell_h as i32, cell.fg) {
                glyph(&mut cv, fonts, &m, ch, x, y, cw, cell.fg, cell.bold);
            }
            if cell.underline {
                cv.fill(x, y + m.cell_h as i32 - 2, cw, 1, cell.fg);
            }
            if cell.strike {
                cv.fill(x, y + m.cell_h as i32 / 2, cw, 1, cell.fg);
            }
        }
    }
    if snap.cursor_visible && snap.cursor.0 < snap.rows && snap.cursor.1 < snap.cols {
        let (r, c) = snap.cursor;
        let x = (c as u32 * m.cell_w) as i32;
        let y = (r as u32 * m.cell_h) as i32;
        let (cw, chh) = (m.cell_w as i32, m.cell_h as i32);
        let color = snap.fg;
        match snap.cursor_shape {
            CursorShape::Beam => cv.fill(x, y, 2, chh, color),
            CursorShape::Underline => cv.fill(x, y + chh - 2, cw, 2, color),
            _ => {
                // An outline, so the character under it stays readable.
                cv.fill(x, y, cw, 1, color);
                cv.fill(x, y + chh - 1, cw, 1, color);
                cv.fill(x, y, 1, chh, color);
                cv.fill(x + cw - 1, y, 1, chh, color);
            }
        }
    }
    encode_png(&cv)
}

#[allow(clippy::too_many_arguments)]
fn glyph(cv: &mut Canvas, fonts: &Fonts, m: &Metrics, ch: char, x: i32, y: i32, cw: i32, fg: Rgb, bold: bool) {
    let found = fonts.list.iter().find(|f| f.glyph_id(ch).0 != 0);
    let Some(font) = found else {
        // Nothing has it: an outlined box, like a terminal's tofu.
        let (bx, by, bw, bh) = (x + 1, y + 2, cw - 2, m.cell_h as i32 - 4);
        cv.fill(bx, by, bw, 1, fg);
        cv.fill(bx, by + bh - 1, bw, 1, fg);
        cv.fill(bx, by, 1, bh, fg);
        cv.fill(bx + bw - 1, by, 1, bh, fg);
        return;
    };
    let scale = PxScale::from(m.px);
    let sf = font.as_scaled(scale);
    let id = font.glyph_id(ch);
    // Fallback fonts can be wider than a cell: shrink to fit.
    let adv = sf.h_advance(id);
    let scale = if adv > cw as f32 + 0.5 && adv > 0.0 { PxScale::from(m.px * (cw as f32 / adv)) } else { scale };
    let g = id.with_scale_and_position(scale, ab_glyph::point(x as f32, y as f32 + m.ascent));
    if let Some(outline) = font.outline_glyph(g) {
        let b = outline.px_bounds();
        let passes: &[i32] = if bold { &[0, 1] } else { &[0] };
        for dx in passes {
            outline.draw(|gx, gy, cov| {
                cv.blend(b.min.x as i32 + gx as i32 + dx, b.min.y as i32 + gy as i32, fg, cov);
            });
        }
    }
}

/// Characters drawn by hand. Returns false for anything else.
fn special(cv: &mut Canvas, ch: char, x: i32, y: i32, w: i32, h: i32, fg: Rgb) -> bool {
    let cp = ch as u32;
    match cp {
        0x2500..=0x257f => box_drawing(cv, cp, x, y, w, h, fg),
        0x2580..=0x259f => {
            block(cv, cp, x, y, w, h, fg);
            true
        }
        0x2800..=0x28ff => {
            braille(cv, cp - 0x2800, x, y, w, h, fg);
            true
        }
        0xe0b0 | 0xe0b2 | 0xe0b4 | 0xe0b6 => {
            powerline(cv, cp, x, y, w, h, fg);
            true
        }
        _ => false,
    }
}

/// (up, right, down, left): 0 none, 1 light, 2 heavy, 3 double.
fn box_arms(cp: u32) -> Option<[u8; 4]> {
    Some(match cp {
        0x2500 | 0x2504 | 0x2508 | 0x254c => [0, 1, 0, 1],
        0x2501 | 0x2505 | 0x2509 | 0x254d => [0, 2, 0, 2],
        0x2502 | 0x2506 | 0x250a | 0x254e => [1, 0, 1, 0],
        0x2503 | 0x2507 | 0x250b | 0x254f => [2, 0, 2, 0],
        0x250c => [0, 1, 1, 0],
        0x250d => [0, 2, 1, 0],
        0x250e => [0, 1, 2, 0],
        0x250f => [0, 2, 2, 0],
        0x2510 => [0, 0, 1, 1],
        0x2511 => [0, 0, 1, 2],
        0x2512 => [0, 0, 2, 1],
        0x2513 => [0, 0, 2, 2],
        0x2514 => [1, 1, 0, 0],
        0x2515 => [1, 2, 0, 0],
        0x2516 => [2, 1, 0, 0],
        0x2517 => [2, 2, 0, 0],
        0x2518 => [1, 0, 0, 1],
        0x2519 => [1, 0, 0, 2],
        0x251a => [2, 0, 0, 1],
        0x251b => [2, 0, 0, 2],
        0x251c => [1, 1, 1, 0],
        0x251d => [1, 2, 1, 0],
        0x251e => [2, 1, 1, 0],
        0x251f => [1, 1, 2, 0],
        0x2520 => [2, 1, 2, 0],
        0x2521 => [2, 2, 1, 0],
        0x2522 => [1, 2, 2, 0],
        0x2523 => [2, 2, 2, 0],
        0x2524 => [1, 0, 1, 1],
        0x2525 => [1, 0, 1, 2],
        0x2526 => [2, 0, 1, 1],
        0x2527 => [1, 0, 2, 1],
        0x2528 => [2, 0, 2, 1],
        0x2529 => [2, 0, 1, 2],
        0x252a => [1, 0, 2, 2],
        0x252b => [2, 0, 2, 2],
        0x252c => [0, 1, 1, 1],
        0x252d => [0, 1, 1, 2],
        0x252e => [0, 2, 1, 1],
        0x252f => [0, 2, 1, 2],
        0x2530 => [0, 1, 2, 1],
        0x2531 => [0, 1, 2, 2],
        0x2532 => [0, 2, 2, 1],
        0x2533 => [0, 2, 2, 2],
        0x2534 => [1, 1, 0, 1],
        0x2535 => [1, 1, 0, 2],
        0x2536 => [1, 2, 0, 1],
        0x2537 => [1, 2, 0, 2],
        0x2538 => [2, 1, 0, 1],
        0x2539 => [2, 1, 0, 2],
        0x253a => [2, 2, 0, 1],
        0x253b => [2, 2, 0, 2],
        0x253c => [1, 1, 1, 1],
        0x253d => [1, 1, 1, 2],
        0x253e => [1, 2, 1, 1],
        0x253f => [1, 2, 1, 2],
        0x2540 => [2, 1, 1, 1],
        0x2541 => [1, 1, 2, 1],
        0x2542 => [2, 1, 2, 1],
        0x2543 => [2, 1, 1, 2],
        0x2544 => [2, 2, 1, 1],
        0x2545 => [1, 1, 2, 2],
        0x2546 => [1, 2, 2, 1],
        0x2547 => [2, 2, 1, 2],
        0x2548 => [1, 2, 2, 2],
        0x2549 => [2, 1, 2, 2],
        0x254a => [2, 2, 2, 1],
        0x254b => [2, 2, 2, 2],
        0x2550 => [0, 3, 0, 3],
        0x2551 => [3, 0, 3, 0],
        0x2552 => [0, 3, 1, 0],
        0x2553 => [0, 1, 3, 0],
        0x2554 => [0, 3, 3, 0],
        0x2555 => [0, 0, 1, 3],
        0x2556 => [0, 0, 3, 1],
        0x2557 => [0, 0, 3, 3],
        0x2558 => [1, 3, 0, 0],
        0x2559 => [3, 1, 0, 0],
        0x255a => [3, 3, 0, 0],
        0x255b => [1, 0, 0, 3],
        0x255c => [3, 0, 0, 1],
        0x255d => [3, 0, 0, 3],
        0x255e => [1, 3, 1, 0],
        0x255f => [3, 1, 3, 0],
        0x2560 => [3, 3, 3, 0],
        0x2561 => [1, 0, 1, 3],
        0x2562 => [3, 0, 3, 1],
        0x2563 => [3, 0, 3, 3],
        0x2564 => [0, 3, 1, 3],
        0x2565 => [0, 1, 3, 1],
        0x2566 => [0, 3, 3, 3],
        0x2567 => [1, 3, 0, 3],
        0x2568 => [3, 1, 0, 1],
        0x2569 => [3, 3, 0, 3],
        0x256a => [1, 3, 1, 3],
        0x256b => [3, 1, 3, 1],
        0x256c => [3, 3, 3, 3],
        0x2574 => [0, 0, 0, 1],
        0x2575 => [1, 0, 0, 0],
        0x2576 => [0, 1, 0, 0],
        0x2577 => [0, 0, 1, 0],
        0x2578 => [0, 0, 0, 2],
        0x2579 => [2, 0, 0, 0],
        0x257a => [0, 2, 0, 0],
        0x257b => [0, 0, 2, 0],
        0x257c => [0, 2, 0, 1],
        0x257d => [1, 0, 2, 0],
        0x257e => [0, 1, 0, 2],
        0x257f => [2, 0, 1, 0],
        _ => return None,
    })
}

fn box_drawing(cv: &mut Canvas, cp: u32, x: i32, y: i32, w: i32, h: i32, fg: Rgb) -> bool {
    let light = ((w as f32) / 9.0).round().max(1.0) as i32;
    let heavy = light * 2;
    let (cx, cy) = (x + w / 2, y + h / 2);
    // Rounded corners: a quarter circle and two straight runs.
    if let 0x256d..=0x2570 = cp {
        let lo = light / 2;
        // Centre lines of the straight strokes, as the other box characters draw them.
        let (lx, ly) = ((cx - lo) as f32 + light as f32 / 2.0, (cy - lo) as f32 + light as f32 / 2.0);
        let r = (w.min(h) / 2) as f32;
        // Arc centre, and which ways the arms run to the cell's edges.
        let (right, down) = match cp {
            0x256d => (true, true),   // ╭
            0x256e => (false, true),  // ╮
            0x256f => (false, false), // ╯
            _ => (true, false),       // ╰
        };
        let acx = if right { lx + r } else { lx - r };
        let acy = if down { ly + r } else { ly - r };
        let t = light as f32;
        for yy in y..y + h {
            for xx in x..x + w {
                let (px, py) = (xx as f32 + 0.5, yy as f32 + 0.5);
                let in_quadrant = (if right { px <= acx } else { px >= acx }) && (if down { py <= acy } else { py >= acy });
                if !in_quadrant {
                    continue;
                }
                let d = ((px - acx).powi(2) + (py - acy).powi(2)).sqrt();
                let cov = (t / 2.0 + 0.5 - (d - r).abs()).clamp(0.0, 1.0);
                cv.blend(xx, yy, fg, cov);
            }
        }
        if right {
            cv.fill(acx.floor() as i32, cy - lo, x + w - acx.floor() as i32, light, fg);
        } else {
            cv.fill(x, cy - lo, acx.ceil() as i32 - x, light, fg);
        }
        if down {
            cv.fill(cx - lo, acy.floor() as i32, light, y + h - acy.floor() as i32, fg);
        } else {
            cv.fill(cx - lo, y, light, acy.ceil() as i32 - y, fg);
        }
        return true;
    }
    let Some([up, right, down, left]) = box_arms(cp) else { return false };
    let thick = |k: u8| if k == 2 { heavy } else { light };
    // The widest crossing arm, so arms meet without gaps.
    let hw = [up, down].iter().map(|&k| if k == 0 { 0 } else { thick(k) }).max().unwrap_or(0);
    let vw = [left, right].iter().map(|&k| if k == 0 { 0 } else { thick(k) }).max().unwrap_or(0);
    let gap = light.max(1) + 1; // double lines sit this far from the centre
    let arm_v = |cv: &mut Canvas, k: u8, y0: i32, y1: i32| match k {
        0 => {}
        3 => {
            cv.fill(cx - gap, y0, light, y1 - y0, fg);
            cv.fill(cx + gap - light + 1, y0, light, y1 - y0, fg);
        }
        k => {
            let t = thick(k);
            cv.fill(cx - t / 2, y0, t, y1 - y0, fg);
        }
    };
    let arm_h = |cv: &mut Canvas, k: u8, x0: i32, x1: i32| match k {
        0 => {}
        3 => {
            cv.fill(x0, cy - gap, x1 - x0, light, fg);
            cv.fill(x0, cy + gap - light + 1, x1 - x0, light, fg);
        }
        k => {
            let t = thick(k);
            cv.fill(x0, cy - t / 2, x1 - x0, t, fg);
        }
    };
    let ext_v = vw.max(gap) / 2 + 1;
    let ext_h = hw.max(gap) / 2 + 1;
    arm_v(cv, up, y, cy + ext_v);
    arm_v(cv, down, cy - ext_v + 1, y + h);
    arm_h(cv, left, x, cx + ext_h);
    arm_h(cv, right, cx - ext_h + 1, x + w);
    true
}

fn block(cv: &mut Canvas, cp: u32, x: i32, y: i32, w: i32, h: i32, fg: Rgb) {
    let eighth_h = |n: i32| (h * n + 4) / 8;
    let eighth_w = |n: i32| (w * n + 4) / 8;
    match cp {
        0x2580 => cv.fill(x, y, w, h / 2, fg),
        0x2581..=0x2587 => {
            let n = (cp - 0x2580) as i32;
            let bh = eighth_h(n);
            cv.fill(x, y + h - bh, w, bh, fg);
        }
        0x2588 => cv.fill(x, y, w, h, fg),
        0x2589..=0x258f => {
            let n = 8 - (cp - 0x2588) as i32;
            cv.fill(x, y, eighth_w(n), h, fg);
        }
        0x2590 => cv.fill(x + w / 2, y, w - w / 2, h, fg),
        0x2591..=0x2593 => {
            let a = (cp - 0x2590) as f32 * 0.25;
            for yy in y..y + h {
                for xx in x..x + w {
                    cv.blend(xx, yy, fg, a);
                }
            }
        }
        0x2594 => cv.fill(x, y, w, eighth_h(1), fg),
        0x2595 => {
            let bw = eighth_w(1);
            cv.fill(x + w - bw, y, bw, h, fg);
        }
        _ => {
            // Quadrants: upper-left, upper-right, lower-left, lower-right.
            let q = match cp {
                0x2596 => [0, 0, 1, 0],
                0x2597 => [0, 0, 0, 1],
                0x2598 => [1, 0, 0, 0],
                0x2599 => [1, 0, 1, 1],
                0x259a => [1, 0, 0, 1],
                0x259b => [1, 1, 1, 0],
                0x259c => [1, 1, 0, 1],
                0x259d => [0, 1, 0, 0],
                0x259e => [0, 1, 1, 0],
                _ => [0, 1, 1, 1],
            };
            let (hw, hh) = (w / 2, h / 2);
            if q[0] == 1 {
                cv.fill(x, y, hw, hh, fg);
            }
            if q[1] == 1 {
                cv.fill(x + hw, y, w - hw, hh, fg);
            }
            if q[2] == 1 {
                cv.fill(x, y + hh, hw, h - hh, fg);
            }
            if q[3] == 1 {
                cv.fill(x + hw, y + hh, w - hw, h - hh, fg);
            }
        }
    }
}

fn braille(cv: &mut Canvas, bits: u32, x: i32, y: i32, w: i32, h: i32, fg: Rgb) {
    // Dot n -> (column, row): 1,2,3,7 down the left; 4,5,6,8 down the right.
    const DOTS: [(i32, i32); 8] = [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3)];
    let d = (w / 4).max(1);
    for (i, (dc, dr)) in DOTS.iter().enumerate() {
        if bits & (1 << i) != 0 {
            let px = x + (w * (2 * dc + 1)) / 4 - d / 2;
            let py = y + (h * (2 * dr + 1)) / 8 - d / 2;
            cv.fill(px, py, d, d, fg);
        }
    }
}

fn powerline(cv: &mut Canvas, cp: u32, x: i32, y: i32, w: i32, h: i32, fg: Rgb) {
    let hh = h as f32 / 2.0;
    for yy in y..y + h {
        let fy = yy as f32 + 0.5 - y as f32;
        let t = 1.0 - ((fy - hh).abs() / hh); // 1 at the middle row, 0 at the edges
        for xx in x..x + w {
            let fx = xx as f32 + 0.5 - x as f32;
            let inside = match cp {
                0xe0b0 => fx <= t * w as f32,           // ▶
                0xe0b2 => fx >= (1.0 - t) * w as f32,   // ◀
                0xe0b4 => {
                    // right half circle: an ellipse centred on the left edge
                    let (dx, dy) = (fx / w as f32, (fy - hh) / hh);
                    dx * dx + dy * dy <= 1.0
                }
                _ => {
                    let (dx, dy) = ((w as f32 - fx) / w as f32, (fy - hh) / hh);
                    dx * dx + dy * dy <= 1.0
                }
            };
            if inside {
                cv.blend(xx, yy, fg, 1.0);
            }
        }
    }
}

fn encode_png(cv: &Canvas) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, cv.w, cv.h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().context("png header")?;
        writer.write_image_data(&cv.px).context("png data")?;
    }
    Ok(out)
}
