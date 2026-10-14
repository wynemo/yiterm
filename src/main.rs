use yiterm::{mouse, paste, ssh};

#[cfg(windows)]
mod conpty;

macro_rules! mouse_trace {
    ($($arg:tt)*) => {
        if cfg!(windows) {
            eprintln!("[yiterm mouse] {}", format_args!($($arg)*));
        }
    };
}

use alacritty_terminal::{
    Term,
    event::{Event, EventListener},
    grid::{Dimensions, Scroll},
    term::{Config, TermMode, cell::Flags},
    vte::ansi::{Color, NamedColor, Processor},
};
use anyhow::{Context, Result};
use clap::Parser;
use eframe::egui::{self, Color32, FontId, Key, Rect, Vec2};
#[cfg(test)]
use portable_pty::CommandBuilder;
use portable_pty::{MasterPty, PtySize};
use std::{
    io::{Read, Write},
    sync::mpsc,
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(version, about = "A small native terminal with clipboard image paste")]
struct Args {
    /// Connect directly to this OpenSSH host (remote shell must be POSIX).
    #[arg(long)]
    ssh: Option<String>,
    /// Local shell executable. Defaults to $SHELL or powershell.exe.
    #[arg(long)]
    shell: Option<String>,
}

#[derive(Clone, Copy)]
struct Size {
    cols: usize,
    rows: usize,
}
impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}
#[derive(Clone)]
struct Listener(mpsc::Sender<Event>);
impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let _ = self.0.send(event);
    }
}

