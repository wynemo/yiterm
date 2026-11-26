//! Optional stream RTT probes. Capability precedes graphics; replies are only
//! inserted between complete escape sequences, never into a Kitty payload.
pub(super) const CAPABILITY: &[u8] = b"\x1b]777;yiterm;v=1\x1b\\";
pub(super) fn pong(token: u64) -> Vec<u8> {
    format!("\x1b]777;yiterm;pong={token}\x1b\\").into_bytes()
}
#[derive(Default)]
pub(super) struct Boundary { state: u8 }
impl Boundary {
    pub fn safe(&self) -> bool { self.state == 0 }
    pub fn consume(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.state = match (self.state, byte) {
                (0, 27) => 1,
                (0, _) => 0,
                (1, b'[') => 2,
                (1, b']' | b'_' | b'P' | b'^' | b'X') => 3,
                (1, _) => 0,
                (2, 64..=126) => 0,
                (2, _) => 2,
                (3, 27) => 4,
                (3, 7) => 0,
                (3, _) => 3,
                (4, b'\\') => 0,
                (4, 27) => 4,
                (4, _) => 3,
                _ => 0,
            }
        }
    }
}
