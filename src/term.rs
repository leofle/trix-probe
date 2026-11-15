//! A virtual terminal: a PTY running a program (trix), read through
//! alacritty_terminal, the same VT engine trix itself uses. It answers the
//! questions a real terminal answers (device attributes, cursor position,
//! colors, text area size), records what a person would notice (bells, the
//! window title, clipboard writes, the program exiting), and turns the screen
//! into a `Snapshot` for text or PNG rendering.

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const CELL_W: u16 = 9;
pub const CELL_H: u16 = 18;

/// The outer terminal's colors: what a pane shows when trix leaves the
/// colors to the terminal, and what trix is told when it asks (OSC 10/11/4).
#[derive(Clone)]
pub struct Palette {
    pub name: &'static str,
    pub fg: Rgb,
    pub bg: Rgb,
    pub ansi: [Rgb; 16],
}

const fn rgb(hex: u32) -> Rgb {
    Rgb { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8 }
}

impl Palette {
    pub fn dark() -> Palette {
        // Tomorrow Night, a common dark terminal default.
        Palette {
            name: "dark",
            fg: rgb(0xc5c8c6),
            bg: rgb(0x1d1f21),
            ansi: [
                rgb(0x1d1f21), rgb(0xcc6666), rgb(0xb5bd68), rgb(0xf0c674), rgb(0x81a2be), rgb(0xb294bb), rgb(0x8abeb7), rgb(0xc5c8c6),
                rgb(0x666666), rgb(0xd54e53), rgb(0xb9ca4a), rgb(0xe7c547), rgb(0x7aa6da), rgb(0xc397d8), rgb(0x70c0b1), rgb(0xeaeaea),
            ],
        }
    }

    pub fn light() -> Palette {
        // Tomorrow (light), to catch UI that only reads well on dark.
        Palette {
            name: "light",
            fg: rgb(0x4d4d4c),
            bg: rgb(0xffffff),
            ansi: [
                rgb(0x000000), rgb(0xc82829), rgb(0x718c00), rgb(0xeab700), rgb(0x4271ae), rgb(0x8959a8), rgb(0x3e999f), rgb(0xd6d6d6),
                rgb(0x8e908c), rgb(0xc82829), rgb(0x718c00), rgb(0xeab700), rgb(0x4271ae), rgb(0x8959a8), rgb(0x3e999f), rgb(0xffffff),
            ],
        }
    }

    pub fn by_name(name: &str) -> Palette {
        if name.eq_ignore_ascii_case("light") { Palette::light() } else { Palette::dark() }
    }

    /// Color `i` of the 256-color table.
    pub fn indexed(&self, i: usize) -> Rgb {
        match i {
            0..=15 => self.ansi[i],
            16..=231 => {
                let i = i - 16;
                let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
                Rgb { r: level(i / 36), g: level((i / 6) % 6), b: level(i % 6) }
            }
            232..=255 => {
                let v = (8 + (i - 232) * 10) as u8;
                Rgb { r: v, g: v, b: v }
            }
            256 => self.fg,
            257 => self.bg,
            258 => self.fg, // cursor
            _ => self.fg,
        }
    }
}

/// Something a person at the terminal would notice.
#[derive(Debug, Clone)]
pub struct Notice {
    pub at: Instant,
    pub kind: &'static str,
    pub detail: String,
}

struct Shared {
    start: Instant,
    /// Milliseconds since `start` of the last screen update.
    last_output: AtomicU64,
    title: Mutex<Option<String>>,
    notices: Mutex<Vec<Notice>>,
    exited: Mutex<Option<Option<i32>>>,
    sender: Mutex<Option<EventLoopSender>>,
    palette: Palette,
    size: Mutex<(u16, u16)>,
}

impl Shared {
    fn notice(&self, kind: &'static str, detail: String) {
        self.notices.lock().unwrap().push(Notice { at: Instant::now(), kind, detail });
    }

    fn write(&self, bytes: Vec<u8>) {
        if let Some(s) = self.sender.lock().unwrap().as_ref() {
            let _ = s.send(Msg::Input(bytes.into()));
        }
    }
}