struct PasteResult {
    text: String,
    target: Option<ssh::TargetIdentity>,
    image_group: Option<Option<i32>>,
}
struct TerminalApp {
    term: Term<Listener>,
    parser: Processor,
    mouse_modes: mouse::MouseModes,
    mouse_wheel: mouse::Wheel,
    output: mpsc::Receiver<Vec<u8>>,
    events: mpsc::Receiver<Event>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    integration: ssh::Integration,
    paste_result: mpsc::Receiver<Result<PasteResult>>,
    paste_sender: mpsc::Sender<Result<PasteResult>>,
    paste_busy: bool,
    ime_preedit: String,
    mouse_button: Option<egui::PointerButton>,
    cursor_blink_started: Instant,
    status: String,
    size: Size,
}
impl TerminalApp {
    fn new(args: Args) -> Result<Self> {
        mouse_trace!(
            "build={} transport=raw_vt",
            option_env!("YITERM_BUILD_REV").unwrap_or("local")
        );
        let integration = ssh::Integration::new(&std::env::current_exe()?)?;
        if let Some(host) = &args.ssh {
            ssh::parse(std::slice::from_ref(host))?;
        }
        #[cfg(windows)]
        conpty::initialize()?;
        let pair = portable_pty::native_pty_system().openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut command = if let Some(host) = &args.ssh {
            integration.direct(host)?
        } else {
            let shell = args.shell.unwrap_or_else(|| {
                if cfg!(windows) {
                    "powershell.exe".into()
                } else {
                    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
                }
            });
            integration.shell(&shell)?
        };
        command.env("TERM", "xterm-256color");
        let child = pair
            .slave
            .spawn_command(command)
            .context("Could not start shell/ssh")?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let (tx, output) = mpsc::sync_channel(64);
        std::thread::spawn(move || {
            let mut buffer = [0; 16384];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buffer[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let (tx, events) = mpsc::channel();
        let size = Size { cols: 80, rows: 24 };
        let mut config = Config::default();
        config.default_cursor_style.blinking = true;
        let term = Term::new(config, &size, Listener(tx));
        let (paste_sender, paste_result) = mpsc::channel();
        Ok(Self {
            term,
            parser: Processor::new(),
            mouse_modes: mouse::MouseModes::default(),
            mouse_wheel: mouse::Wheel::default(),
            output,
            events,
            writer,
            master: pair.master,
            child,
            integration,
            paste_result,
            paste_sender,
            paste_busy: false,
            ime_preedit: String::new(),
            mouse_button: None,
            cursor_blink_started: Instant::now(),
            status: "Ready · paste: Cmd+V / Ctrl+Shift+V / Shift+Insert".into(),
            size,
        })
    }
    fn write(&mut self, bytes: &[u8]) {
        if let Err(error) = self
            .writer
            .write_all(bytes)
            .and_then(|_| self.writer.flush())
        {
            self.status = error.to_string();
        }
    }
    fn write_mouse(&mut self, report: &[u8]) {
        mouse_trace!(
            "send protocol={:?} cols={} rows={} hex={}",
            self.mouse_modes.protocol(*self.term.mode()),
            self.size.cols,
            self.size.rows,
            mouse::hex(report)
        );
        match self
            .writer
            .write_all(report)
            .and_then(|_| self.writer.flush())
        {
            Ok(()) => mouse_trace!("write_ok"),
            Err(error) => {
                mouse_trace!("write_error={error}");
                self.status = error.to_string();
            }
        }
    }
    fn paste(&mut self) {
        if self.paste_busy {
            return;
        }
        self.paste_busy = true;
        self.status = "Reading clipboard / uploading image…".into();
        let tx = self.paste_sender.clone();
        let root = self.child.process_id();
        let group = self.foreground_group();
        let scp = self.integration.real_scp.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<PasteResult> {
                let mut clipboard = arboard::Clipboard::new()?;
                if let Ok(image) = clipboard.get_image() {
                    let path = paste::save_image(image)?;
                    let root = root.context("Shell process is unavailable")?;
                    let detected = ssh::detect(root, group)?;
                    let (text, target) = if let Some(detected) = detected {
                        (
                            paste::upload(&path, &detected.remote, &scp)?,
                            Some(detected.identity),
                        )
                    } else if cfg!(windows) {
                        (paste::quote_powershell(&path.to_string_lossy()), None)
                    } else {
                        (paste::quote_posix(&path.to_string_lossy()), None)
                    };
                    Ok(PasteResult {
                        text,
                        target,
                        image_group: Some(group),
                    })
                } else {
                    Ok(PasteResult {
                        text: clipboard
                            .get_text()
                            .context("Clipboard has no image or text")?,
                        target: None,
                        image_group: None,
                    })
                }
            })();
            let _ = tx.send(result);
        });
    }
    fn foreground_group(&self) -> Option<i32> {
        #[cfg(unix)]
        {
            self.master.process_group_leader()
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
    fn paste_text(&mut self, text: &str) {
        if self.term.mode().contains(TermMode::BRACKETED_PASTE) {
            let safe = text.replace('\x1b', "");
            self.write(format!("\x1b[200~{safe}\x1b[201~").as_bytes());
        } else {
            self.write(text.replace("\r\n", "\n").replace('\n', "\r").as_bytes());
        }
    }
}
impl Drop for TerminalApp {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn color(color: Color, foreground: bool) -> Color32 {
    const PALETTE: [Color32; 16] = [
        Color32::from_rgb(24, 26, 32),
        Color32::from_rgb(224, 108, 117),
        Color32::from_rgb(152, 195, 121),
        Color32::from_rgb(229, 192, 123),
        Color32::from_rgb(97, 175, 239),
        Color32::from_rgb(198, 120, 221),
        Color32::from_rgb(86, 182, 194),
        Color32::from_rgb(171, 178, 191),
        Color32::from_rgb(92, 99, 112),
        Color32::from_rgb(255, 128, 139),
        Color32::from_rgb(178, 219, 147),
        Color32::from_rgb(255, 218, 149),
        Color32::from_rgb(123, 201, 255),
        Color32::from_rgb(224, 146, 247),
        Color32::from_rgb(112, 208, 220),
        Color32::from_rgb(230, 230, 230),
    ];
    let index = match color {
        Color::Spec(rgb) => return Color32::from_rgb(rgb.r, rgb.g, rgb.b),
        Color::Named(NamedColor::Foreground) => return PALETTE[7],
        Color::Named(NamedColor::Background) => return PALETTE[0],
        Color::Named(named) => named as usize,
        Color::Indexed(index) => index as usize,
    };
    if index < 16 {
        PALETTE[index]
    } else if index < 232 {
        let n = index - 16;
        let level = |n| if n == 0 { 0 } else { 55 + n as u8 * 40 };
        Color32::from_rgb(level(n / 36), level(n / 6 % 6), level(n % 6))
    } else if index < 256 {
        Color32::from_gray(8 + (index - 232) as u8 * 10)
    } else if foreground {
        PALETTE[7]
    } else {
        PALETTE[0]
    }
}

impl eframe::App for TerminalApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        // Keep the cursor visible while typing and restart its blink on focus.
        if ctx.input(|input| {
            input.events.iter().any(|event| {
                matches!(
                    event,
                    egui::Event::Key { pressed: true, .. }
                        | egui::Event::Text(_)
                        | egui::Event::Paste(_)
                        | egui::Event::Ime(_)
                        | egui::Event::WindowFocused(true)
                )
            })
        }) {
            self.cursor_blink_started = Instant::now();
        }
        for _ in 0..128 {
            let Ok(bytes) = self.output.try_recv() else {
                break;
            };
            let previous_mode = *self.term.mode();
            let requests = self.mouse_modes.observe(&bytes);
            self.parser.advance(&mut self.term, &bytes);
            for (mode, enabled) in requests {
                mouse_trace!("request mode={mode} enabled={enabled}");
            }
            if previous_mode != *self.term.mode() {
                mouse_trace!("state {:?}", self.term.mode());
            }
            self.cursor_blink_started = Instant::now();
        }
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::PtyWrite(text) => self.write(text.as_bytes()),
                Event::Title(title) => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("yiterm — {title}")))
                }
                _ => {}
            }
        }
        if let Ok(result) = self.paste_result.try_recv() {
            self.paste_busy = false;
            match result {
                Ok(result) => {
                    let group = self.foreground_group();
                    if result.image_group.is_some_and(|expected| expected != group)
                        || result
                            .target
                            .is_some_and(|target| !ssh::target_is_alive(target, group))
                    {
                        self.status =
                            "Session changed during image upload; path was not pasted".into();
                    } else {
                        self.paste_text(&result.text);
                        self.status = if result.target.is_some() {
                            "Uploaded image and pasted remote path".into()
                        } else {
                            "Pasted".into()
                        };
                    }
                }
                Err(error) => self.status = format!("{error:#}"),
            }
        }
        if let Ok(Some(_)) = self.child.try_wait() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Paste").clicked() {
                    self.paste();
                }
                ui.label(&self.status);
            });
        });
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::from_rgb(24, 26, 32))).show(ctx, |ui| {
            let rect = ui.available_rect_before_wrap();
            let font = FontId::monospace(16.0);
            let cell = Vec2::new(ui.fonts_mut(|f| f.glyph_width(&font, 'M')), 21.0);
            let size = Size { cols: ((rect.width() / cell.x) as usize).max(2), rows: ((rect.height() / cell.y) as usize).max(1) };
            if size.cols != self.size.cols || size.rows != self.size.rows {
                if let Err(error) = self.master.resize(PtySize { cols: size.cols as u16, rows: size.rows as u16, pixel_width: 0, pixel_height: 0 }) { self.status = error.to_string(); }
                self.term.resize(size);
                self.size = size;
            }
            let scroll = ctx.input(|i| i.smooth_scroll_delta.y);
            let mouse_protocol = self.mouse_modes.protocol(*self.term.mode());
            let mouse_mode = mouse_protocol.mode;
            let report_mouse = mouse_mode.intersects(TermMode::MOUSE_MODE)
                && !ctx.input(|i| i.modifiers.shift)
                && self.term.grid().display_offset() == 0;
            if !report_mouse { self.mouse_wheel.reset(); }
            if scroll != 0.0 && !report_mouse {
                self.term.scroll_display(Scroll::Delta((scroll / cell.y).round() as i32));
            }
            let content = self.term.renderable_content();
            let offset = content.display_offset;
            let cells: Vec<_> = content.display_iter.collect();
            // Paint all backgrounds first: a wide glyph extends into its spacer
            // cell, whose background would otherwise erase the glyph's right half.
            for indexed in &cells {
                let row = indexed.point.line.0 + offset as i32;
                if row < 0 { continue; }
                let pos = rect.min + Vec2::new(indexed.point.column.0 as f32 * cell.x, row as f32 * cell.y);
                let bg = if indexed.cell.flags.contains(Flags::INVERSE) {
                    color(indexed.cell.fg, true)
                } else {
                    color(indexed.cell.bg, false)
                };
                ui.painter().rect_filled(Rect::from_min_size(pos, cell), 0.0, bg);
            }
            for indexed in &cells {
                let row = indexed.point.line.0 + offset as i32;
                if row < 0 { continue; }
                let pos = rect.min + Vec2::new(indexed.point.column.0 as f32 * cell.x, row as f32 * cell.y);
                let fg = if indexed.cell.flags.contains(Flags::INVERSE) {
                    color(indexed.cell.bg, false)
                } else {
                    color(indexed.cell.fg, true)
                };
                if !indexed.cell.flags.intersects(Flags::HIDDEN | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    let mut text = indexed.cell.c.to_string();
                    if let Some(extra) = indexed.cell.zerowidth() { text.extend(extra); }
                    ui.painter().text(pos, egui::Align2::LEFT_TOP, text, font.clone(), fg);
                    if indexed.cell.flags.contains(Flags::UNDERLINE) {
                        let width = if indexed.cell.flags.contains(Flags::WIDE_CHAR) { 2.0 * cell.x } else { cell.x };
                        ui.painter().line_segment([pos + Vec2::new(0.0, cell.y - 2.0), pos + Vec2::new(width, cell.y - 2.0)], (1.0, fg));
                    }
                }
            }
            let blink_visible = !ctx.input(|input| input.focused)
                || !self.term.cursor_style().blinking
                || self.cursor_blink_started.elapsed().as_millis() % 1000 < 500;
            if offset == 0 && self.term.mode().contains(TermMode::SHOW_CURSOR) && blink_visible {
                let point = content.cursor.point;
                let pos = rect.min + Vec2::new(point.column.0 as f32 * cell.x, point.line.0 as f32 * cell.y);
                ui.painter().rect_stroke(Rect::from_min_size(pos, cell), 0.0, (1.0, Color32::LIGHT_GRAY), egui::StrokeKind::Inside);
            }
            let point = content.cursor.point;
            let cursor_pos = rect.min + Vec2::new(point.column.0 as f32 * cell.x, point.line.0 as f32 * cell.y);
            let cursor_rect = Rect::from_min_size(cursor_pos, cell);
            ctx.output_mut(|output| {
                output.ime = Some(egui::output::IMEOutput { rect: cursor_rect, cursor_rect });
            });
            let events = ctx.input(|i| i.events.clone());
            // Consume candidate-selection keys even if Commit clears the preedit
            // earlier in this frame. They must not execute a shell command.
            let composing = ime_owns_keyboard(&self.ime_preedit, &events);
            let paste_shortcut = events.iter().any(|event| matches!(event, egui::Event::Key { key, pressed: true, modifiers, .. } if is_paste_shortcut(*key, *modifiers)));
            if paste_shortcut { self.paste(); }
            for event in events {
                match event {
                    egui::Event::MouseWheel { unit, delta, modifiers } => {
                        mouse_trace!("wheel unit={unit:?} delta={} report={report_mouse}", delta.y);
                        if !report_mouse || modifiers.shift || delta.y == 0.0 { continue; }
                        let Some(pos) = ctx.input(|i| i.pointer.hover_pos()).filter(|pos| rect.contains(*pos)) else {
                            self.mouse_wheel.reset();
                            continue;
                        };
                        let (units_per_step, multiplier) = match unit {
                            egui::MouseWheelUnit::Line => (1.0, 1),
                            egui::MouseWheelUnit::Point => (cell.y, 1),
                            egui::MouseWheelUnit::Page => (1.0, size.rows as i32),
                        };
                        let steps = self.mouse_wheel.add(delta.y, units_per_step) * multiplier;
                        let button = if steps > 0 { 64 } else { 65 };
                        for _ in 0..steps.unsigned_abs() {
                            if let Some(bytes) = mouse_bytes(mouse_protocol, rect, cell, size, pos, (button, true), modifiers) {
                                self.write_mouse(&bytes);
                            }
                        }
                    },
                    egui::Event::PointerButton { pos, button, pressed, modifiers } => {
                        let tracked = self.mouse_button == Some(button);
                        mouse_trace!("button={button:?} pressed={pressed} report={report_mouse} inside={} shift={} offset={} pos={pos:?} rect={rect:?} cell={cell:?} mode={mouse_mode:?}", rect.contains(pos), modifiers.shift, self.term.grid().display_offset());
                        if report_mouse && (rect.contains(pos) || (!pressed && tracked))
                            && let Some(code) = mouse_button_code(button) {
                                if let Some(bytes) = mouse_bytes(mouse_protocol, rect, cell, size, pos, (code, pressed), modifiers) {
                                    self.write_mouse(&bytes);
                                }
                                if pressed { self.mouse_button = Some(button); }
                        }
                        if !pressed && tracked { self.mouse_button = None; }
                    },
                    egui::Event::PointerMoved(pos) if report_mouse => {
                        if rect.contains(pos) && (mouse_mode.contains(TermMode::MOUSE_MOTION)
                            || (mouse_mode.contains(TermMode::MOUSE_DRAG) && self.mouse_button.is_some())) {
                            let code = self.mouse_button.and_then(mouse_button_code).unwrap_or(3) + 32;
                            if let Some(bytes) = mouse_bytes(mouse_protocol, rect, cell, size, pos, (code, true), ctx.input(|i| i.modifiers)) {
                                self.write(&bytes);
                            }
                        }
                    },
                    egui::Event::Paste(_) if paste_shortcut => {},
                    egui::Event::Paste(_) => self.paste(),
                    egui::Event::Ime(egui::ImeEvent::Preedit(text)) => { self.ime_preedit = text; },
                    egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                        self.ime_preedit.clear();
                        self.term.scroll_display(Scroll::Bottom);
                        self.write(text.as_bytes());
                    },
                    egui::Event::Ime(egui::ImeEvent::Disabled) | egui::Event::WindowFocused(false) => { self.ime_preedit.clear(); },
                    egui::Event::Text(_) if composing => {},
                    egui::Event::Text(text) => { self.term.scroll_display(Scroll::Bottom); self.write(text.as_bytes()); },
                    egui::Event::Key { key, pressed: true, modifiers, .. } => {
                        if composing { continue; }
                        if is_paste_shortcut(key, modifiers) { continue; }
                        let app_cursor = self.term.mode().contains(TermMode::APP_CURSOR);
                        if let Some(bytes) = key_bytes(key, modifiers, app_cursor) {
                            self.term.scroll_display(Scroll::Bottom);
                            self.write(&bytes);
                        }
                    }
                    _ => {},
                }
            }
            if !self.ime_preedit.is_empty() {
                let galley = ui.painter().layout_no_wrap(self.ime_preedit.clone(), font.clone(), Color32::WHITE);
                ui.painter().rect_filled(Rect::from_min_size(cursor_pos, galley.size()), 0.0, Color32::from_rgb(24, 26, 32));
                ui.painter().galley(cursor_pos, galley, Color32::WHITE);
            }
        });
        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

