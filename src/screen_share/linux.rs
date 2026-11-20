use super::{
    Options,
    desktop::{self, Desktop, View, capture_size},
    graphics::Frame,
    input::{self, Event},
};
use anyhow::{Context, Result, ensure};
use portable_pty::PtySize;
use tokio::sync::{mpsc, watch};
use x11rb::{
    connection::Connection,
    image::{BitsPerPixel, Image, ImageOrder, PixelLayout},
    protocol::{xproto::*, xtest},
    rust_connection::RustConnection,
};
use zeroize::Zeroizing;

fn connect() -> Result<(RustConnection, usize)> {
    let (connection, screen) = x11rb::connect(None)
        .context("Linux sharing needs access to an X11 desktop (DISPLAY and Xauthority)")?;
    ensure!(
        !matches!(std::env::var("XDG_SESSION_TYPE").as_deref(), Ok("wayland"))
            && !connection.query_extension(b"XWAYLAND")?.reply()?.present,
        "Linux YiTerm sharing requires an X11 session. Log in to an Xorg desktop."
    );
    Ok((connection, screen))
}

pub fn list_sources() -> Result<()> {
    let (connection, default) = connect()?;
    for (id, screen) in connection.setup().roots.iter().enumerate() {
        let geometry = connection.get_geometry(screen.root)?.reply()?;
        let label = if id == default { " (default)" } else { "" };
        println!(
            "Display {id}: {}x{}{label}",
            geometry.width, geometry.height
        );
    }
    Ok(())
}

pub fn require_desktop(options: &Options) -> Result<()> {
    X11Desktop::open(options)?.geometry()?;
    Ok(())
}

pub fn stream(
    options: &Options,
    sizes: watch::Receiver<PtySize>,
    input: mpsc::Receiver<Zeroizing<Vec<u8>>>,
    output: &mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    desktop::stream(X11Desktop::open(options)?, options, sizes, input, output)
}

struct X11Desktop {
    connection: RustConnection,
    root: Window,
    colors: PixelLayout,
    bgr_masks: bool,
    displayed_size: (i32, i32),
    buttons: [bool; 3],
}

impl X11Desktop {
    fn open(options: &Options) -> Result<Self> {
        ensure!(
            options.share_window.is_none(),
            "Linux sharing captures a full X11 screen. Use --share-display ID to choose a screen."
        );
        let (connection, default) = connect()?;
        let id = options.share_display.map_or(default, |id| id as usize);
        let screen = connection
            .setup()
            .roots
            .get(id)
            .context("X11 screen is unavailable. Run --share-screen --share-list")?;
        let visual = screen
            .allowed_depths
            .iter()
            .flat_map(|depth| &depth.visuals)
            .find(|visual| {
                visual.visual_id == screen.root_visual && visual.class == VisualClass::TRUE_COLOR
            })
            .context("Linux screen sharing requires a TrueColor X11 visual")?;
        let colors = PixelLayout::from_visual_type(*visual)?;
        let bgr_masks = (visual.red_mask, visual.green_mask, visual.blue_mask)
            == (0x00ff0000, 0x0000ff00, 0x000000ff);
        if !options.share_read_only {
            xtest::get_version(&connection, 2, 2)
                .context("X11 control needs the XTEST extension. Use --share-read-only to share the screen.")?
                .reply()
                .context("Could not initialize X11 input control")?;
        }
        Ok(Self {
            root: screen.root,
            connection,
            colors,
            bgr_masks,
            displayed_size: (0, 0),
            buttons: [false; 3],
        })
    }

    fn geometry(&self) -> Result<(u16, u16)> {
        let reply = self.connection.get_geometry(self.root)?.reply()?;
        let size = (reply.width, reply.height);
        ensure!(
            (1..=16384).contains(&size.0)
                && (1..=16384).contains(&size.1)
                && usize::from(size.0) * usize::from(size.1) * 4 <= 96 * 1024 * 1024,
            "X11 capture dimensions exceed the memory budget"
        );
        Ok(size)
    }

    fn fake_input(&self, event: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        let root = if event == MOTION_NOTIFY_EVENT {
            self.root
        } else {
            x11rb::NONE
        };
        // Queue complete event sequences without a round trip per request.
        xtest::fake_input(
            &self.connection,
            event,
            detail,
            x11rb::CURRENT_TIME,
            root,
            x,
            y,
            0,
        )?
        .ignore_error();
        Ok(())
    }

