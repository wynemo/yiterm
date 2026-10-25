//! Session-scoped prompt directory reports. No user profile files are changed.
use std::path::{Path, PathBuf};
const MAX_REPORT_BYTES: usize = 16 * 1024;

pub fn token_for(shell: &str) -> Option<String> {
    let name = Path::new(shell).file_stem()?.to_str()?;
    (cfg!(windows)
        && (name.eq_ignore_ascii_case("powershell") || name.eq_ignore_ascii_case("pwsh")))
    .then(|| uuid::Uuid::new_v4().to_string())
}

pub fn powershell_init(token: &str) -> String {
    // Tokens are generated internally. Keeping this assertion also protects the
    // inline command if another caller is added later.
    assert!(uuid::Uuid::parse_str(token).is_ok());
    include_str!("../assets/shell-integration/powershell.ps1")
        .replace("__YITERM_DIRECTORY_TOKEN__", token)
}

#[derive(Clone, Copy, Default)]
enum State {
    #[default]
    Ground,
    Escape,
    Osc,
    OscEscape,
    DiscardOsc,
    DiscardOscEscape,
    OtherString,
    OtherStringEscape,
}

/// A bounded OSC observer, run on the PTY reader thread. Plain output is skipped
/// in chunks; only OSC 7 bodies are buffered. BEL/ST and split reads are supported.
pub struct Reports {
    tag: Option<String>,
    state: State,
    body: Vec<u8>,
}
impl Reports {
    pub fn new(token: Option<String>) -> Self {
        Self {
            tag: token.map(|t| format!("yiterm-cwd={t}")),
            state: State::Ground,
            body: Vec::new(),
        }
    }
    /// None: no report; Some(None): the prompt is in a non-filesystem provider.
    pub fn observe(&mut self, bytes: &[u8]) -> Option<Option<PathBuf>> {
        self.tag.as_ref()?;
        let mut latest = None;
        let mut index = 0;
        while index < bytes.len() {
            if matches!(self.state, State::Ground) {
                let Some(offset) = bytes[index..].iter().position(|b| *b == 0x1b) else {
                    break;
                };
                index += offset + 1;
                self.state = State::Escape;
                continue;
            }
            let byte = bytes[index];
            index += 1;
            match self.state {
                State::Ground => unreachable!(),
                State::Escape => self.escape(byte),
                State::Osc => match byte {
                    0x07 => {
                        if let Some(report) = self.finish() {
                            latest = Some(report);
                        }
                    }
                    0x1b => self.state = State::OscEscape,
                    0x18 | 0x1a => {
                        self.body.clear();
                        self.state = State::Ground;
                    }
                    _ => {
                        if self.body.len() >= MAX_REPORT_BYTES
                            || self.body.is_empty() && byte != b'7'
                            || self.body.len() == 1 && byte != b';'
                        {
                            self.body.clear();
                            self.state = State::DiscardOsc;
                        } else {
                            self.body.push(byte);
                        }
                    }
                },
                State::OscEscape => {
                    if byte == b'\\' {
                        if let Some(report) = self.finish() {
                            latest = Some(report);
                        }
                    } else {
                        self.body.clear();
                        self.escape(byte);
                    }
                }
                State::DiscardOsc => match byte {
                    0x07 | 0x18 | 0x1a => self.state = State::Ground,
                    0x1b => self.state = State::DiscardOscEscape,
                    _ => {}
                },
                State::DiscardOscEscape => {
                    if byte == b'\\' {
                        self.state = State::Ground;
                    } else {
                        self.escape(byte);
                    }
                }
                State::OtherString => match byte {
                    0x1b => self.state = State::OtherStringEscape,
                    0x18 | 0x1a => self.state = State::Ground,
                    _ => {}
                },
                State::OtherStringEscape => {
                    self.state = if byte == b'\\' || byte == 0x18 || byte == 0x1a {
                        State::Ground
                    } else if byte == 0x1b {
                        State::OtherStringEscape
                    } else {
                        State::OtherString
                    };
                }
            }
        }
        latest
    }
    fn escape(&mut self, byte: u8) {
        self.state = match byte {
            b']' => {
                self.body.clear();
                State::Osc
            }
            b'P' | b'X' | b'^' | b'_' => State::OtherString,
            0x1b => State::Escape,
            _ => State::Ground,
        };
    }
    fn finish(&mut self) -> Option<Option<PathBuf>> {
        self.state = State::Ground;
        let result = self.decode();
        self.body.clear();
        result
    }
    fn decode(&self) -> Option<Option<PathBuf>> {
        let text = std::str::from_utf8(self.body.strip_prefix(b"7;")?).ok()?;
        let mut uri = url::Url::parse(text).ok()?;
        if uri.scheme() != "file"
            || !uri.username().is_empty()
            || uri.password().is_some()
            || uri.query().is_some()
        {
            return None;
        }
        let tag = self.tag.as_deref()?;
        let fragment = uri.fragment()?;
        if fragment.strip_suffix(":unavailable") == Some(tag) {
            return Some(None);
        }
        // Bare OSC 7 from an SSH child or another terminal application cannot
        // override this local PowerShell pane. The tag is not an environment var.
        if fragment != tag {
            return None;
        }
        uri.set_fragment(None);
        let path = uri.to_file_path().ok()?;
        if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
            return None;
        }
        Some(Some(path))
    }
}