fn mouse_button_code(button: egui::PointerButton) -> Option<u8> {
    match button {
        egui::PointerButton::Primary => Some(0),
        egui::PointerButton::Middle => Some(1),
        egui::PointerButton::Secondary => Some(2),
        _ => None,
    }
}

fn mouse_bytes(
    protocol: mouse::Protocol,
    rect: Rect,
    cell: Vec2,
    size: Size,
    pos: egui::Pos2,
    action: (u8, bool),
    modifiers: egui::Modifiers,
) -> Option<Vec<u8>> {
    let mode = protocol.mode;
    let (button, pressed) = action;
    let col = (((pos.x - rect.min.x) / cell.x).floor().max(0.0) as usize).min(size.cols - 1) + 1;
    let row = (((pos.y - rect.min.y) / cell.y).floor().max(0.0) as usize).min(size.rows - 1) + 1;
    let modifiers =
        u8::from(modifiers.shift) * 4 + u8::from(modifiers.alt) * 8 + u8::from(modifiers.ctrl) * 16;
    if mode.contains(TermMode::SGR_MOUSE) {
        let suffix = if pressed { 'M' } else { 'm' };
        Some(format!("\x1b[<{};{col};{row}{suffix}", button + modifiers).into_bytes())
    } else if protocol.urxvt {
        let code = if pressed { button } else { 3 };
        Some(format!("\x1b[{};{col};{row}M", 32 + code + modifiers).into_bytes())
    } else {
        // The original X10 encoding cannot represent coordinates above 223.
        if col > 223 || row > 223 {
            return None;
        }
        let code = if pressed { button } else { 3 };
        Some(vec![
            0x1b,
            b'[',
            b'M',
            32 + code + modifiers,
            32 + col as u8,
            32 + row as u8,
        ])
    }
}

