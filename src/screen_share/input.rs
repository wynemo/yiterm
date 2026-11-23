//! Bounded terminal input decoder. UTF-8 and escape strings may span SSH reads.
use std::time::{Duration, Instant};

pub const ESCAPE: u32 = 0xe000;
pub const ENTER: u32 = 0xe001;
pub const TAB: u32 = 0xe002;
pub const BACKSPACE: u32 = 0xe003;
pub const DELETE: u32 = 0xe004;
pub const INSERT: u32 = 0xe005;
pub const HOME: u32 = 0xe006;
pub const END: u32 = 0xe007;
pub const PAGE_UP: u32 = 0xe008;
pub const PAGE_DOWN: u32 = 0xe009;
pub const UP: u32 = 0xe00a;
pub const DOWN: u32 = 0xe00b;
pub const LEFT: u32 = 0xe00c;
pub const RIGHT: u32 = 0xe00d;
pub const F1: u32 = 0xe010;

// Linux pastes one clipboard payload at a time, so keep normal multi-line
// pastes together even when the transport splits them across multiple reads.
#[cfg(target_os = "linux")]
const PASTE_CHUNK_BYTES: usize = 64 * 1024;
#[cfg(not(target_os = "linux"))]
const PASTE_CHUNK_BYTES: usize = 4096;

pub enum Event {
    Canvas(bool),
    #[cfg(windows)]
    DisplayScale(u32),
    Text(String),
    Paste(String),
    Key(u32, u32), // Shift=1, Alt/Option=2, Control=4, Win/Command=8.
    Focus(bool),
    #[cfg(target_os = "macos")]
    Size(usize, usize, usize),
    #[cfg(target_os = "macos")]
    DesktopSize(usize, usize, usize),
    Mouse(usize, usize, usize, bool),
    Exit,
}

pub fn vertical_wheel(button: usize, pressed: bool) -> Option<i32> {
    match (button & !28, pressed) {
        (64, true) => Some(1),
        (65, true) => Some(-1),
        _ => None,
    }
}

