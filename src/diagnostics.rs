//! Observe mouse-related control requests without recording terminal contents.
use alacritty_terminal::vte::ansi::{Handler, PrivateMode, Processor};

#[derive(Default)]
pub struct MouseModes {
    parser: Processor,
    requests: Requests,
}

#[derive(Default)]
struct Requests(Vec<(u16, bool)>);

impl Requests {
    fn record(&mut self, mode: PrivateMode, enabled: bool) {
        let mode = mode.raw();
        if matches!(mode, 1000..=1007 | 1015 | 1016 | 9001) {
            self.0.push((mode, enabled));
        }
    }
}

impl Handler for Requests {
    fn set_private_mode(&mut self, mode: PrivateMode) {
        self.record(mode, true);
    }

    fn unset_private_mode(&mut self, mode: PrivateMode) {
        self.record(mode, false);
    }
}

impl MouseModes {
    pub fn observe(&mut self, bytes: &[u8]) -> Vec<(u16, bool)> {
        self.parser.advance(&mut self.requests, bytes);
        std::mem::take(&mut self.requests.0)
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}
