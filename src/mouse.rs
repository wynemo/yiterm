//! Mouse protocol state, including URXVT requests preserved by Windows ConPTY.
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Handler, PrivateMode, Processor};

#[derive(Default)]
pub struct MouseModes {
    parser: Processor,
    requests: Requests,
}

#[derive(Default)]
struct Requests {
    changes: Vec<(u16, bool)>,
    urxvt: bool,
}

impl Requests {
    fn record(&mut self, mode: PrivateMode, enabled: bool) {
        let mode = mode.raw();
        if mode == 1015 {
            self.urxvt = enabled;
        }
        if matches!(mode, 1000..=1007 | 1015 | 1016 | 9001) {
            self.changes.push((mode, enabled));
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

    fn reset_state(&mut self) {
        self.urxvt = false;
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Protocol {
    pub mode: TermMode,
    pub urxvt: bool,
}

impl From<TermMode> for Protocol {
    fn from(mode: TermMode) -> Self {
        Self { mode, urxvt: false }
    }
}

impl MouseModes {
    pub fn observe(&mut self, bytes: &[u8]) -> Vec<(u16, bool)> {
        self.parser.advance(&mut self.requests, bytes);
        std::mem::take(&mut self.requests.changes)
    }

    pub fn protocol(&self, mode: TermMode) -> Protocol {
        Protocol {
            mode,
            urxvt: self.requests.urxvt && !mode.contains(TermMode::SGR_MOUSE),
        }
    }
}

#[derive(Default)]
pub struct Wheel {
    remainder: f32,
    scale: f32,
}

impl Wheel {
    pub fn add(&mut self, delta: f32, cell_height: f32) -> i32 {
        if self.scale != cell_height || self.remainder.signum() != delta.signum() {
            self.remainder = 0.0;
        }
        self.scale = cell_height;
        self.remainder += delta;
        let steps = (self.remainder / cell_height).trunc() as i32;
        self.remainder -= steps as f32 * cell_height;
        steps
    }

    pub fn reset(&mut self) {
        self.remainder = 0.0;
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_requests_do_not_enable_mouse_tracking() {
        let shell = TermMode::SHOW_CURSOR | TermMode::BRACKETED_PASTE;
        let mut mouse = MouseModes::default();
        assert_eq!(mouse.protocol(shell).mode, shell);
        mouse.observe(b"\x1b[?101");
        mouse.observe(b"5h");
        let protocol = mouse.protocol(shell);
        assert_eq!(protocol.mode, shell);
        assert!(protocol.urxvt);
        let sgr = shell | TermMode::MOUSE_MOTION | TermMode::SGR_MOUSE;
        assert_eq!(mouse.protocol(sgr).mode, sgr);
        assert!(!mouse.protocol(sgr).urxvt);
        mouse.observe(b"\x1b[?1015l");
        assert_eq!(mouse.protocol(shell).mode, shell);
        assert!(!mouse.protocol(shell).urxvt);
        mouse.observe(b"\x1b[?1015h\x1bc");
        assert_eq!(mouse.protocol(shell).mode, shell);
        assert!(!mouse.protocol(shell).urxvt);
    }

    #[test]
    fn fractional_wheel_deltas_accumulate_and_reverse_without_extra_steps() {
        let mut wheel = Wheel::default();
        for _ in 0..20 {
            assert_eq!(wheel.add(-1.0, 21.0), 0);
        }
        assert_eq!(wheel.add(-1.0, 21.0), -1);
        assert_eq!(wheel.add(-10.0, 21.0), 0);
        assert_eq!(wheel.add(21.0, 21.0), 1);
        wheel.add(20.0, 21.0);
        wheel.reset();
        assert_eq!(wheel.add(1.0, 21.0), 0);
    }

    #[test]
    fn a_native_line_tick_is_sent_without_font_height_or_smoothing() {
        let mut wheel = Wheel::default();
        assert_eq!(wheel.add(1.0, 1.0), 1);
        assert_eq!(wheel.add(-1.0, 1.0), -1);
        assert_eq!(wheel.add(20.0, 21.0), 0);
        assert_eq!(wheel.add(1.0, 1.0), 1);
    }
}