pub struct Decoder {
    bytes: Vec<u8>,
    last_input: Instant,
    paste: bool,
}
impl Decoder {
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            last_input: Instant::now(),
            paste: false,
        }
    }
    pub fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
        self.last_input = Instant::now();
    }
    pub fn next(&mut self) -> Option<Event> {
        loop {
            if self.bytes.is_empty() {
                return None;
            }
            if self.paste {
                let marker = b"\x1b[201~";
                if let Some(end) = self.bytes.windows(marker.len()).position(|s| s == marker) {
                    let text = String::from_utf8_lossy(&self.bytes[..end]).into_owned();
                    self.bytes.drain(..end + marker.len());
                    self.paste = false;
                    return Some(Event::Paste(text));
                }
                if self.bytes.len() < PASTE_CHUNK_BYTES {
                    return None;
                }
                // Keep a possible partial end marker and partial UTF-8 codepoint.
                let limit = PASTE_CHUNK_BYTES - marker.len();
                let end = std::str::from_utf8(&self.bytes[..limit])
                    .map_or_else(|e| e.valid_up_to(), |_| limit);
                if end == 0 {
                    self.bytes.remove(0);
                    continue;
                }
                let text = String::from_utf8_lossy(&self.bytes[..end]).into_owned();
                self.bytes.drain(..end);
                return Some(Event::Paste(text));
            }
            let first = self.bytes[0];
            if first != 27 {
                if first < 32 || first == 127 {
                    self.bytes.remove(0);
                    if first == 29 {
                        return Some(Event::Exit);
                    }
                    return Some(control(first, 0));
                }
                let limit = self
                    .bytes
                    .iter()
                    .position(|b| *b < 32 || *b == 127)
                    .unwrap_or(self.bytes.len());
                let end = match std::str::from_utf8(&self.bytes[..limit]) {
                    Ok(_) => limit,
                    Err(e) if e.valid_up_to() > 0 => e.valid_up_to(),
                    Err(e) if e.error_len().is_none() => return None,
                    Err(_) => {
                        self.bytes.remove(0);
                        continue;
                    }
                };
                let text = String::from_utf8_lossy(&self.bytes[..end]).into_owned();
                self.bytes.drain(..end);
                return Some(Event::Text(text));
            }
            if self.bytes.len() == 1 {
                if self.last_input.elapsed() < Duration::from_millis(80) {
                    return None;
                }
                self.bytes.clear();
                return Some(Event::Key(ESCAPE, 0));
            }
            if matches!(self.bytes[1], b'[' | b'O') {
                let end = self
                    .bytes
                    .iter()
                    .enumerate()
                    .skip(2)
                    .find_map(|(i, b)| (0x40..=0x7e).contains(b).then_some(i));
                let Some(end) = end else {
                    if self.bytes.len() > 128
                        || self.last_input.elapsed() > Duration::from_millis(250)
                    {
                        self.bytes.remove(0);
                        return Some(Event::Key(ESCAPE, 0));
                    }
                    return None;
                };
                let sequence: Vec<_> = self.bytes.drain(..=end).collect();
                let final_byte = sequence[end];
                let text = std::str::from_utf8(&sequence[2..end]).unwrap_or("");
                if text.starts_with('<') && matches!(final_byte, b'M' | b'm') {
                    if let Some([button, x, y]) = numbers::<3>(&text[1..]) {
                        return Some(Event::Mouse(button, x, y, final_byte == b'M'));
                    }
                    continue;
                }
                if final_byte == b'y' && text.starts_with("?9002;") {
                    return Some(Event::Canvas(matches!(text, "?9002;1$" | "?9002;2$")));
                }
                #[cfg(target_os = "macos")]
                if final_byte == b't' && text.starts_with("?9004;") {
                    if let Some([w, h, scale]) = numbers::<3>(&text[6..]) {
                        if (320..=1920).contains(&w) && (320..=1920).contains(&h)
                            && (100..=400).contains(&scale) {
                            return Some(Event::DesktopSize(w, h, scale));
                        }
                    }
                    continue;
                }
                // YiTerm display metadata travels inside the existing bounded input
                // channel. Older peers discard this private CSI without typing it.
                if final_byte == b't' && text.starts_with("?9003;") {
                    #[cfg(windows)]
                    if let Ok(scale) = text[6..].parse::<u32>() {
                        if (100..=500).contains(&scale) {
                            return Some(Event::DisplayScale(scale));
                        }
                    }
                    continue;
                }
                #[cfg(target_os = "macos")]
                if final_byte == b't' {
                    if let Some([kind, h, w]) = numbers::<3>(text) {
                        return Some(Event::Size(kind, w, h));
                    }
                    continue;
                }
                if text.is_empty() && final_byte == b'I' {
                    return Some(Event::Focus(true));
                }
                if text.is_empty() && final_byte == b'O' && sequence[1] == b'[' {
                    return Some(Event::Focus(false));
                }
                if text == "200" && final_byte == b'~' {
                    self.paste = true;
                    continue;
                }
                if text == "201" && final_byte == b'~' {
                    continue;
                }
                if final_byte == b'u' && text.starts_with("?9005;") {
                    if let Some([code, modifiers]) = numbers::<2>(&text[6..]) {
                        if (32..=126).contains(&code) && (8..=15).contains(&modifiers) {
                            return Some(Event::Key(code as u32, modifiers as u32));
                        }
                    }
                    continue;
                }
                if let Some(key) = special(text, final_byte) {
                    return Some(key);
                }
                continue; // Unknown protocol replies never become text/keystrokes.
            }
            let second = self.bytes[1];
            if second < 32 || second == 127 {
                self.bytes.drain(..2);
                return Some(control(second, 2));
            }
            let text = match std::str::from_utf8(&self.bytes[1..]) {
                Ok(s) => s,
                Err(e) if e.valid_up_to() > 0 => {
                    std::str::from_utf8(&self.bytes[1..1 + e.valid_up_to()]).ok()?
                }
                Err(e) if e.error_len().is_none() => return None,
                Err(_) => {
                    self.bytes.drain(..2);
                    continue;
                }
            };
            let c = text.chars().next()?;
            self.bytes.drain(..1 + c.len_utf8());
            return Some(Event::Key(c as u32, 2));
        }
    }
}
fn numbers<const N: usize>(text: &str) -> Option<[usize; N]> {
    let values: Vec<_> = text
        .split(';')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    values.try_into().ok()
}
fn control(byte: u8, modifiers: u32) -> Event {
    if cfg!(target_os = "macos") && modifiers & 2 != 0 && (1..=26).contains(&byte) {
        return Event::Key(u32::from(b'a' + byte - 1), 8);
    }
    let key = match byte {
        b'\r' | b'\n' => ENTER,
        b'\t' => TAB,
        8 | 127 => BACKSPACE,
        27 => ESCAPE,
        _ => {
            let code = match byte {
                0 => 32,
                1..=26 => u32::from(b'a' + byte - 1),
                28 => 92,
                30 => 94,
                31 => 95,
                _ => return Event::Key(ESCAPE, modifiers),
            };
            // Ctrl+Alt supplies Command shortcuts from Windows/Linux keyboards.
            return Event::Key(
                code,
                if cfg!(target_os = "macos") && modifiers & 2 != 0 {
                    8
                } else {
                    modifiers | 4
                },
            );
        }
    };
    Event::Key(key, modifiers)
}
fn special(text: &str, final_byte: u8) -> Option<Event> {
    let values: Vec<usize> = if text.is_empty() {
        Vec::new()
    } else {
        text.split(';')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?
    };
    let modifiers = values.get(1).copied().unwrap_or(1).checked_sub(1)?;
    if modifiers > 15 {
        return None;
    }
    let modifiers = if cfg!(target_os = "macos") && modifiers & 6 == 6 {
        (modifiers & !6) | 8
    } else {
        modifiers
    } as u32;
    let key = match final_byte {
        b'A' => UP,
        b'B' => DOWN,
        b'C' => RIGHT,
        b'D' => LEFT,
        b'H' => HOME,
        b'F' => END,
        b'P' => F1,
        b'Q' => F1 + 1,
        b'R' => F1 + 2,
        b'S' => F1 + 3,
        b'Z' => return Some(Event::Key(TAB, 1)),
        b'~' => match *values.first()? {
            1 | 7 => HOME,
            2 => INSERT,
            3 => DELETE,
            4 | 8 => END,
            5 => PAGE_UP,
            6 => PAGE_DOWN,
            11..=15 => F1 + (*values.first()? as u32 - 11),
            17..=21 => F1 + 5 + (*values.first()? as u32 - 17),
            23..=24 => F1 + 10 + (*values.first()? as u32 - 23),
            _ => return None,
        },
        _ => return None,
    };
    Some(Event::Key(key, modifiers))
}