#[derive(Clone)]
struct Listener(Arc<Shared>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let s = &self.0;
        match event {
            TermEvent::Wakeup => s.last_output.store(s.start.elapsed().as_millis() as u64, Ordering::Relaxed),
            TermEvent::Title(t) => {
                let mut title = s.title.lock().unwrap();
                if title.as_deref() != Some(t.as_str()) {
                    *title = Some(t.clone());
                    drop(title);
                    s.notice("title", t);
                }
            }
            TermEvent::ResetTitle => *s.title.lock().unwrap() = None,
            TermEvent::Bell => s.notice("bell", String::new()),
            TermEvent::PtyWrite(text) => s.write(text.into_bytes()),
            TermEvent::ColorRequest(i, f) => s.write(f(s.palette.indexed(i)).into_bytes()),
            TermEvent::TextAreaSizeRequest(f) => {
                let (cols, rows) = *s.size.lock().unwrap();
                s.write(f(window_size(cols, rows)).into_bytes());
            }
            TermEvent::ClipboardStore(_, text) => s.notice("clipboard", text),
            TermEvent::ClipboardLoad(_, f) => s.write(f("").into_bytes()),
            TermEvent::ChildExit(status) => {
                *s.exited.lock().unwrap() = Some(status.code());
                s.notice("exit", status.code().map_or("killed by a signal".into(), |c| format!("code {c}")));
            }
            _ => {}
        }
    }
}

struct TermSize {
    cols: usize,
    lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

fn window_size(cols: u16, rows: u16) -> WindowSize {
    WindowSize { num_cols: cols, num_lines: rows, cell_width: CELL_W, cell_height: CELL_H }
}

pub struct Terminal {
    pub command: String,
    term: Arc<FairMutex<Term<Listener>>>,
    shared: Arc<Shared>,
    sender: EventLoopSender,
    pid: u32,
    pub keyboard: crate::keys::Keyboard,
}

impl Terminal {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        program: &Path,
        args: &[String],
        cwd: &Path,
        env: &std::collections::BTreeMap<String, String>,
        (cols, rows): (u16, u16),
        palette: Palette,
        keyboard: crate::keys::Keyboard,
    ) -> Result<Terminal> {
        let shared = Arc::new(Shared {
            start: Instant::now(),
            last_output: AtomicU64::new(0),
            title: Mutex::new(None),
            notices: Mutex::new(Vec::new()),
            exited: Mutex::new(None),
            sender: Mutex::new(None),
            palette,
            size: Mutex::new((cols, rows)),
        });
        let opts = tty::Options {
            shell: Some(tty::Shell::new(program.display().to_string(), args.to_vec())),
            working_directory: Some(cwd.to_path_buf()),
            drain_on_exit: true,
            env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<HashMap<_, _>>(),
        };
        let pty = tty::new(&opts, window_size(cols, rows), 0)?;
        let pid = pty.child().id();
        let listener = Listener(shared.clone());
        let config = Config { scrolling_history: 2000, kitty_keyboard: keyboard == crate::keys::Keyboard::Kitty, ..Config::default() };
        let term = Arc::new(FairMutex::new(Term::new(config, &TermSize { cols: cols as usize, lines: rows as usize }, listener.clone())));
        let ev = EventLoop::new(term.clone(), listener, pty, true, false)?;
        let sender = ev.channel();
        *shared.sender.lock().unwrap() = Some(sender.clone());
        ev.spawn();
        let mut command = program.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        for a in args {
            command.push(' ');
            command.push_str(a);
        }
        Ok(Terminal { command, term, shared, sender, pid, keyboard })
    }

    pub fn size(&self) -> (u16, u16) {
        *self.shared.size.lock().unwrap()
    }

    pub fn write(&self, bytes: &[u8]) {
        let _ = self.sender.send(Msg::Input(bytes.to_vec().into()));
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        *self.shared.size.lock().unwrap() = (cols, rows);
        self.term.lock().resize(TermSize { cols: cols as usize, lines: rows as usize });
        let _ = self.sender.send(Msg::Resize(window_size(cols, rows)));
    }

    pub fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    pub fn exited(&self) -> Option<Option<i32>> {
        *self.shared.exited.lock().unwrap()
    }

    pub fn title(&self) -> Option<String> {
        self.shared.title.lock().unwrap().clone()
    }

    pub fn notices_since(&self, since: Instant) -> Vec<Notice> {
        self.shared.notices.lock().unwrap().iter().filter(|n| n.at > since).cloned().collect()
    }

    /// How long the screen has been unchanged.
    pub fn quiet_for(&self) -> Duration {
        let last = self.shared.last_output.load(Ordering::Relaxed);
        self.shared.start.elapsed().saturating_sub(Duration::from_millis(last))
    }

