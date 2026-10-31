//! Keep APC control strings intact when forwarding SSH output to a console.
//!
//! SSH packets and the OSC 52 filter can split an APC at any byte. Writing that
//! prefix to ConPTY before its terminator leaves the host inside a control
//! string while the SSH loop can make console API calls (for example resizing).
//! Buffer one bounded APC and write it as a complete unit instead.

const MAX_APC_BYTES: usize = 8192;

#[derive(Default)]
pub(crate) struct Output {
    state: u8,
    pending: Vec<u8>,
    overflow: bool,
}

impl Output {
    pub fn advance(&mut self, input: &[u8]) -> Vec<Vec<u8>> {
        let mut parts = Vec::new();
        let mut text = Vec::with_capacity(input.len());
        for &byte in input {
            match self.state {
                0 => {
                    if byte == 0x1b {
                        self.state = 1;
                    } else {
                        text.push(byte);
                    }
                }
                1 => {
                    if byte == b'_' {
                        if !text.is_empty() {
                            parts.push(std::mem::take(&mut text));
                        }
                        self.pending.clear();
                        self.pending.extend_from_slice(b"\x1b_");
                        self.overflow = false;
                        self.state = 6;
                    } else if byte == 0x1b {
                        text.push(0x1b);
                    } else {
                        text.extend_from_slice(&[0x1b, byte]);
                        self.state = match byte {
                            b']' => 2,
                            b'P' | b'^' | b'X' => 4,
                            _ => 0,
                        };
                    }
                }
                2 | 4 => {
                    text.push(byte);
                    if byte == 0x1b {
                        self.state += 1;
                    } else if matches!(byte, 0x18 | 0x1a) || (self.state == 2 && byte == 7) {
                        self.state = 0;
                    }
                }
                3 | 5 => {
                    text.push(byte);
                    if byte == b'\\' || matches!(byte, 0x18 | 0x1a) {
                        self.state = 0;
                    } else if byte != 0x1b {
                        self.state -= 1;
                    }
                }
                6 => {
                    if byte == 0x1b {
                        self.state = 7;
                    } else if matches!(byte, 0x18 | 0x1a) {
                        self.pending.clear();
                        self.state = 0;
                    } else if self.pending.len() < MAX_APC_BYTES - 2 {
                        self.pending.push(byte);
                    } else {
                        self.overflow = true;
                    }
                }
                7 => {
                    if byte == b'\\' {
                        if self.overflow {
                            tracing::warn!("Discarded oversized SSH APC sequence before ConPTY");
                            self.pending.clear();
                        } else {
                            self.pending.extend_from_slice(b"\x1b\\");
                            tracing::debug!(target: "yiterm::graphics", bytes = self.pending.len(),
                                "Forward complete SSH APC to ConPTY");
                            parts.push(std::mem::take(&mut self.pending));
                        }
                        self.state = 0;
                    } else {
                        // ESC cancels an unterminated APC. Never release the
                        // abandoned payload as ordinary console text.
                        self.pending.clear();
                        if byte == b'_' {
                            self.pending.extend_from_slice(b"\x1b_");
                            self.overflow = false;
                            self.state = 6;
                        } else if byte == 0x1b {
                            self.state = 1;
                        } else {
                            text.extend_from_slice(&[0x1b, byte]);
                            self.state = match byte {
                                b']' => 2,
                                b'P' | b'^' | b'X' => 4,
                                _ => 0,
                            };
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
        if !text.is_empty() {
            parts.push(text);
        }
        parts
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let trailing_escape = self.state == 1;
        if matches!(self.state, 6 | 7) {
            tracing::warn!("Discarded incomplete SSH APC sequence at stream close");
        }
        self.state = 0;
        self.pending.clear();
        if trailing_escape {
            vec![0x1b]
        } else {
            Vec::new()
        }
    }
}
