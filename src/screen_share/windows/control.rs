use super::{Source, View, check_desktop};
use crate::screen_share::input::{self, Event};
use anyhow::{Result, ensure};
use std::{cell::Cell, mem, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::{HWND, POINT},
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

thread_local! {
    static INPUT_REJECTED: Cell<bool> = const { Cell::new(false) };
}

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
    pub(super) fn abandon(&mut self) {
        self.held = [false; 3];
    }

    pub(super) fn handle(
        &mut self,
        event: Event,
        source: &Source,
        view: View,
        stopped: impl Fn() -> bool,
    ) -> Result<()> {
        INPUT_REJECTED.set(false);
        match event {
            Event::Text(text) => send_text(&text, source, true, stopped)?,
            Event::Paste(text) => send_text(&text, source, false, stopped)?,
            Event::Key(code, modifiers) => {
                check_desktop()?;
                if let Some(window) = foreground(source) {
                    if let Some((key, extended)) = virtual_key(code) {
                        send_key(key, extended, modifiers);
                    } else if let Ok(c) = u16::try_from(code) {
                        send_layout_key(window, c, modifiers);
                    }
                }
            }
            Event::Mouse(button, x, y, pressed) => {
                let wheel = input::vertical_wheel(button, pressed);
                if wheel.is_none() && button & !(3 | 28 | 32) != 0 {
                    return Ok(());
                }
                check_desktop()?;
                let button_id = button & 3;
                let motion = button & 32 != 0;
                let rect = source.bounds()?;
                let point = view.point(
                    x,
                    y,
                    (rect.left, rect.top),
                    (rect.right - rect.left, rect.bottom - rect.top),
                );
                let movement = point.and_then(|(x, y)| {
                    if let Source::Window(window) = source {
                        let under_pointer = unsafe { WindowFromPoint(POINT { x, y }) };
                        if unsafe { GetAncestor(under_pointer, GA_ROOT) } != window.handle {
                            return None;
                        }
                    }
                    mouse_move(x, y)
                });
                if let Some(direction) = wheel {
                    // Windows can route scrolling to the foreground window.
                    if let Some(movement) = movement
                        && (matches!(source, Source::Display(_)) || foreground(source).is_some())
                    {
                        let delta = direction * WHEEL_DELTA as i32;
                        send(&[movement, mouse_event(0, 0, MOUSEEVENTF_WHEEL, delta as u32)]);
                    }
                    return Ok(());
                }
                if let Some((down, up)) = BUTTONS.get(button_id).copied() {
                    if !motion && !pressed && self.held[button_id] {
                        if let Some(movement) = movement {
                            send(&[movement]);
                        }
                        self.held[button_id] = send(&[mouse_event(0, 0, up, 0)]) != 1;
                        return Ok(());
                    }
                    if let Some(movement) = movement {
                        if motion {
                            send(&[movement]);
                        } else if pressed && !self.held[button_id] {
                            self.held[button_id] =
                                send(&[movement, mouse_event(0, 0, down, 0)]) == 2;
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
        ensure!(
            !INPUT_REJECTED.get(),
            "Windows 拒绝远程输入，请确认共享端已以管理员身份启动，并重新启动共享"
        );
        Ok(())
    }

    pub(super) fn release(&mut self) {
        for ((_, up), held) in BUTTONS.into_iter().zip(&mut self.held) {
            if *held {
                *held = send(&[mouse_event(0, 0, up, 0)]) != 1;
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
    match source {
        // Winlogon can have no foreground HWND while the clock curtain is up.
        // Full-display input targets the bound desktop, not a particular window.
        Source::Display(_) => Some(window),
        Source::Window(shared) => {
            (!window.is_null() && window == shared.handle && source.bounds().is_ok())
                .then_some(window)
        }
    }
}

fn send_text(text: &str, source: &Source, typed: bool, stopped: impl Fn() -> bool) -> Result<()> {
    for c in text.chars() {
        if stopped() {
            break;
        }
        let Some(window) = foreground(source) else {
            break;
        };
        check_desktop()?;
        match c {
            '\r' | '\n' => send_key(VK_RETURN, false, 0),
            '\t' => send_key(VK_TAB, false, 0),
            // Keep ordinary typing in the host's IME. Paste and non-ASCII text
            // are already composed and use Unicode input.
            ' '..='~' if typed && send_layout_key(window, c as u16, 0) => {}
            c if !c.is_control() => send_character(c),
            _ => {}
        }
    }
    Ok(())
}

fn send_layout_key(window: HWND, c: u16, modifiers: u32) -> bool {
    let thread = if window.is_null() {
        0
    } else {
        unsafe { GetWindowThreadProcessId(window, null_mut()) }
    };
    let layout = unsafe { GetKeyboardLayout(thread) };
    let mapped = unsafe { VkKeyScanExW(c, layout) };
    if mapped == -1 {
        return false;
    }
    let [key, shift_state] = mapped.to_le_bytes();
    if shift_state & !7 != 0 {
        return false;
    }
    // Win32 orders these bits Shift, Control, Alt. The decoder uses Shift, Alt, Control.
    let shift_state = u32::from(shift_state);
    let modifiers =
        modifiers | (shift_state & 1) | ((shift_state & 2) << 1) | ((shift_state & 4) >> 1);
    let scan = unsafe { MapVirtualKeyExW(u32::from(key), MAPVK_VK_TO_VSC_EX, layout) };
    let (key, scan, flags) = match (scan >> 8, scan as u8) {
        (0, code) if code != 0 => (0, u16::from(code), KEYEVENTF_SCANCODE),
        (0xe0, code) => (
            0,
            u16::from(code),
            KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY,
        ),
        _ => (u16::from(key), 0, 0),
    };
    tap_key(key, scan, flags, modifiers);
    true
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
    // LogonUI expects physical keys, including Enter, Tab and modifiers.
    // Unicode packets already contain UTF-16 units and must remain unchanged.
    let (key, scan, flags) = if key != 0 && flags & KEYEVENTF_UNICODE == 0 {
        let layout = unsafe { GetKeyboardLayout(0) };
        let mapped = unsafe { MapVirtualKeyExW(u32::from(key), MAPVK_VK_TO_VSC_EX, layout) };
        if mapped != 0 && mapped >> 8 != 0xe1 {
            (
                0,
                (mapped & 0xff) as u16,
                flags
                    | KEYEVENTF_SCANCODE
                    | if mapped >> 8 == 0xe0 {
                        KEYEVENTF_EXTENDEDKEY
                    } else {
                        0
                    },
            )
        } else {
            (key, scan, flags)
        }
    } else {
        (key, scan, flags)
    };
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
    let flags = if extended { KEYEVENTF_EXTENDEDKEY } else { 0 };
    tap_key(key, 0, flags, modifiers);
}

fn tap_key(key: u16, scan: u16, flags: u32, modifiers: u32) {
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
    events.push(key_event(key, scan, flags));
    events.push(key_event(key, scan, flags | KEYEVENTF_KEYUP));
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

fn mouse_event(x: i32, y: i32, flags: u32, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: x,
                dy: y,
                mouseData: data,
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
        0,
    ))
}

fn send(events: &[INPUT]) -> u32 {
    // Match the native controller: submit each transition individually.
    let mut sent = 0;
    for event in events {
        let accepted = unsafe { SendInput(1, event, mem::size_of::<INPUT>() as i32) };
        if accepted != 1 {
            INPUT_REJECTED.set(true);
            break;
        }
        sent += 1;
    }
    sent
}
