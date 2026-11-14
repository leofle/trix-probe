//! Key names to the bytes a terminal sends, and mouse events in the
//! encoding the program asked for.
//!
//! Key specs look like a person would write them: `alt+n`, `ctrl+c`,
//! `enter`, `shift+tab`, `up`, `pgdn`, `f1`, `alt+,`, `alt+?`, `alt+plus`,
//! `ctrl+alt+left`, or a single character such as `y`.

use alacritty_terminal::term::TermMode;
use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyboard {
    /// What Terminal.app and iTerm2 send by default: alt is an ESC prefix.
    Legacy,
    /// The kitty keyboard protocol (Ghostty, kitty, WezTerm), when the
    /// program turns it on. An approximation of the "disambiguate" level.
    Kitty,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Mods {
    fn param(self) -> u8 {
        1 + self.shift as u8 + 2 * self.alt as u8 + 4 * self.ctrl as u8
    }
    fn any(self) -> bool {
        self.shift || self.alt || self.ctrl
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Key {
    Char(char),
    Enter,
    Tab,
    Esc,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PgUp,
    PgDn,
    Insert,
    Delete,
    F(u8),
}

fn parse(spec: &str) -> Result<(Mods, Key)> {
    let s = spec.trim();
    if s.is_empty() {
        bail!("empty key");
    }
    // A lone character, as is ("+" and "A" included).
    if s.chars().count() == 1 {
        let c = s.chars().next().unwrap();
        return Ok((Mods { shift: c.is_ascii_uppercase(), ..Mods::default() }, Key::Char(c)));
    }
    let mut parts: Vec<&str> = s.split('+').collect();
    // "alt++" splits into ["alt", "", ""]: the key was "+".
    let key_part = if s.ends_with("++") {
        parts.truncate(parts.len() - 2);
        "+".to_string()
    } else {
        parts.pop().unwrap_or("").to_string()
    };
    let mut mods = Mods::default();
    for m in parts {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods.ctrl = true,
            "alt" | "option" | "opt" | "meta" => mods.alt = true,
            "shift" => mods.shift = true,
            "cmd" | "super" | "command" | "win" => bail!("{spec}: terminals don't pass cmd/super to programs"),
            other => bail!("{spec}: unknown modifier '{other}'"),
        }
    }
    let lower = key_part.to_ascii_lowercase();
    let key = match lower.as_str() {
        "enter" | "return" | "cr" => Key::Enter,
        "tab" => Key::Tab,
        "esc" | "escape" => Key::Esc,
        "space" | "spc" => Key::Char(' '),
        "backspace" | "bs" | "bksp" => Key::Backspace,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pgup" | "pageup" => Key::PgUp,
        "pgdn" | "pgdown" | "pagedown" => Key::PgDn,
        "insert" | "ins" => Key::Insert,
        "delete" | "del" => Key::Delete,
        "plus" => Key::Char('+'),
        "minus" => Key::Char('-'),
        "comma" => Key::Char(','),
        "period" | "dot" => Key::Char('.'),
        "slash" => Key::Char('/'),
        "question" => Key::Char('?'),
        "backslash" => Key::Char('\\'),
        f if f.len() >= 2 && f.starts_with('f') && f[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) => Key::F(f[1..].parse().unwrap()),
        _ if key_part.chars().count() == 1 => {
            let c = key_part.chars().next().unwrap();
            if c.is_ascii_uppercase() {
                mods.shift = true;
            }
            Key::Char(c)
        }
        _ => bail!("{spec}: unknown key '{key_part}'"),
    };
    Ok((mods, key))
}

/// The bytes for one key spec.
pub fn encode(spec: &str, mode: TermMode, keyboard: Keyboard) -> Result<Vec<u8>> {
    let (mods, key) = parse(spec)?;
    let kitty = keyboard == Keyboard::Kitty && mode.contains(TermMode::DISAMBIGUATE_ESC_CODES);
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let csi = |s: String| format!("\x1b[{s}").into_bytes();

    let cursor_key = |letter: char| -> Vec<u8> {
        if mods.any() {
            csi(format!("1;{}{letter}", mods.param()))
        } else if app_cursor {
            format!("\x1bO{letter}").into_bytes()
        } else {
            csi(letter.to_string())
        }
    };
    let tilde = |n: u8| -> Vec<u8> { if mods.any() { csi(format!("{n};{}~", mods.param())) } else { csi(format!("{n}~")) } };

    Ok(match key {
        Key::Up => cursor_key('A'),
        Key::Down => cursor_key('B'),
        Key::Right => cursor_key('C'),
        Key::Left => cursor_key('D'),
        Key::Home => cursor_key('H'),
        Key::End => cursor_key('F'),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PgUp => tilde(5),
        Key::PgDn => tilde(6),
        Key::F(n @ 1..=4) => {
            let letter = (b'P' + n - 1) as char;
            if mods.any() { csi(format!("1;{}{letter}", mods.param())) } else { format!("\x1bO{letter}").into_bytes() }
        }
        Key::F(n) => tilde(match n {
            5 => 15,
            6 => 17,
            7 => 18,
            8 => 19,
            9 => 20,
            10 => 21,
            11 => 23,
            _ => 24,
        }),
        Key::Enter if kitty && mods.any() => csi(format!("13;{}u", mods.param())),
        Key::Tab if kitty && mods.any() && !(mods.shift && !mods.alt && !mods.ctrl) => csi(format!("9;{}u", mods.param())),
        Key::Backspace if kitty && mods.any() => csi(format!("127;{}u", mods.param())),
        Key::Esc if kitty => if mods.any() { csi(format!("27;{}u", mods.param())) } else { csi("27u".into()) },
        Key::Enter => alt_prefix(mods.alt, b"\r"),
        Key::Tab if mods.shift => csi("Z".into()),
        Key::Tab => alt_prefix(mods.alt, b"\t"),
        Key::Esc => alt_prefix(mods.alt, b"\x1b"),
        Key::Backspace => alt_prefix(mods.alt, if mods.ctrl { b"\x08" } else { b"\x7f" }),
        Key::Char(c) => {
            if kitty && (mods.alt || mods.ctrl) {
                // Report the key without shift folded in for letters, as kitty does.
                let code = if c.is_ascii_uppercase() { c.to_ascii_lowercase() } else { c } as u32;
                return Ok(csi(format!("{code};{}u", mods.param())));
            }
            let c = if mods.shift && c.is_ascii_lowercase() { c.to_ascii_uppercase() } else { c };
            let mut bytes = if mods.ctrl { ctrl_byte(c)?.to_vec() } else { c.to_string().into_bytes() };
            if mods.alt {
                bytes.insert(0, 0x1b);
            }
            bytes
        }
    })
}

fn alt_prefix(alt: bool, b: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(b.len() + 1);
    if alt {
        v.push(0x1b);
    }
    v.extend_from_slice(b);
    v
}

fn ctrl_byte(c: char) -> Result<[u8; 1]> {
    let b = match c.to_ascii_lowercase() {
        c @ 'a'..='z' => c as u8 - b'a' + 1,
        ' ' | '@' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '/' | '7' | '-' => 0x1f,
        '?' | '8' => 0x7f,
        other => bail!("ctrl+{other} has no terminal encoding"),
    };
    Ok([b])
}

/// Text typed (or pasted) into the terminal. Newlines become Enter.
pub fn text(s: &str, paste: bool, mode: TermMode) -> Vec<u8> {
    if paste && mode.contains(TermMode::BRACKETED_PASTE) {
        let mut v = b"\x1b[200~".to_vec();
        v.extend_from_slice(s.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
        v.extend_from_slice(b"\x1b[201~");
        v
    } else {
        s.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Button {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseAct {
    Press,
    Release,
    Drag,
}

/// One mouse event at (row, col), both from 0. `None` when the program
/// hasn't asked for mouse reports (a click then does nothing, as in a real
/// terminal it would select text instead).
pub fn mouse(button: Button, act: MouseAct, row: usize, col: usize, mods: Mods, mode: TermMode) -> Option<Vec<u8>> {
    let reporting = mode.intersects(TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION);
    if !reporting {
        return None;
    }
    if act == MouseAct::Drag && !mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
        return Some(Vec::new());
    }
    let mut code: u32 = match button {
        Button::Left => 0,
        Button::Middle => 1,
        Button::Right => 2,
        Button::WheelUp => 64,
        Button::WheelDown => 65,
    };
    if act == MouseAct::Drag {
        code += 32;
    }
    if mods.shift {
        code += 4;
    }
    if mods.alt {
        code += 8;
    }
    if mods.ctrl {
        code += 16;
    }
    let (x, y) = (col + 1, row + 1);
    if mode.contains(TermMode::SGR_MOUSE) {
        let end = if act == MouseAct::Release { 'm' } else { 'M' };
        Some(format!("\x1b[<{code};{x};{y}{end}").into_bytes())
    } else {
        // X10/normal: release is button 3; coordinates offset by 32.
        let code = if act == MouseAct::Release { 3 + (code & !3) } else { code };
        if x > 223 || y > 223 {
            return Some(Vec::new());
        }
        Some(vec![0x1b, b'[', b'M', (32 + code) as u8, (32 + x) as u8, (32 + y) as u8])
    }
}

pub fn focus(focused: bool, mode: TermMode) -> Option<&'static [u8]> {
    mode.contains(TermMode::FOCUS_IN_OUT).then_some(if focused { b"\x1b[I" } else { b"\x1b[O" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy(s: &str) -> Vec<u8> {
        encode(s, TermMode::empty(), Keyboard::Legacy).unwrap()
    }

    #[test]
    fn legacy_keys() {
        assert_eq!(legacy("alt+n"), b"\x1bn");
        assert_eq!(legacy("ctrl+c"), [3]);
        assert_eq!(legacy("enter"), b"\r");
        assert_eq!(legacy("shift+tab"), b"\x1b[Z");
        assert_eq!(legacy("alt+,"), b"\x1b,");
        assert_eq!(legacy("alt+?"), b"\x1b?");
        assert_eq!(legacy("alt++"), b"\x1b+");
        assert_eq!(legacy("alt+plus"), b"\x1b+");
        assert_eq!(legacy("up"), b"\x1b[A");
        assert_eq!(legacy("ctrl+down"), b"\x1b[1;5B");
        assert_eq!(legacy("shift+pgup"), b"\x1b[5;2~");
        assert_eq!(legacy("f1"), b"\x1bOP");
        assert_eq!(legacy("f5"), b"\x1b[15~");
        assert_eq!(legacy("A"), b"A");
        assert_eq!(legacy("+"), b"+");
        assert_eq!(legacy("alt+shift+a"), b"\x1bA");
        assert!(encode("cmd+q", TermMode::empty(), Keyboard::Legacy).is_err());
    }

    #[test]
    fn app_cursor_and_kitty() {
        assert_eq!(encode("up", TermMode::APP_CURSOR, Keyboard::Legacy).unwrap(), b"\x1bOA");
        let k = TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(encode("alt+n", k, Keyboard::Kitty).unwrap(), b"\x1b[110;3u");
        assert_eq!(encode("esc", k, Keyboard::Kitty).unwrap(), b"\x1b[27u");
        assert_eq!(encode("shift+enter", k, Keyboard::Kitty).unwrap(), b"\x1b[13;2u");
        // Kitty asked for, but the program didn't turn it on: legacy.
        assert_eq!(encode("alt+n", TermMode::empty(), Keyboard::Kitty).unwrap(), b"\x1bn");
    }

    #[test]
    fn mouse_sgr() {
        let m = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(mouse(Button::Left, MouseAct::Press, 0, 4, Mods::default(), m).unwrap(), b"\x1b[<0;5;1M");
        assert_eq!(mouse(Button::Left, MouseAct::Release, 0, 4, Mods::default(), m).unwrap(), b"\x1b[<0;5;1m");
        assert_eq!(mouse(Button::WheelDown, MouseAct::Press, 9, 9, Mods::default(), m).unwrap(), b"\x1b[<65;10;10M");
        assert!(mouse(Button::Left, MouseAct::Press, 0, 0, Mods::default(), TermMode::empty()).is_none());
    }
}
