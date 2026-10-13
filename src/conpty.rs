//! Windows ConPTY input negotiation, independent of xterm mouse tracking.
use alacritty_terminal::vte::ansi::{Handler, PrivateMode, Processor};

#[derive(Default)]
pub struct Input {
    parser: Processor,
    mode: Mode,
}

#[derive(Default)]
struct Mode {
    win32: bool,
}

impl Handler for Mode {
    fn set_private_mode(&mut self, mode: PrivateMode) {
        if mode.raw() == 9001 {
            self.win32 = true;
        }
    }

    fn unset_private_mode(&mut self, mode: PrivateMode) {
        if mode.raw() == 9001 {
            self.win32 = false;
        }
    }

    fn reset_state(&mut self) {
        self.win32 = false;
    }
}

impl Input {
    pub fn observe(&mut self, output: &[u8]) {
        self.parser.advance(&mut self.mode, output);
    }

    pub fn mouse_report(&self, report: &[u8]) -> Vec<u8> {
        if !self.mode.win32 {
            return report.to_vec();
        }
        // Send character KEY_EVENTs, not native MOUSE_EVENTs. Windows SSH
        // must receive the VT report as text to forward it to the remote PTY.
        let mut wire = String::with_capacity(report.len() * 20);
        use std::fmt::Write;
        for byte in report {
            // CSI VirtualKey;ScanCode;UnicodeChar;KeyDown;ControlState;Repeat _
            write!(wire, "\x1b[0;0;{byte};1;0;1_").unwrap();
        }
        wire.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn characters(wire: &[u8]) -> Vec<u8> {
        std::str::from_utf8(wire)
            .unwrap()
            .split('\x1b')
            .filter(|record| !record.is_empty())
            .map(|record| {
                let fields: Vec<_> = record
                    .strip_prefix('[')
                    .unwrap()
                    .strip_suffix('_')
                    .unwrap()
                    .split(';')
                    .collect();
                assert_eq!(fields.len(), 6);
                assert_eq!(
                    (fields[0], fields[1], fields[3], fields[4], fields[5]),
                    ("0", "0", "1", "0", "1")
                );
                fields[2].parse::<u8>().unwrap()
            })
            .collect()
    }

    #[test]
    fn ssh_receives_complete_mouse_reports_as_characters() {
        let mut input = Input::default();
        input.observe(b"\x1b[?90");
        input.observe(b"01h");
        for report in [
            b"\x1b[<0;38;1M".as_slice(),
            b"\x1b[<0;38;1m",
            b"\x1b[<32;132;10M",
            b"\x1b[<64;132;10M",
        ] {
            let wire = input.mouse_report(report);
            assert_ne!(wire, report);
            assert_eq!(characters(&wire), report);
        }
    }

    #[test]
    fn reports_stay_raw_without_negotiation_and_after_reset() {
        let mut input = Input::default();
        let report = b"\x1b[<0;38;1M";
        input.observe(b"9001h\x1b]0;[?9001h\x07\x1b[?1003;1006h");
        assert_eq!(input.mouse_report(report), report);
        input.observe(b"\x1b[?9001h\x1b[?9001l");
        assert_eq!(input.mouse_report(report), report);
        input.observe(b"\x1b[?9001h\x1bc");
        assert_eq!(input.mouse_report(report), report);
    }
}