    fn button(&mut self, index: usize, pressed: bool) -> Result<()> {
        let event = if pressed {
            BUTTON_PRESS_EVENT
        } else {
            BUTTON_RELEASE_EVENT
        };
        self.fake_input(event, index as u8 + 1, 0, 0)?;
        self.buttons[index] = pressed;
        Ok(())
    }

    fn pointer(
        &mut self,
        view: View,
        button: usize,
        x: usize,
        y: usize,
        pressed: bool,
    ) -> Result<()> {
        let wheel = input::vertical_wheel(button, pressed);
        if wheel.is_none() && button & !(3 | 28 | 32) != 0 {
            return Ok(());
        }
        let index = button & 3;
        let motion = button & 32 != 0;
        let point = view.point(x, y, (0, 0), self.displayed_size);
        if let Some((x, y)) = point {
            self.fake_input(MOTION_NOTIFY_EVENT, 0, x as i16, y as i16)?;
            if let Some(direction) = wheel {
                let detail = if direction > 0 { 4 } else { 5 };
                self.fake_input(BUTTON_PRESS_EVENT, detail, 0, 0)?;
                self.fake_input(BUTTON_RELEASE_EVENT, detail, 0, 0)?;
                return Ok(());
            }
        }
        if !motion && index < self.buttons.len() {
            if pressed && point.is_some() && !self.buttons[index] {
                self.button(index, true)?;
            } else if !pressed && self.buttons[index] {
                self.button(index, false)?;
            }
        }
        Ok(())
    }

    fn keyboard(
        &self,
        keys: impl Iterator<Item = (u32, u32)>,
        stopped: impl Fn() -> bool,
    ) -> Result<()> {
        // XTEST keys target the focused window. Keep them on the shared X screen.
        let focus = self.connection.get_input_focus()?.reply()?.focus;
        match focus {
            0 => return Ok(()),
            1 if !self
                .connection
                .query_pointer(self.root)?
                .reply()?
                .same_screen =>
            {
                return Ok(());
            }
            1 => {}
            window => match self.connection.get_geometry(window)?.reply() {
                Ok(geometry) if geometry.root == self.root => {}
                _ => return Ok(()),
            },
        }

        let setup = self.connection.setup();
        let first = setup.min_keycode;
        let count = setup.max_keycode - first + 1;
        let mapping = self
            .connection
            .get_keyboard_mapping(first, count)?
            .reply()?;
        let stride = usize::from(mapping.keysyms_per_keycode);
        ensure!(stride != 0, "X11 keyboard mapping is empty");

        // Resolve the two core levels of the host's primary X11 keyboard layout.
        let lookup = |symbol| {
            mapping
                .keysyms
                .chunks_exact(stride)
                .enumerate()
                .find_map(|(index, levels)| {
                    levels
                        .iter()
                        .take(2)
                        .position(|&value| value == symbol)
                        .map(|level| (first + index as u8, level == 1))
                })
        };
        let mut modifiers = [None; 4];
        for (slot, symbols) in modifiers.iter_mut().zip([
            [0xffe1, 0xffe2], // Shift
            [0xffe9, 0xffea], // Alt
            [0xffe3, 0xffe4], // Control
            [0xffeb, 0xffec], // Super
        ]) {
            *slot = symbols
                .into_iter()
                .find_map(|symbol| lookup(symbol).map(|(key, _)| key));
        }
        let down = self.connection.query_keymap()?.reply()?.keys;
        let held = |key: u8| down[usize::from(key / 8)] & (1 << (key % 8)) != 0;
        for (symbol, mut flags) in keys.take_while(|_| !stopped()) {
            let Some((key, shift)) = lookup(symbol) else {
                continue;
            };
            if held(key) {
                continue;
            }
            flags |= u32::from(shift);
            if (0..4).any(|bit| flags & (1 << bit) != 0 && modifiers[bit].is_none()) {
                continue;
            }
            let mut pressed = [None; 4];
            for (bit, code) in modifiers.into_iter().enumerate() {
                if let Some(code) = code
                    && flags & (1 << bit) != 0
                    && !held(code)
                {
                    self.fake_input(KEY_PRESS_EVENT, code, 0, 0)?;
                    pressed[bit] = Some(code);
                }
            }
            self.fake_input(KEY_PRESS_EVENT, key, 0, 0)?;
            self.fake_input(KEY_RELEASE_EVENT, key, 0, 0)?;
            for code in pressed.into_iter().rev().flatten() {
                self.fake_input(KEY_RELEASE_EVENT, code, 0, 0)?;
            }
        }
        Ok(())
    }
}

