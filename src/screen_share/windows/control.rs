use super::{Source, View, check_desktop};
use crate::screen_share::input::{self, Event};
use anyhow::Result;
use std::{mem, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::{HWND, POINT},
    Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromWindow},
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

const BUTTONS: [(u32, u32); 3] = [
    (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
    (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
];

#[derive(Default)]
pub(super) struct Control {
    held: [bool; 3],
}

impl Control {
    pub(super) fn handle(
        &mut self,
        event: Event,
        source: &Source,
        view: View,
        stopped: impl Fn() -> bool,
    ) -> Result<()> {
        match event {
            Event::Text(text) => {
                for c in text.chars() {
                    if stopped() || foreground(source).is_none() {
                        break;
                    }
                    check_desktop()?;
                    match c {
                        '\r' | '\n' => send_key(VK_RETURN, false, 0),
                        '\t' => send_key(VK_TAB, false, 0),
                        c if !c.is_control() => send_character(c),
                        _ => {}
                    }
                }
            }
            Event::Key(code, modifiers) => {
                check_desktop()?;
                if let Some(window) = foreground(source) {
                    if let Some((key, extended)) = virtual_key(code) {
                        send_key(key, extended, modifiers);
                    } else if let Ok(c) = u16::try_from(code) {
                        let thread = unsafe { GetWindowThreadProcessId(window, null_mut()) };
                        let key = unsafe { VkKeyScanExW(c, GetKeyboardLayout(thread)) };
                        if key != -1 {
                            send_key(
                                (key & 255) as u16,
                                false,
                                modifiers | ((key as u32 >> 8) & 7),
                            );
                        }
                    }
                }
            }
            Event::Mouse(button, x, y, pressed) if button & !(3 | 28 | 32) == 0 => {
                check_desktop()?;
                let button_id = button & 3;
                let motion = button & 32 != 0;
                let point = view.point(x, y, source.bounds()?);
                let movement = point.and_then(|(x, y)| {
                    if let Source::Window(window) = source {
                        let under_pointer = unsafe { WindowFromPoint(POINT { x, y }) };
                        if unsafe { GetAncestor(under_pointer, GA_ROOT) } != window.handle {
                            return None;
                        }
                    }
                    mouse_move(x, y)
                });
                if let Some((down, up)) = BUTTONS.get(button_id).copied() {
                    if !motion && !pressed && self.held[button_id] {
                        if let Some(movement) = movement {
                            send(&[movement]);
                        }
                        self.held[button_id] = send(&[mouse_event(0, 0, up)]) != 1;
                        return Ok(());
                    }
                    if let Some(movement) = movement {
                        if motion {
                            send(&[movement]);
                        } else if pressed && !self.held[button_id] {
                            self.held[button_id] = send(&[movement, mouse_event(0, 0, down)]) == 2;
                        }
                    }
                } else if let Some(movement) = movement {
                    if motion {
                        send(&[movement]);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn release(&mut self) {
        for ((_, up), held) in BUTTONS.into_iter().zip(&mut self.held) {
            if *held {
                *held = send(&[mouse_event(0, 0, up)]) != 1;
            }
        }
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.release();
    }
}

fn foreground(source: &Source) -> Option<HWND> {
    let window = unsafe { GetForegroundWindow() };
    if window.is_null() {
        return None;
    }
    let visible = match source {
        Source::Window(shared) => window == shared.handle && source.bounds().is_ok(),
        Source::Display(display) => unsafe {
            MonitorFromWindow(window, MONITOR_DEFAULTTONULL) == display.handle
        },
    };
    visible.then_some(window)
}

fn send_character(c: char) {
    let mut units = [0; 2];
    let mut events = [key_event(0, 0, 0); 4];
    let mut count = 0;
    for unit in c.encode_utf16(&mut units) {
        events[count] = key_event(0, *unit, KEYEVENTF_UNICODE);
        events[count + 1] = key_event(0, *unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
        count += 2;
    }
    let sent = send(&events[..count]) as usize;
    if sent % 2 == 1 {
        send(&events[sent..sent + 1]);
    }
}

fn virtual_key(code: u32) -> Option<(u16, bool)> {
    Some(match code {
        input::ESCAPE => (VK_ESCAPE, false),
        input::ENTER => (VK_RETURN, false),
        input::TAB => (VK_TAB, false),
        input::BACKSPACE => (VK_BACK, false),
        input::DELETE => (VK_DELETE, true),
        input::INSERT => (VK_INSERT, true),
        input::HOME => (VK_HOME, true),
        input::END => (VK_END, true),
        input::PAGE_UP => (VK_PRIOR, true),
        input::PAGE_DOWN => (VK_NEXT, true),
        input::UP => (VK_UP, true),
        input::DOWN => (VK_DOWN, true),
        input::LEFT => (VK_LEFT, true),
        input::RIGHT => (VK_RIGHT, true),
        code if (input::F1..input::F1 + 12).contains(&code) => {
            (VK_F1 + (code - input::F1) as u16, false)
        }
        _ => return None,
    })
}

fn key_event(key: u16, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key,
                wScan: scan,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}

fn send_key(key: u16, extended: bool, modifiers: u32) {
    let mut events = Vec::with_capacity(10);
    let mut releases = Vec::with_capacity(4);
    for (bit, modifier, flags) in [
        (1, VK_SHIFT, 0),
        (2, VK_MENU, 0),
        (4, VK_CONTROL, 0),
        (8, VK_LWIN, KEYEVENTF_EXTENDEDKEY),
    ] {
        if modifiers & bit != 0 && unsafe { GetAsyncKeyState(i32::from(modifier)) } >= 0 {
            events.push(key_event(modifier, 0, flags));
            releases.push(key_event(modifier, 0, flags | KEYEVENTF_KEYUP));
        }
    }
    let flags = if extended { KEYEVENTF_EXTENDEDKEY } else { 0 };
    events.push(key_event(key, 0, flags));
    let key_up = key_event(key, 0, flags | KEYEVENTF_KEYUP);
    events.push(key_up);
    events.extend(releases.iter().rev().copied());
    let sent = send(&events) as usize;
    if sent != 0 && sent < events.len() {
        // Release only keys whose down event was actually accepted.
        if sent <= releases.len() {
            for release in releases[..sent].iter().rev() {
                send(std::slice::from_ref(release));
            }
        } else {
            send(&events[sent..]);
        }
    }
}

fn mouse_event(x: i32, y: i32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: x,
                dy: y,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}

fn mouse_move(x: i32, y: i32) -> Option<INPUT> {
    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    let (x, y) = (
        i64::from(x) - i64::from(left),
        i64::from(y) - i64::from(top),
    );
    if !(0..i64::from(width)).contains(&x) || !(0..i64::from(height)).contains(&y) {
        return None;
    }
    // Target the center of a pixel on the normalized virtual desktop.
    let normalize =
        |coordinate, extent| (((2 * coordinate + 1) * 32768_i64) / i64::from(extent)) as i32;
    Some(mouse_event(
        normalize(x, width),
        normalize(y, height),
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    ))
}

fn send(events: &[INPUT]) -> u32 {
    unsafe {
        SendInput(
            events.len() as u32,
            events.as_ptr(),
            mem::size_of::<INPUT>() as i32,
        )
    }
}