fn ime_owns_keyboard(preedit: &str, events: &[egui::Event]) -> bool {
    !preedit.is_empty()
        || events.iter().any(|event| {
            matches!(event,
                egui::Event::Ime(egui::ImeEvent::Preedit(text)) if !text.is_empty()
            ) || matches!(event, egui::Event::Ime(egui::ImeEvent::Commit(_)))
        })
}

fn is_paste_shortcut(key: Key, modifiers: egui::Modifiers) -> bool {
    (key == Key::V && (modifiers.mac_cmd || modifiers.ctrl && modifiers.shift))
        || (key == Key::Insert && modifiers.shift && !modifiers.ctrl && !modifiers.alt && !modifiers.mac_cmd)
}

fn key_bytes(key: Key, modifiers: egui::Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    if modifiers.ctrl && !modifiers.shift {
        let name = format!("{key:?}");
        if name.len() == 1 && name.as_bytes()[0].is_ascii_uppercase() {
            return Some(vec![name.as_bytes()[0] - b'A' + 1]);
        }
    }
    let arrow = if app_cursor { "\x1bO" } else { "\x1b[" };
    let text = match key {
        Key::Enter => "\r".into(),
        Key::Tab if modifiers.shift => "\x1b[Z".into(),
        Key::Tab => "\t".into(),
        Key::Backspace => "\x7f".into(),
        Key::Escape => "\x1b".into(),
        Key::ArrowUp => format!("{arrow}A"),
        Key::ArrowDown => format!("{arrow}B"),
        Key::ArrowRight => format!("{arrow}C"),
        Key::ArrowLeft => format!("{arrow}D"),
        Key::Home => "\x1b[H".into(),
        Key::End => "\x1b[F".into(),
        Key::Insert => "\x1b[2~".into(),
        Key::Delete => "\x1b[3~".into(),
        Key::PageUp => "\x1b[5~".into(),
        Key::PageDown => "\x1b[6~".into(),
        _ => return None,
    };
    Some(text.into_bytes())
}

