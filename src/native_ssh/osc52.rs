//! Extract complete OSC 52 sequences before Windows ConPTY can consume them.

const PREFIX: &[u8] = b"\x1b]52;";
const MAX_SEQUENCE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Default)]
pub struct Filter {
    pending: Vec<u8>,
    in_osc52: bool,
    saw_escape: bool,
}

impl Filter {
    /// Returns ordinary terminal output and complete OSC 52 requests separately.
    pub fn advance(&mut self, input: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut output = Vec::with_capacity(input.len());
        let mut requests = Vec::new();

        for &byte in input {
            if self.in_osc52 {
                self.pending.push(byte);
                let complete = byte == 0x07 || byte == 0x9c || (self.saw_escape && byte == b'\\');
                self.saw_escape = byte == 0x1b;

                if complete {
                    let sequence = std::mem::take(&mut self.pending);
                    if is_clipboard_store(&sequence) {
                        requests.push(sequence);
                    } else {
                        output.extend(sequence);
                    }
                    self.in_osc52 = false;
                    self.saw_escape = false;
                } else if self.pending.len() > MAX_SEQUENCE_BYTES {
                    output.extend(std::mem::take(&mut self.pending));
                    self.in_osc52 = false;
                    self.saw_escape = false;
                }
                continue;
            }

            if self.pending.is_empty() {
                if byte == 0x1b {
                    self.pending.push(byte);
                } else {
                    output.push(byte);
                }
                continue;
            }

            self.pending.push(byte);
            if PREFIX.starts_with(&self.pending) {
                if self.pending == PREFIX {
                    self.in_osc52 = true;
                }
                continue;
            }

            // Preserve mismatched escape sequences while retaining any suffix
            // that could begin a new OSC 52 prefix.
            let keep = (1..=self.pending.len().min(PREFIX.len()))
                .rev()
                .find(|&length| PREFIX.starts_with(&self.pending[self.pending.len() - length..]))
                .unwrap_or(0);
            let split = self.pending.len() - keep;
            output.extend_from_slice(&self.pending[..split]);
            self.pending.drain(..split);
        }

        (output, requests)
    }

    /// Releases an unterminated prefix or OSC sequence when the SSH stream closes.
    pub fn finish(&mut self) -> Vec<u8> {
        self.in_osc52 = false;
        self.saw_escape = false;
        std::mem::take(&mut self.pending)
    }
}

fn is_clipboard_store(sequence: &[u8]) -> bool {
    if !sequence.starts_with(PREFIX) {
        return false;
    }
    let end = if sequence.ends_with(b"\x1b\\") {
        sequence.len() - 2
    } else if sequence.ends_with(b"\x07") || sequence.ends_with(&[0x9c]) {
        sequence.len() - 1
    } else {
        return false;
    };
    let body = &sequence[PREFIX.len()..end];
    let Some(separator) = body.iter().position(|&byte| byte == b';') else {
        return false;
    };
    let selection = &body[..separator];
    let payload = &body[separator + 1..];
    (selection.is_empty() || selection.first() == Some(&b'c')) && payload != b"?"
}
