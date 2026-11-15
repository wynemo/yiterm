//! Preserve xterm modifiers for navigation/function keys and Alt+Control chords.
pub fn modified(sequence: Vec<u8>, shift: bool, alt: bool, control: bool) -> Vec<u8> {
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(control);
    if modifier == 1 {
        return sequence;
    }
    if sequence.len() == 3
        && sequence[0] == 27
        && matches!(sequence[1], b'[' | b'O')
        && matches!(sequence[2], b'A'..=b'D' | b'H' | b'F' | b'P'..=b'S')
    {
        return format!("\x1b[1;{modifier}{}", sequence[2] as char).into_bytes();
    }
    if sequence.starts_with(b"\x1b[") && sequence.last() == Some(&b'~') {
        let number = String::from_utf8_lossy(&sequence[2..sequence.len() - 1]);
        return format!("\x1b[{number};{modifier}~").into_bytes();
    }
    if alt && sequence.len() == 1 {
        return vec![27, sequence[0]];
    }
    sequence
}