fn main() -> Result<()> {
    if ssh::is_wrapper() {
        return ssh::run_wrapper();
    }
    let app = TerminalApp::new(Args::parse())?;
    eframe::run_native(
        "yiterm",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([960.0, 640.0]),
            ..Default::default()
        },
        Box::new(move |cc| {
            configure_fonts(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn configure_fonts(ctx: &egui::Context) {
    #[cfg(windows)]
    {
        let windows = std::env::var_os("WINDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
        let mut fonts = egui::FontDefinitions::default();
        // Append CJK fonts so ASCII keeps the built-in monospace metrics.
        // FontData uses face zero, including for Windows TTC collections.
        for filename in ["msyh.ttc", "simhei.ttf", "simsun.ttc"] {
            let Ok(data) = std::fs::read(windows.join("Fonts").join(filename)) else {
                continue;
            };
            fonts.font_data.insert(
                filename.into(),
                std::sync::Arc::new(egui::FontData::from_owned(data)),
            );
            for family in [egui::FontFamily::Monospace, egui::FontFamily::Proportional] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push(filename.into());
            }
        }
        ctx.set_fonts(fonts);
    }
    #[cfg(target_os = "macos")]
    {
        let mut fonts = egui::FontDefinitions::default();
        for path in [
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
        ] {
            let Ok(data) = std::fs::read(path) else {
                continue;
            };
            fonts.font_data.insert(
                path.into(),
                std::sync::Arc::new(egui::FontData::from_owned(data)),
            );
            for family in [egui::FontFamily::Monospace, egui::FontFamily::Proportional] {
                fonts.families.entry(family).or_default().push(path.into());
            }
        }
        ctx.set_fonts(fonts);
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = ctx;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mouse_reports_use_one_based_cells_and_preserve_release_button() {
        let rect = Rect::from_min_size(egui::pos2(10.0, 20.0), Vec2::new(800.0, 500.0));
        let cell = Vec2::new(10.0, 20.0);
        let size = Size { cols: 80, rows: 25 };
        let encode = |mode: TermMode, pos, button, pressed| {
            mouse_bytes(
                mode.into(),
                rect,
                cell,
                size,
                pos,
                (button, pressed),
                egui::Modifiers::NONE,
            )
        };
        assert_eq!(
            encode(TermMode::SGR_MOUSE, egui::pos2(25.0, 20.0), 0, true),
            Some(b"\x1b[<0;2;1M".to_vec())
        );
        assert_eq!(
            encode(TermMode::SGR_MOUSE, egui::pos2(25.0, 20.0), 0, false),
            Some(b"\x1b[<0;2;1m".to_vec())
        );
        assert_eq!(
            encode(TermMode::empty(), rect.min, 0, true),
            Some(vec![27, 91, 77, 32, 33, 33])
        );
        assert_eq!(
            encode(TermMode::empty(), rect.min, 0, false),
            Some(vec![27, 91, 77, 35, 33, 33])
        );
        assert_eq!(
            encode(TermMode::SGR_MOUSE, rect.max, 64, true),
            Some(b"\x1b[<64;80;25M".to_vec())
        );
    }
    #[test]
    fn urxvt_reports_preserve_large_coordinates_and_event_types() {
        let protocol = mouse::Protocol {
            mode: TermMode::MOUSE_DRAG,
            urxvt: true,
        };
        let rect = Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(4000.0, 500.0));
        let encode = |button, pressed| {
            mouse_bytes(
                protocol,
                rect,
                Vec2::new(10.0, 20.0),
                Size {
                    cols: 400,
                    rows: 25,
                },
                egui::pos2(2995.0, 10.0),
                (button, pressed),
                egui::Modifiers::NONE,
            )
            .unwrap()
        };
        assert_eq!(encode(0, true), b"\x1b[32;300;1M");
        assert_eq!(encode(0, false), b"\x1b[35;300;1M");
        assert_eq!(encode(32, true), b"\x1b[64;300;1M");
        assert_eq!(encode(64, true), b"\x1b[96;300;1M");
        assert_eq!(encode(65, true), b"\x1b[97;300;1M");
    }
    #[test]
    fn ime_reserves_composition_and_commit_keys() {
        assert!(!ime_owns_keyboard("", &[egui::Event::Text("a".into())]));
        assert!(ime_owns_keyboard("ni", &[]));
        assert!(ime_owns_keyboard(
            "",
            &[egui::Event::Ime(egui::ImeEvent::Preedit("ni".into()))]
        ));
        assert!(ime_owns_keyboard(
            "",
            &[
                egui::Event::Ime(egui::ImeEvent::Preedit(String::new())),
                egui::Event::Ime(egui::ImeEvent::Commit("你".into())),
            ]
        ));
        // Windows may leave IME enabled between compositions: ordinary keys
        // still belong to the shell when no preedit or commit is present.
        assert!(!ime_owns_keyboard(
            "",
            &[egui::Event::Ime(egui::ImeEvent::Enabled)]
        ));
    }
    #[test]
    fn terminal_parses_output_and_answers_queries() {
        let (tx, rx) = mpsc::channel();
        let mut term = Term::new(
            Config::default(),
            &Size { cols: 80, rows: 24 },
            Listener(tx),
        );
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"hello\x1b[6n");
        assert_eq!(
            term.grid()[alacritty_terminal::index::Line(0)][alacritty_terminal::index::Column(0)].c,
            'h'
        );
        assert!(
            rx.try_iter()
                .any(|e| matches!(e, Event::PtyWrite(text) if text == "\x1b[1;6R"))
        );
    }
    #[test]
    fn real_pty_runs_shell_and_receives_output() {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = if cfg!(windows) {
            let mut command = CommandBuilder::new("powershell.exe");
            command.args(["-NoProfile", "-Command", "Write-Output yiterm-pty-ok"]);
            command
        } else {
            let mut command = CommandBuilder::new("/bin/sh");
            command.args(["-c", "printf yiterm-pty-ok"]);
            command
        };
        command.env("TERM", "xterm-256color");
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut output = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => output.extend_from_slice(&buffer[..n]),
                }
                // ConPTY may keep its output pipe open after the child exits.
                if String::from_utf8_lossy(&output).contains("yiterm-pty-ok") {
                    break;
                }
            }
            let _ = tx.send(output);
        });
        let output = match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(output) => output,
            Err(error) => {
                let _ = child.kill();
                panic!("PTY output timed out: {error}");
            }
        };
        assert!(child.wait().unwrap().success());
        assert!(String::from_utf8_lossy(&output).contains("yiterm-pty-ok"));
    }
    #[test]
    fn keyboard_supports_control_and_application_cursor() {
        assert_eq!(
            key_bytes(Key::C, egui::Modifiers::CTRL, false),
            Some(vec![3])
        );
        assert_eq!(
            key_bytes(Key::ArrowUp, egui::Modifiers::NONE, true),
            Some(b"\x1bOA".to_vec())
        );
    }

    #[test]
    fn paste_shortcuts_include_shift_insert() {
        assert!(is_paste_shortcut(Key::Insert, egui::Modifiers::SHIFT));
        assert!(is_paste_shortcut(Key::V, egui::Modifiers::CTRL | egui::Modifiers::SHIFT));
        assert!(is_paste_shortcut(Key::V, egui::Modifiers::MAC_CMD));
        assert!(!is_paste_shortcut(Key::Insert, egui::Modifiers::NONE));
        assert!(!is_paste_shortcut(Key::Insert, egui::Modifiers::CTRL));
        assert!(!is_paste_shortcut(Key::Insert, egui::Modifiers::CTRL | egui::Modifiers::SHIFT));
        assert!(!is_paste_shortcut(Key::V, egui::Modifiers::CTRL));
        assert_eq!(key_bytes(Key::Insert, egui::Modifiers::NONE, false), Some(b"\x1b[2~".to_vec()));
    }
}