    /// Wait until the program writes nothing for `quiet`, or `max` passes:
    /// stricter than `settle`, spinners included. `launch` uses it so its
    /// first screen shows the agents ready, not starting up.
    pub fn settle_output(&self, quiet: Duration, max: Duration) {
        let start = Instant::now();
        std::thread::sleep(Duration::from_millis(40));
        while start.elapsed() < max && self.quiet_for() < quiet {
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    /// Wait until the screen stops changing for `quiet`, or `max` passes.
    /// Settled means the program wrote nothing for `quiet`, or what it wrote
    /// left the screen the same apart from what changes by itself (the clock,
    /// durations, spinners, sparklines: see `mask`). Without the second, a
    /// working agent's spinner kept every action waiting out `max`.
    pub fn settle(&self, quiet: Duration, max: Duration) {
        let start = Instant::now();
        // Give the program a moment to react before we judge quiet.
        std::thread::sleep(Duration::from_millis(40));
        let mut last = self.fingerprint();
        let mut same_since = Instant::now();
        while start.elapsed() < max {
            if self.quiet_for() >= quiet || same_since.elapsed() >= quiet {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
            let now = self.fingerprint();
            if now != last {
                last = now;
                same_since = Instant::now();
            }
        }
    }

    /// The screen as `settle` sees it, hashed: see `mask::fingerprint`.
    pub fn fingerprint(&self) -> u64 {
        crate::mask::fingerprint(&self.snapshot())
    }

    /// Wait until the screen differs from `from` (a `fingerprint`), the clock
    /// and spinners aside, or `max` passes. Whether it changed.
    pub fn wait_change(&self, from: u64, max: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < max {
            if self.fingerprint() != from {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    pub fn signal(&self, sig: i32) {
        unsafe {
            libc::kill(self.pid as i32, sig);
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let term = self.term.lock();
        let palette = &self.shared.palette;
        let colors = term.colors();
        let resolve = |c: Color| -> Rgb {
            match c {
                Color::Spec(rgb) => rgb,
                Color::Indexed(i) => colors[i as usize].unwrap_or_else(|| palette.indexed(i as usize)),
                Color::Named(n) => {
                    let i = n as usize;
                    match n {
                        NamedColor::Foreground | NamedColor::BrightForeground => colors[i].unwrap_or(palette.fg),
                        NamedColor::Background => colors[i].unwrap_or(palette.bg),
                        NamedColor::Cursor => colors[i].unwrap_or(palette.fg),
                        NamedColor::DimForeground => colors[i].unwrap_or_else(|| blend(palette.fg, palette.bg, 0.66)),
                        _ if i < 16 => colors[i].unwrap_or(palette.ansi[i]),
                        // Dim variants of the 8 colors.
                        _ => {
                            let base = i.saturating_sub(NamedColor::DimBlack as usize) % 8;
                            blend(colors[base].unwrap_or(palette.ansi[base]), palette.bg, 0.66)
                        }
                    }
                }
            }
        };
        let rows = term.screen_lines();
        let cols = term.columns();
        let grid = term.grid();
        let mut cells = Vec::with_capacity(rows);
        for l in 0..rows {
            let row = &grid[Line(l as i32)];
            let mut out = Vec::with_capacity(cols);
            for c in 0..cols {
                let cell = &row[Column(c)];
                let mut fg = resolve(cell.fg);
                let mut bg = resolve(cell.bg);
                let f = cell.flags;
                if f.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if f.contains(Flags::DIM) {
                    fg = blend(fg, bg, 0.66);
                }
                let mut text = String::new();
                text.push(cell.c);
                if let Some(zw) = cell.zerowidth() {
                    text.extend(zw.iter());
                }
                out.push(Cell {
                    text,
                    fg,
                    bg,
                    bold: f.contains(Flags::BOLD),
                    underline: f.intersects(Flags::ALL_UNDERLINES),
                    strike: f.contains(Flags::STRIKEOUT),
                    hidden: f.contains(Flags::HIDDEN),
                    wide: f.contains(Flags::WIDE_CHAR),
                    spacer: f.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
                });
            }
            cells.push(out);
        }
        let rc = term.renderable_content();
        let cursor_visible = term.mode().contains(TermMode::SHOW_CURSOR) && rc.cursor.shape != CursorShape::Hidden;
        let cp = rc.cursor.point;
        Snapshot {
            cols,
            rows,
            cells,
            cursor: (cp.line.0.max(0) as usize, cp.column.0),
            cursor_visible,
            cursor_shape: rc.cursor.shape,
            title: self.shared.title.lock().unwrap().clone(),
            fg: palette.fg,
            bg: palette.bg,
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // A closed window: the program gets SIGHUP, as from a real terminal.
        if self.exited().is_none() {
            self.signal(libc::SIGHUP);
        }
        let _ = self.sender.send(Msg::Shutdown);
    }
}

pub fn blend(a: Rgb, b: Rgb, keep: f32) -> Rgb {
    let mix = |x: u8, y: u8| (x as f32 * keep + y as f32 * (1.0 - keep)).round() as u8;
    Rgb { r: mix(a.r, b.r), g: mix(a.g, b.g), b: mix(a.b, b.b) }
}

#[derive(Clone)]
pub struct Cell {
    pub text: String,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
    pub underline: bool,
    pub strike: bool,
    pub hidden: bool,
    pub wide: bool,
    pub spacer: bool,
}

#[derive(Clone)]
pub struct Snapshot {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<Vec<Cell>>,
    /// (row, col)
    pub cursor: (usize, usize),
    pub cursor_visible: bool,
    pub cursor_shape: CursorShape,
    pub title: Option<String>,
    pub fg: Rgb,
    pub bg: Rgb,
}

impl Snapshot {
    /// One row as text, wide characters once (their spacer cell dropped).
    pub fn row_text(&self, r: usize) -> String {
        let mut s = String::new();
        for cell in &self.cells[r] {
            if cell.spacer {
                continue;
            }
            if cell.hidden { s.push(' ') } else { s.push_str(&cell.text) }
        }
        s.trim_end().to_string()
    }

    /// A hash of one row's colors and styles, to tell when only they changed.
    pub fn row_style(&self, r: usize) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for c in &self.cells[r] {
            (c.fg.r, c.fg.g, c.fg.b, c.bg.r, c.bg.g, c.bg.b, c.bold, c.underline, c.strike).hash(&mut h);
        }
        h.finish()
    }

    /// The screen for a model to read: a column ruler, then each row with
    /// its number. Rows and columns start at 0, as `click` takes them. Runs
    /// of identical rows (empty pane borders, mostly) are folded into one line.
    pub fn to_text(&self) -> String {
        let all: Vec<usize> = (0..self.rows).collect();
        format!("{}\n{}\n{}", self.header(), self.ruler(), self.rows_text(&all))
    }

    /// Size, cursor and title, in brackets.
    pub fn header(&self) -> String {
        let cur = if self.cursor_visible {
            format!("cursor row {} col {}", self.cursor.0, self.cursor.1)
        } else {
            "cursor hidden".to_string()
        };
        format!("[{}x{} · {} · title: {}]", self.cols, self.rows, cur, self.title.as_deref().map_or("(none)".into(), |t| format!("{t:?}")))
    }

    /// Column numbers every 10, lined up with the rows.
    pub fn ruler(&self) -> String {
        let mut ruler = String::from("    ");
        let mut c = 0;
        while c < self.cols {
            let label = c.to_string();
            ruler.push_str(&label);
            let pad = 10usize.saturating_sub(label.len());
            ruler.push_str(&" ".repeat(pad.min(self.cols.saturating_sub(c + label.len()))));
            c += 10;
        }
        ruler.trim_end().to_string()
    }

    /// These rows (in order), numbered. Three or more consecutive rows with
    /// the same text show as the first and a line saying how far it repeats.
    pub fn rows_text(&self, rows: &[usize]) -> String {
        let mut out = String::new();
        let mut i = 0;
        while i < rows.len() {
            let r = rows[i];
            let text = self.row_text(r);
            let mut j = i + 1;
            while j < rows.len() && rows[j] == rows[j - 1] + 1 && self.row_text(rows[j]) == text {
                j += 1;
            }
            out.push_str(&format!("{r:>3}│{text}\n"));
            match j - i {
                1 => {}
                2 => out.push_str(&format!("{:>3}│{text}\n", rows[i + 1])),
                _ => out.push_str(&format!("  ⋮ rows {}–{} {}\n", rows[i + 1], rows[j - 1], if text.is_empty() { "are blank too".to_string() } else { format!("are the same as row {r}") })),
            }
            i = j;
        }
        out
    }

    /// Every place `needle` appears: (row, col) of its first cell.
    pub fn find(&self, needle: &str) -> Vec<(usize, usize)> {
        let mut hits = Vec::new();
        if needle.is_empty() {
            return hits;
        }
        for r in 0..self.rows {
            // Map each char of the row text to its column.
            let mut text = String::new();
            let mut col_of = Vec::new();
            for (c, cell) in self.cells[r].iter().enumerate() {
                if cell.spacer {
                    continue;
                }
                for ch in cell.text.chars() {
                    text.push(ch);
                    col_of.push(c);
                }
            }
            let chars: Vec<char> = text.chars().collect();
            let n: Vec<char> = needle.chars().collect();
            if n.len() > chars.len() {
                continue;
            }
            for i in 0..=chars.len() - n.len() {
                if chars[i..i + n.len()] == n[..] {
                    hits.push((r, col_of[i]));
                }
            }
        }
        hits
    }

    /// The full text of the screen, rows joined by newlines.
    pub fn plain(&self) -> String {
        (0..self.rows).map(|r| self.row_text(r)).collect::<Vec<_>>().join("\n")
    }
}