impl Desktop for X11Desktop {
    fn frame(&mut self, width: u32, divisor: usize) -> Result<Frame> {
        let (mut sw, mut sh) = self.geometry()?;
        let capture = |w, h| Image::get(&self.connection, self.root, 0, 0, w, h);
        let (image, _) = match capture(sw, sh) {
            Ok(image) => image,
            Err(error) => {
                let size = self.geometry()?;
                if size == (sw, sh) {
                    return Err(error).context("Cannot capture the X11 screen");
                }
                (sw, sh) = size;
                capture(sw, sh).context("Cannot capture the resized X11 screen")?
            }
        };
        self.displayed_size = (i32::from(sw), i32::from(sh));
        let (w, h) = capture_size((usize::from(sw), usize::from(sh)), width, divisor);
        let mut pixels = Vec::with_capacity(w * h * 4);
        let (sw, sh) = (usize::from(sw), usize::from(sh));
        let native_bgr = image.bits_per_pixel() == BitsPerPixel::B32
            && image.byte_order() == ImageOrder::LsbFirst
            && self.bgr_masks;
        for y in 0..h {
            let sy = y * sh / h;
            for x in 0..w {
                let sx = x * sw / w;
                if native_bgr {
                    let offset = (sy * sw + sx) * 4;
                    let color = &image.data()[offset..offset + 3];
                    pixels.extend_from_slice(&[color[0], color[1], color[2], 0]);
                } else {
                    let pixel = image.get_pixel(sx as u16, sy as u16);
                    let (r, g, b) = self.colors.decode(pixel);
                    pixels.extend_from_slice(&[(b >> 8) as u8, (g >> 8) as u8, (r >> 8) as u8, 0]);
                }
            }
        }
        Ok(Frame { w, h, pixels })
    }

    fn input(&mut self, event: Event, view: View, stopped: impl Fn() -> bool) -> Result<()> {
        match event {
            Event::Mouse(button, x, y, pressed) => self.pointer(view, button, x, y, pressed)?,
            Event::Key(code, modifiers) => self.keyboard(
                keysym(code).map(|sym| (sym, modifiers)).into_iter(),
                stopped,
            )?,
            Event::Text(text) => self.keyboard(
                text.chars()
                    .filter_map(|ch| text_keysym(u32::from(ch)).map(|sym| (sym, 0))),
                stopped,
            )?,
            _ => {}
        }
        self.connection.flush()?;
        Ok(())
    }

    fn release(&mut self) {
        for index in 0..self.buttons.len() {
            if self.buttons[index] {
                let _ = self.button(index, false);
            }
        }
        let _ = self.connection.flush();
    }
}

impl Drop for X11Desktop {
    fn drop(&mut self) {
        self.release();
    }
}

fn keysym(code: u32) -> Option<u32> {
    Some(match code {
        input::ESCAPE => 0xff1b,
        input::ENTER => 0xff0d,
        input::TAB => 0xff09,
        input::BACKSPACE => 0xff08,
        input::DELETE => 0xffff,
        input::INSERT => 0xff63,
        input::HOME => 0xff50,
        input::END => 0xff57,
        input::PAGE_UP => 0xff55,
        input::PAGE_DOWN => 0xff56,
        input::UP => 0xff52,
        input::DOWN => 0xff54,
        input::LEFT => 0xff51,
        input::RIGHT => 0xff53,
        code if (input::F1..input::F1 + 12).contains(&code) => 0xffbe + (code - input::F1),
        _ => return text_keysym(code),
    })
}

fn text_keysym(code: u32) -> Option<u32> {
    Some(match code {
        10 | 13 => 0xff0d,
        9 => 0xff09,
        0x20..=0x7e | 0xa0..=0xff => code,
        code if char::from_u32(code).is_some_and(|ch| !ch.is_control()) => 0x01000000 | code,
        _ => return None,
    })
}
