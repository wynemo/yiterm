#![cfg_attr(windows, windows_subsystem = "windows")]

use yiterm::{logging, mouse, paste, ssh};

#[cfg(windows)]
mod conpty;

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

const TERMINAL_FONT_SIZE: f32 = 16.0;
const TERMINAL_LINE_HEIGHT: f32 = 1.3;

fn terminal_cell_rect(origin: egui::Pos2, cell: Vec2, column: usize, row: usize) -> Rect {
    // Compute both sides from grid coordinates. Adding cell height to the top
    // can round differently from the next row's top at fractional line heights.
    Rect::from_min_max(
        origin + Vec2::new(column as f32 * cell.x, row as f32 * cell.y),
        origin + Vec2::new((column + 1) as f32 * cell.x, (row + 1) as f32 * cell.y),
    )
}

#[derive(Parser, Clone)]
#[command(version, about = "A small native terminal with clipboard image paste")]
struct Args {
    /// Connect directly to this OpenSSH host (remote shell must be POSIX).
    #[arg(long)]
    ssh: Option<String>,
    /// Configuration TOML file.
    #[arg(long)]
    config: Option<std::path::PathBuf>,
    /// Override logging filter (e.g. warn,yiterm=debug).
    #[arg(long)]
    log_filter: Option<String>,
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
struct TerminalSession {
    title: String,
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
impl TerminalSession {
    fn new(args: &Args, warning: Option<&str>) -> Result<Self> {
        tracing::trace!(target: "yiterm::mouse",
            "build={} transport=raw_vt",
            option_env!("YITERM_BUILD_REV").unwrap_or("local")
        );
        tracing::info!(
            session_kind = if args.ssh.is_some() { "ssh" } else { "local" },
            "Starting terminal session"
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
            let shell = args.shell.clone().unwrap_or_else(|| {
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
                    Ok(0) => {
                        tracing::info!("PTY output closed");
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "PTY read failed");
                        break;
                    }
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
        tracing::info!(child_pid = ?child.process_id(), "Terminal session started");
        Ok(Self {
            title: args.ssh.clone().unwrap_or_else(|| "Terminal".into()),
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
            status: warning
                .map(str::to_owned)
                .unwrap_or_else(|| "Ready · paste: Cmd+V / Ctrl+Shift+V / Shift+Insert".into()),
            size,
        })
    }
    fn write(&mut self, bytes: &[u8]) {
        if let Err(error) = self
            .writer
            .write_all(bytes)
            .and_then(|_| self.writer.flush())
        {
            tracing::error!(%error, "PTY write failed");
            self.status = error.to_string();
        }
    }
    fn write_mouse(&mut self, report: &[u8]) {
        tracing::trace!(target: "yiterm::mouse",
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
            Ok(()) => tracing::trace!(target: "yiterm::mouse", "write_ok"),
            Err(error) => {
                tracing::error!(%error, "PTY mouse write failed");
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
            let started = Instant::now();
            tracing::debug!("Clipboard paste started");
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
            tracing::debug!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                success = result.is_ok(),
                "Clipboard paste completed"
            );
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
impl Drop for TerminalSession {
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

impl TerminalSession {
    fn poll(&mut self) -> bool {
        for _ in 0..128 {
            let Ok(bytes) = self.output.try_recv() else {
                break;
            };
            let previous_mode = *self.term.mode();
            let requests = self.mouse_modes.observe(&bytes);
            self.parser.advance(&mut self.term, &bytes);
            for (mode, enabled) in requests {
                tracing::trace!(target: "yiterm::mouse", "request mode={mode} enabled={enabled}");
            }
            if previous_mode != *self.term.mode() {
                tracing::trace!(target: "yiterm::mouse", "state {:?}", self.term.mode());
            }
            self.cursor_blink_started = Instant::now();
        }
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::PtyWrite(text) => self.write(text.as_bytes()),
                Event::Title(title) => self.title = title,
                Event::ResetTitle => self.title = "Terminal".into(),
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
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "Clipboard paste failed");
                    self.status = format!("{error:#}");
                }
            }
        }
        if let Ok(Some(status)) = self.child.try_wait() {
            tracing::info!(?status, "Terminal session exited");
            return false;
        }
        true
    }

    fn show(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, focused: bool) {
        // Keep the cursor visible while typing and restart its blink on focus.
        if focused
            && ctx.input(|input| {
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
            })
        {
            self.cursor_blink_started = Instant::now();
        }
        let rect = ui.available_rect_before_wrap();
        let font = FontId::monospace(TERMINAL_FONT_SIZE);
        let cell = Vec2::new(
            ui.fonts_mut(|f| f.glyph_width(&font, 'M')),
            TERMINAL_FONT_SIZE * TERMINAL_LINE_HEIGHT,
        );
        let size = Size {
            cols: ((rect.width() / cell.x) as usize).max(2),
            rows: ((rect.height() / cell.y) as usize).max(1),
        };
        if size.cols != self.size.cols || size.rows != self.size.rows {
            if let Err(error) = self.master.resize(PtySize {
                cols: size.cols as u16,
                rows: size.rows as u16,
                pixel_width: 0,
                pixel_height: 0,
            }) {
                tracing::warn!(%error, "PTY resize failed");
                self.status = error.to_string();
            }
            self.term.resize(size);
            self.size = size;
        }
        let hovered = ctx.input(|i| i.pointer.hover_pos().is_some_and(|pos| rect.contains(pos)));
        let scroll = if hovered {
            ctx.input(|i| i.smooth_scroll_delta.y)
        } else {
            0.0
        };
        let mouse_protocol = self.mouse_modes.protocol(*self.term.mode());
        let mouse_mode = mouse_protocol.mode;
        let report_mouse = mouse_mode.intersects(TermMode::MOUSE_MODE)
            && !ctx.input(|i| i.modifiers.shift)
            && self.term.grid().display_offset() == 0;
        if !report_mouse {
            self.mouse_wheel.reset();
        }
        if scroll != 0.0 && !report_mouse {
            self.term
                .scroll_display(Scroll::Delta((scroll / cell.y).round() as i32));
        }
        let content = self.term.renderable_content();
        let offset = content.display_offset;
        let cells: Vec<_> = content.display_iter.collect();
        // Paint all backgrounds first: a wide glyph extends into its spacer
        // cell, whose background would otherwise erase the glyph's right half.
        for indexed in &cells {
            let row = indexed.point.line.0 + offset as i32;
            if row < 0 {
                continue;
            }
            let bg = if indexed.cell.flags.contains(Flags::INVERSE) {
                color(indexed.cell.fg, true)
            } else {
                color(indexed.cell.bg, false)
            };
            ui.painter().rect_filled(
                terminal_cell_rect(rect.min, cell, indexed.point.column.0, row as usize),
                0.0,
                bg,
            );
        }
        for indexed in &cells {
            let row = indexed.point.line.0 + offset as i32;
            if row < 0 {
                continue;
            }
            let pos =
                rect.min + Vec2::new(indexed.point.column.0 as f32 * cell.x, row as f32 * cell.y);
            let fg = if indexed.cell.flags.contains(Flags::INVERSE) {
                color(indexed.cell.bg, false)
            } else {
                color(indexed.cell.fg, true)
            };
            if !indexed.cell.flags.intersects(
                Flags::HIDDEN | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER,
            ) {
                let mut text = indexed.cell.c.to_string();
                if let Some(extra) = indexed.cell.zerowidth() {
                    text.extend(extra);
                }
                let galley = ui.painter().layout_no_wrap(text, font.clone(), fg);
                // Center the font's line box in the terminal row, as GPUI does.
                let text_pos = pos + Vec2::new(0.0, (cell.y - galley.size().y) / 2.0);
                ui.painter().galley(text_pos, galley, fg);
                if indexed.cell.flags.contains(Flags::UNDERLINE) {
                    let width = if indexed.cell.flags.contains(Flags::WIDE_CHAR) {
                        2.0 * cell.x
                    } else {
                        cell.x
                    };
                    ui.painter().line_segment(
                        [
                            pos + Vec2::new(0.0, cell.y - 2.0),
                            pos + Vec2::new(width, cell.y - 2.0),
                        ],
                        (1.0, fg),
                    );
                }
            }
        }
        let blink_visible = !ctx.input(|input| input.focused)
            || !self.term.cursor_style().blinking
            || self.cursor_blink_started.elapsed().as_millis() % 1000 < 500;
        if focused
            && offset == 0
            && self.term.mode().contains(TermMode::SHOW_CURSOR)
            && blink_visible
        {
            let point = content.cursor.point;
            let pos =
                rect.min + Vec2::new(point.column.0 as f32 * cell.x, point.line.0 as f32 * cell.y);
            ui.painter().rect_stroke(
                Rect::from_min_size(pos, cell),
                0.0,
                (1.0, Color32::LIGHT_GRAY),
                egui::StrokeKind::Inside,
            );
        }
        let point = content.cursor.point;
        let cursor_pos =
            rect.min + Vec2::new(point.column.0 as f32 * cell.x, point.line.0 as f32 * cell.y);
        let cursor_rect = Rect::from_min_size(cursor_pos, cell);
        if focused {
            ctx.output_mut(|output| {
                output.ime = Some(egui::output::IMEOutput {
                    rect: cursor_rect,
                    cursor_rect,
                });
            });
        }
        let events = if focused && !egui::Popup::is_any_open(ctx) {
            ctx.input(|i| i.events.clone())
        } else {
            Vec::new()
        };
        // Consume candidate-selection keys even if Commit clears the preedit
        // earlier in this frame. They must not execute a shell command.
        let composing = ime_owns_keyboard(&self.ime_preedit, &events);
        let paste_shortcut = events.iter().any(|event| matches!(event, egui::Event::Key { key, pressed: true, modifiers, .. } if is_paste_shortcut(*key, *modifiers)));
        if paste_shortcut {
            self.paste();
        }
        for event in events {
            match event {
                egui::Event::MouseWheel {
                    unit,
                    delta,
                    modifiers,
                } => {
                    tracing::trace!(target: "yiterm::mouse", "wheel unit={unit:?} delta={} report={report_mouse}", delta.y);
                    if !report_mouse || modifiers.shift || delta.y == 0.0 {
                        continue;
                    }
                    let Some(pos) = ctx
                        .input(|i| i.pointer.hover_pos())
                        .filter(|pos| rect.contains(*pos))
                    else {
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
                        if let Some(bytes) = mouse_bytes(
                            mouse_protocol,
                            rect,
                            cell,
                            size,
                            pos,
                            (button, true),
                            modifiers,
                        ) {
                            self.write_mouse(&bytes);
                        }
                    }
                }
                egui::Event::PointerButton {
                    button: egui::PointerButton::Secondary,
                    ..
                } => {}
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers,
                } => {
                    let tracked = self.mouse_button == Some(button);
                    tracing::trace!(target: "yiterm::mouse", "button={button:?} pressed={pressed} report={report_mouse} inside={} shift={} offset={} pos={pos:?} rect={rect:?} cell={cell:?} mode={mouse_mode:?}", rect.contains(pos), modifiers.shift, self.term.grid().display_offset());
                    if report_mouse
                        && (rect.contains(pos) || (!pressed && tracked))
                        && let Some(code) = mouse_button_code(button)
                    {
                        if let Some(bytes) = mouse_bytes(
                            mouse_protocol,
                            rect,
                            cell,
                            size,
                            pos,
                            (code, pressed),
                            modifiers,
                        ) {
                            self.write_mouse(&bytes);
                        }
                        if pressed {
                            self.mouse_button = Some(button);
                        }
                    }
                    if !pressed && tracked {
                        self.mouse_button = None;
                    }
                }
                egui::Event::PointerMoved(pos) if report_mouse => {
                    if rect.contains(pos)
                        && (mouse_mode.contains(TermMode::MOUSE_MOTION)
                            || (mouse_mode.contains(TermMode::MOUSE_DRAG)
                                && self.mouse_button.is_some()))
                    {
                        let code = self.mouse_button.and_then(mouse_button_code).unwrap_or(3) + 32;
                        if let Some(bytes) = mouse_bytes(
                            mouse_protocol,
                            rect,
                            cell,
                            size,
                            pos,
                            (code, true),
                            ctx.input(|i| i.modifiers),
                        ) {
                            self.write(&bytes);
                        }
                    }
                }
                egui::Event::Paste(_) if paste_shortcut => {}
                egui::Event::Paste(_) => self.paste(),
                egui::Event::Ime(egui::ImeEvent::Preedit(text)) => {
                    self.ime_preedit = text;
                }
                egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                    self.ime_preedit.clear();
                    self.term.scroll_display(Scroll::Bottom);
                    self.write(text.as_bytes());
                }
                egui::Event::Ime(egui::ImeEvent::Disabled) | egui::Event::WindowFocused(false) => {
                    self.ime_preedit.clear();
                }
                egui::Event::Text(_) if composing => {}
                egui::Event::Text(text) => {
                    self.term.scroll_display(Scroll::Bottom);
                    self.write(text.as_bytes());
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if composing {
                        continue;
                    }
                    if is_paste_shortcut(key, modifiers) {
                        continue;
                    }
                    let app_cursor = self.term.mode().contains(TermMode::APP_CURSOR);
                    if let Some(bytes) = key_bytes(key, modifiers, app_cursor) {
                        self.term.scroll_display(Scroll::Bottom);
                        self.write(&bytes);
                    }
                }
                _ => {}
            }
        }
        if !self.ime_preedit.is_empty() {
            let galley =
                ui.painter()
                    .layout_no_wrap(self.ime_preedit.clone(), font.clone(), Color32::WHITE);
            ui.painter().rect_filled(
                Rect::from_min_size(cursor_pos, galley.size()),
                0.0,
                Color32::from_rgb(24, 26, 32),
            );
            ui.painter().galley(cursor_pos, galley, Color32::WHITE);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum SplitDirection {
    Right,
    Down,
}

#[derive(Debug, PartialEq)]
enum PaneLayout {
    Leaf(usize),
    Split {
        direction: SplitDirection,
        ratio: f32,
        first: Box<PaneLayout>,
        second: Box<PaneLayout>,
    },
}

impl PaneLayout {
    fn split(&mut self, pane: usize, new_pane: usize, direction: SplitDirection) {
        match self {
            Self::Leaf(index) if *index == pane => {
                *self = Self::Split {
                    direction,
                    ratio: 0.5,
                    first: Box::new(Self::Leaf(pane)),
                    second: Box::new(Self::Leaf(new_pane)),
                };
            }
            Self::Split { first, second, .. } => {
                first.split(pane, new_pane, direction);
                second.split(pane, new_pane, direction);
            }
            _ => {}
        }
    }

    // Collapse the removed pane's split and renumber the surviving leaves.
    fn remove(self, pane: usize) -> Option<Self> {
        match self {
            Self::Leaf(index) if index == pane => None,
            Self::Leaf(index) => Some(Self::Leaf(index - usize::from(index > pane))),
            Self::Split {
                direction,
                ratio,
                first,
                second,
            } => match (first.remove(pane), second.remove(pane)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    direction,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (remaining, None) | (None, remaining) => remaining,
            },
        }
    }

    fn rectangles(
        &mut self,
        ui: &egui::Ui,
        rect: Rect,
        path: &str,
        panes: &mut Vec<(usize, Rect)>,
    ) {
        match self {
            Self::Leaf(index) => panes.push((*index, rect)),
            Self::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let horizontal = *direction == SplitDirection::Right;
                let length = if horizontal {
                    rect.width()
                } else {
                    rect.height()
                };
                let available = (length - 5.0).max(0.0);
                let minimum = 80.0_f32.min(available / 2.0);
                let mut offset = (available * *ratio).clamp(minimum, available - minimum);
                let divider = if horizontal {
                    Rect::from_min_max(
                        egui::pos2(rect.left() + offset, rect.top()),
                        egui::pos2(rect.left() + offset + 5.0, rect.bottom()),
                    )
                } else {
                    Rect::from_min_max(
                        egui::pos2(rect.left(), rect.top() + offset),
                        egui::pos2(rect.right(), rect.top() + offset + 5.0),
                    )
                };
                let response = ui
                    .interact(divider, ui.id().with(path), egui::Sense::drag())
                    .on_hover_cursor(if horizontal {
                        egui::CursorIcon::ResizeHorizontal
                    } else {
                        egui::CursorIcon::ResizeVertical
                    });
                if response.dragged() && available > 0.0 {
                    offset = (offset
                        + if horizontal {
                            response.drag_delta().x
                        } else {
                            response.drag_delta().y
                        })
                    .clamp(minimum, available - minimum);
                    *ratio = offset / available;
                }
                ui.painter().rect_filled(
                    divider,
                    0.0,
                    if response.hovered() || response.dragged() {
                        Color32::GRAY
                    } else {
                        Color32::from_gray(55)
                    },
                );
                let (first_rect, second_rect) = if horizontal {
                    (
                        Rect::from_min_max(
                            rect.min,
                            egui::pos2(rect.left() + offset, rect.bottom()),
                        ),
                        Rect::from_min_max(
                            egui::pos2(rect.left() + offset + 5.0, rect.top()),
                            rect.max,
                        ),
                    )
                } else {
                    (
                        Rect::from_min_max(rect.min, egui::pos2(rect.right(), rect.top() + offset)),
                        Rect::from_min_max(
                            egui::pos2(rect.left(), rect.top() + offset + 5.0),
                            rect.max,
                        ),
                    )
                };
                first.rectangles(ui, first_rect, &format!("{path}/0"), panes);
                second.rectangles(ui, second_rect, &format!("{path}/1"), panes);
            }
        }
    }
}

struct TerminalTab {
    panes: Vec<TerminalSession>,
    layout: PaneLayout,
    active: usize,
}

impl TerminalTab {
    fn new(session: TerminalSession) -> Self {
        Self {
            panes: vec![session],
            layout: PaneLayout::Leaf(0),
            active: 0,
        }
    }

    fn focused(&self) -> &TerminalSession {
        &self.panes[self.active]
    }
    fn focused_mut(&mut self) -> &mut TerminalSession {
        &mut self.panes[self.active]
    }

    fn select(&mut self, pane: usize) {
        let previous = self.focused_mut();
        previous.ime_preedit.clear();
        previous.mouse_button = None;
        previous.mouse_wheel.reset();
        self.active = pane;
        self.focused_mut().cursor_blink_started = Instant::now();
    }

    fn split(&mut self, session: TerminalSession, direction: SplitDirection) {
        let index = self.panes.len();
        self.panes.push(session);
        self.layout.split(self.active, index, direction);
        self.select(index);
    }

    fn close_pane(&mut self, index: usize) {
        let was_active = self.active == index;
        self.panes.remove(index);
        let layout = std::mem::replace(&mut self.layout, PaneLayout::Leaf(0));
        self.layout = layout.remove(index).unwrap_or(PaneLayout::Leaf(0));
        self.active = active_after_close(self.active, index, self.panes.len());
        if was_active && !self.panes.is_empty() {
            self.select(self.active);
        }
    }

    fn poll(&mut self) -> bool {
        for index in (0..self.panes.len()).rev() {
            if !self.panes[index].poll() {
                self.close_pane(index);
            }
        }
        !self.panes.is_empty()
    }

    fn show(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) -> Option<(usize, SplitDirection)> {
        let mut rectangles = Vec::new();
        self.layout.rectangles(
            ui,
            ui.available_rect_before_wrap(),
            "split",
            &mut rectangles,
        );
        let mut close = None;
        let mut split = None;
        // Resolve focus before delivering this frame's keyboard and clipboard events.
        for &(index, rect) in &rectangles {
            if ctx
                .input(|input| input.pointer.primary_pressed() || input.pointer.secondary_pressed())
                && ui.rect_contains_pointer(rect)
            {
                self.select(index);
            }
            let response = ui.interact(
                rect,
                ui.id().with(("pane-menu", index)),
                egui::Sense::click(),
            );
            response.context_menu(|ui| {
                if ui.button("Split Right").clicked() {
                    split = Some((index, SplitDirection::Right));
                    ui.close();
                }
                if ui.button("Split Down").clicked() {
                    split = Some((index, SplitDirection::Down));
                    ui.close();
                }
            });
            let header = Rect::from_min_size(rect.min, Vec2::new(rect.width(), 24.0));
            let mut header_ui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("pane", index))
                    .max_rect(header),
            );
            header_ui.set_clip_rect(header.intersect(ui.clip_rect()));
            header_ui.horizontal(|ui| {
                if ui.small_button("×").on_hover_text("Close pane").clicked() {
                    close = Some(index);
                }
                ui.add(egui::Label::new(&self.panes[index].title).truncate())
                    .on_hover_text(&self.panes[index].title);
            });
        }
        for (index, rect) in rectangles {
            let terminal_rect = Rect::from_min_max(
                egui::pos2(rect.left(), (rect.top() + 24.0).min(rect.bottom())),
                rect.max,
            )
            .shrink(2.0);
            let mut pane_ui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("terminal", index))
                    .max_rect(terminal_rect),
            );
            pane_ui.set_clip_rect(terminal_rect.intersect(ui.clip_rect()));
            self.panes[index].show(ctx, &mut pane_ui, index == self.active);
            ui.painter().rect_stroke(
                rect.shrink(0.5),
                0.0,
                (
                    1.0,
                    if index == self.active {
                        Color32::from_rgb(97, 175, 239)
                    } else {
                        Color32::from_gray(55)
                    },
                ),
                egui::StrokeKind::Inside,
            );
        }
        if let Some(index) = close {
            self.close_pane(index);
            return None;
        }
        split
    }
}

struct TerminalApp {
    tabs: Vec<TerminalTab>,
    active: usize,
    args: Args,
    log_directory: std::path::PathBuf,
    warning: Option<String>,
    error: Option<String>,
}

impl TerminalApp {
    fn new(args: Args, logging: &logging::Logging) -> Result<Self> {
        let session = TerminalSession::new(&args, logging.warning.as_deref())?;
        Ok(Self {
            tabs: vec![TerminalTab::new(session)],
            active: 0,
            args,
            log_directory: logging.directory.clone(),
            warning: logging.warning.clone(),
            error: None,
        })
    }

    fn new_tab(&mut self) {
        match TerminalSession::new(&self.args, self.warning.as_deref()) {
            Ok(session) => {
                self.tabs.push(TerminalTab::new(session));
                self.select(self.tabs.len() - 1);
                self.error = None;
            }
            Err(error) => {
                tracing::warn!(%error, "Cannot create terminal tab");
                self.error = Some(format!("Cannot create tab: {error:#}"));
            }
        }
    }

    fn split_pane(&mut self, direction: SplitDirection) {
        match TerminalSession::new(&self.args, self.warning.as_deref()) {
            Ok(session) => {
                self.tabs[self.active].split(session, direction);
                self.error = None;
            }
            Err(error) => self.error = Some(format!("Cannot split pane: {error:#}")),
        }
    }

    fn select(&mut self, index: usize) {
        if let Some(previous) = self.tabs.get_mut(self.active) {
            previous.focused_mut().ime_preedit.clear();
            previous.focused_mut().mouse_button = None;
            previous.focused_mut().mouse_wheel.reset();
        }
        self.active = index;
        self.tabs[index].focused_mut().cursor_blink_started = Instant::now();
    }

    fn close_tab(&mut self, index: usize) {
        let was_active = index == self.active;
        self.tabs.remove(index);
        self.active = active_after_close(self.active, index, self.tabs.len());
        if was_active && !self.tabs.is_empty() {
            self.select(self.active);
        }
    }
}

fn active_after_close(active: usize, closed: usize, remaining: usize) -> usize {
    let active = if closed < active { active - 1 } else { active };
    active.min(remaining.saturating_sub(1))
}

fn consume_tab_shortcuts(input: &mut egui::InputState) -> (bool, bool, bool, bool) {
    let command = if cfg!(target_os = "macos") {
        egui::Modifiers::MAC_CMD
    } else {
        egui::Modifiers::CTRL | egui::Modifiers::SHIFT
    };
    // egui allows extra Shift when matching Ctrl: consume the specific shortcut first.
    let previous = input.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, Key::Tab);
    let next = input.consume_key(egui::Modifiers::CTRL, Key::Tab);
    (
        input.consume_key(command, Key::T),
        input.consume_key(command, Key::W),
        next,
        previous,
    )
}

fn reserve_terminal_keyboard(ctx: &egui::Context) {
    if !egui::Popup::is_any_open(ctx) {
        // egui resolves Tab/arrow navigation before update. The terminal owns
        // these keys, so cancel navigation before any menu buttons are rendered.
        ctx.memory_mut(|memory| {
            memory.move_focus(egui::FocusDirection::None);
            if let Some(id) = memory.focused() {
                memory.surrender_focus(id);
            }
        });
    }
}

impl eframe::App for TerminalApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        reserve_terminal_keyboard(ctx);
        // Drain every PTY, including background tabs, so output and paste results
        // cannot block and terminal queries are answered while another tab is active.
        for index in (0..self.tabs.len()).rev() {
            if !self.tabs[index].poll() {
                self.close_tab(index);
            }
        }
        if self.tabs.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let composing = ctx.input(|input| {
            ime_owns_keyboard(&self.tabs[self.active].focused().ime_preedit, &input.events)
        });
        if !composing {
            let (new, close, next, previous) = ctx.input_mut(consume_tab_shortcuts);
            if new {
                self.new_tab();
            }
            if close {
                self.close_tab(self.active);
            }
            if !self.tabs.is_empty() && (next || previous) {
                let count = self.tabs.len();
                self.select((self.active + if previous { count - 1 } else { 1 }) % count);
            }
        }
        let mut selected = None;
        let mut closed = None;
        let mut new = false;
        let mut split = None;
        let mut close_pane = false;
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("Pane", |ui| {
                    if ui.button("Split Right").clicked() {
                        split = Some(SplitDirection::Right);
                        ui.close();
                    }
                    if ui.button("Split Down").clicked() {
                        split = Some(SplitDirection::Down);
                        ui.close();
                    }
                    if ui.button("Close Pane").clicked() {
                        close_pane = true;
                        ui.close();
                    }
                });
                ui.menu_button("Tools", |ui| {
                    if ui.button("Open Log Directory").clicked() {
                        if let Err(error) = logging::open_directory(&self.log_directory) {
                            self.error = Some(format!("{error:#}"));
                        }
                        ui.close();
                    }
                });
            });
            egui::ScrollArea::horizontal()
                .id_salt("tabs")
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for (index, session) in self.tabs.iter().enumerate() {
                            ui.push_id(index, |ui| {
                                if ui
                                    .add_sized(
                                        [180.0, ui.spacing().interact_size.y],
                                        egui::Button::selectable(
                                            index == self.active,
                                            format!("{}: {}", index + 1, session.focused().title),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(&session.focused().title)
                                    .clicked()
                                {
                                    selected = Some(index);
                                }
                                if ui.small_button("×").on_hover_text("Close tab").clicked() {
                                    closed = Some(index);
                                }
                            });
                        }
                        new = ui.button("+").on_hover_text("New tab").clicked();
                    });
                });
            if let Some(error) = &self.error {
                ui.colored_label(Color32::LIGHT_RED, error);
            }
        });
        if let Some(index) = selected {
            self.select(index);
        }
        if let Some(index) = closed {
            self.close_tab(index);
        }
        if new {
            self.new_tab();
        }
        if self.tabs.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
            "yiterm — {}",
            self.tabs[self.active].focused().title
        )));
        if let Some(direction) = split {
            self.split_pane(direction);
        }
        if close_pane {
            let tab = &mut self.tabs[self.active];
            tab.close_pane(tab.active);
        }
        if self.tabs[self.active].panes.is_empty() {
            self.close_tab(self.active);
            if self.tabs.is_empty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
        }
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            let session = self.tabs[self.active].focused_mut();
            ui.horizontal(|ui| {
                if ui.button("Paste").clicked() {
                    session.paste();
                }
                ui.label(&session.status);
            });
        });
        let mut pane_split = None;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(24, 26, 32)))
            .show(ctx, |ui| {
                pane_split = self.tabs[self.active].show(ctx, ui);
            });
        if let Some((pane, direction)) = pane_split {
            self.tabs[self.active].select(pane);
            self.split_pane(direction);
        }
        // A pane can be closed from its header during rendering.
        if self.tabs[self.active].panes.is_empty() {
            self.close_tab(self.active);
            if self.tabs.is_empty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
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
        || (key == Key::Insert
            && modifiers.shift
            && !modifiers.ctrl
            && !modifiers.alt
            && !modifiers.mac_cmd)
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
    let args = Args::parse();
    let logging = logging::init(args.config.as_deref(), args.log_filter.as_deref())?;
    let result = run(args, &logging);
    if let Err(error) = &result {
        tracing::error!(error = %format!("{error:#}"), "Application failed");
    }
    result
}

fn run(args: Args, logging: &logging::Logging) -> Result<()> {
    let app = TerminalApp::new(args, logging)?;
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
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "Lilex".into(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/lilex/Lilex-Regular.ttf"
        ))),
    );
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "Lilex".into());
    #[cfg(windows)]
    {
        let windows = std::env::var_os("WINDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
        // Append fallback fonts so ASCII and box drawing keep Lilex's metrics.
        // Remote macOS apps use symbols such as ⌃/⌥/⌘ for keyboard shortcuts;
        // the built-in and CJK fonts do not cover all of these glyphs.
        // FontData uses face zero, including for Windows TTC collections.
        for filename in ["seguisym.ttf", "msyh.ttc", "simhei.ttf", "simsun.ttc"] {
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
    }
    #[cfg(target_os = "macos")]
    {
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
    }
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_background_cells_share_pixel_edges() {
        use egui::emath::GuiRounding;

        let cell = Vec2::new(9.6, TERMINAL_FONT_SIZE * TERMINAL_LINE_HEIGHT);
        for scale in [1.0, 1.25, 1.5, 2.0] {
            for origin in [egui::pos2(2.0, 26.5), egui::pos2(7.25, 63.0)] {
                for row in 0..100 {
                    for column in 0..100 {
                        let rect =
                            terminal_cell_rect(origin, cell, column, row).round_to_pixels(scale);
                        let below = terminal_cell_rect(origin, cell, column, row + 1)
                            .round_to_pixels(scale);
                        let right = terminal_cell_rect(origin, cell, column + 1, row)
                            .round_to_pixels(scale);
                        assert_eq!(rect.max.y, below.min.y, "background gap at row {row}");
                        assert_eq!(rect.max.x, right.min.x, "background gap at column {column}");
                    }
                }
            }
        }
    }

    #[test]
    fn embedded_terminal_font_keeps_box_drawing_on_the_cell_grid() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let ctx = egui::Context::default();
            configure_fonts(&ctx);
            ctx.set_pixels_per_point(scale);
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                ctx.fonts_mut(|fonts| {
                    let font = FontId::monospace(TERMINAL_FONT_SIZE);
                    let width = fonts.glyph_width(&font, 'M');
                    // Lilex has a 600/1000 em advance. Check the resolved font,
                    // not just registration: falling back to Hack changes this.
                    assert!((width - 9.6).abs() < 0.01);
                    for ch in "─│┌┐└┘├┤┬┴┼".chars() {
                        assert!((fonts.glyph_width(&font, ch) - width).abs() < 0.01);
                    }
                    let height = fonts.row_height(&font);
                    assert!(
                        (height - TERMINAL_FONT_SIZE * TERMINAL_LINE_HEIGHT).abs() < 0.1,
                        "Lilex line box {height} does not match terminal rows at scale {scale}"
                    );
                });
            });
        }
    }

    #[test]
    fn popup_clicks_do_not_activate_the_pane_underneath() {
        let ctx = egui::Context::default();
        let pos = egui::pos2(100.0, 100.0);
        for pressed in [false, true] {
            let mut events = vec![egui::Event::PointerMoved(pos)];
            if pressed {
                events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                });
            }
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(800.0, 600.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::Area::new(egui::Id::new("pane-menu"))
                        .order(egui::Order::Foreground)
                        .fixed_pos(egui::pos2(50.0, 50.0))
                        .show(ctx, |ui| {
                            ui.allocate_space(Vec2::splat(200.0));
                        });
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let rect = ui.available_rect_before_wrap();
                        assert!(rect.contains(pos));
                        if pressed {
                            assert!(ctx.input(|input| input.pointer.primary_pressed()));
                            assert!(
                                !ui.rect_contains_pointer(rect),
                                "Menu click reached underlying pane"
                            );
                        }
                    });
                },
            );
        }
    }

    #[test]
    fn nested_splits_have_disjoint_rectangles_and_collapse_after_close() {
        let mut layout = PaneLayout::Leaf(0);
        layout.split(0, 1, SplitDirection::Right);
        layout.split(1, 2, SplitDirection::Down);
        let ctx = egui::Context::default();
        let mut rectangles = Vec::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                layout.rectangles(
                    ui,
                    Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(805.0, 605.0)),
                    "test",
                    &mut rectangles,
                );
            });
        });
        assert_eq!(rectangles.len(), 3);
        assert_eq!(rectangles[0].1.width(), 400.0);
        assert_eq!(rectangles[1].1.height(), 300.0);
        assert_eq!(rectangles[2].1.height(), 300.0);
        assert!(rectangles[0].1.right() < rectangles[1].1.left());
        assert!(rectangles[1].1.bottom() < rectangles[2].1.top());
        let layout = layout.remove(1).unwrap();
        assert_eq!(
            layout,
            PaneLayout::Split {
                direction: SplitDirection::Right,
                ratio: 0.5,
                first: Box::new(PaneLayout::Leaf(0)),
                second: Box::new(PaneLayout::Leaf(1)),
            }
        );
        assert_eq!(layout.remove(0), Some(PaneLayout::Leaf(0)));
        assert_eq!(PaneLayout::Leaf(0).remove(0), None);
    }

    #[cfg(unix)]
    #[test]
    fn split_panes_route_keyboard_only_to_clicked_pane_and_preserve_survivors() {
        let args = Args {
            ssh: None,
            config: None,
            log_filter: None,
            shell: Some("/bin/sh".into()),
        };
        let mut tab = TerminalTab::new(TerminalSession::new(&args, None).unwrap());
        tab.split(
            TerminalSession::new(&args, None).unwrap(),
            SplitDirection::Right,
        );
        tab.split(
            TerminalSession::new(&args, None).unwrap(),
            SplitDirection::Down,
        );
        assert_eq!(tab.active, 2);
        let ctx = egui::Context::default();
        let pos = egui::pos2(100.0, 100.0);
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(805.0, 605.0),
            )),
            events: vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::Text("printf '\\033]0;focused-pane\\007'".into()),
                egui::Event::Key {
                    key: Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| tab.show(ctx, ui));
        });
        assert_eq!(tab.active, 0);
        assert!(tab.panes[0].size.cols < 80);
        assert!(tab.panes[1].size.rows < tab.panes[0].size.rows);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(tab.poll());
            if tab.panes[0].title == "focused-pane" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Focused pane did not receive keyboard input"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(tab.panes[1].title, "focused-pane");
        assert_ne!(tab.panes[2].title, "focused-pane");
        // Exiting one shell removes only its pane and collapses its parent split.
        tab.panes[1].write(b"exit\n");
        loop {
            assert!(tab.poll());
            if tab.panes.len() == 2 {
                break;
            }
            assert!(Instant::now() < deadline, "Exited pane was not removed");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(tab.active, 0);
        assert_eq!(tab.panes[0].title, "focused-pane");
        tab.close_pane(0);
        assert_eq!(tab.layout, PaneLayout::Leaf(0));
        assert!(tab.focused_mut().child.try_wait().unwrap().is_none());
    }

    #[test]
    fn closing_tabs_preserves_selection_or_selects_a_neighbor() {
        assert_eq!(active_after_close(2, 0, 3), 1);
        assert_eq!(active_after_close(1, 3, 3), 1);
        assert_eq!(active_after_close(1, 1, 3), 1);
        assert_eq!(active_after_close(3, 3, 3), 2);
        assert_eq!(active_after_close(0, 0, 0), 0);
    }

    #[test]
    fn tab_shortcuts_are_consumed_without_reversing_previous_tab() {
        for (modifiers, expected) in [
            (egui::Modifiers::CTRL, (false, false, true, false)),
            (
                egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
                (false, false, false, true),
            ),
            (egui::Modifiers::NONE, (false, false, false, false)),
        ] {
            let mut input = egui::InputState::default();
            input.events.push(egui::Event::Key {
                key: Key::Tab,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            });
            assert_eq!(consume_tab_shortcuts(&mut input), expected);
            assert_eq!(
                input.events.len(),
                usize::from(modifiers == egui::Modifiers::NONE)
            );
        }
    }

    #[test]
    fn terminal_keys_do_not_move_focus_to_menu_buttons() {
        let ctx = egui::Context::default();
        for (key, modifiers, bytes) in [
            (Key::Tab, egui::Modifiers::NONE, "\t"),
            (Key::Tab, egui::Modifiers::SHIFT, "\x1b[Z"),
            (Key::ArrowDown, egui::Modifiers::NONE, "\x1b[B"),
        ] {
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers,
                    }],
                    ..Default::default()
                },
                |ctx| {
                    reserve_terminal_keyboard(ctx);
                    egui::TopBottomPanel::top("menu").show(ctx, |ui| {
                        ui.menu_button("Pane", |_| {});
                        let _ = ui.button("+");
                    });
                    assert_eq!(ctx.memory(|memory| memory.focused()), None);
                    assert!(ctx.input(|input| input.key_pressed(key)));
                    assert_eq!(
                        key_bytes(key, modifiers, false),
                        Some(bytes.as_bytes().to_vec())
                    );
                },
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn background_sessions_process_output_and_survive_another_tab_closing() {
        let args = Args {
            ssh: None,
            config: None,
            log_filter: None,
            shell: Some("/bin/sh".into()),
        };
        let first = TerminalSession::new(&args, None).unwrap();
        let second = TerminalSession::new(&args, None).unwrap();
        let mut app = TerminalApp {
            tabs: vec![TerminalTab::new(first), TerminalTab::new(second)],
            active: 0,
            args,
            log_directory: std::env::temp_dir(),
            warning: None,
            error: None,
        };
        // Set different titles through each real PTY; the background session
        // must parse output too, without changing the foreground session title.
        app.tabs[0]
            .focused_mut()
            .write(b"printf '\\033]0;first-session\\007'\n");
        app.tabs[1]
            .focused_mut()
            .write(b"printf '\\033]0;background-session\\007'\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(app.tabs[0].poll());
            assert!(app.tabs[1].poll());
            if app.tabs[0].focused().title == "first-session"
                && app.tabs[1].focused().title == "background-session"
            {
                break;
            }
            assert!(Instant::now() < deadline, "PTY titles were not processed");
            std::thread::sleep(Duration::from_millis(10));
        }
        app.close_tab(0);
        assert_eq!(app.active, 0);
        assert_eq!(app.tabs[0].focused().title, "background-session");
        assert!(
            app.tabs[0]
                .focused_mut()
                .child
                .try_wait()
                .unwrap()
                .is_none()
        );
        app.new_tab();
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active, 1);
        app.args.shell = Some("/yiterm-nonexistent-shell".into());
        app.new_tab();
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active, 1);
        assert!(app.error.is_some());
    }

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
        assert!(is_paste_shortcut(
            Key::V,
            egui::Modifiers::CTRL | egui::Modifiers::SHIFT
        ));
        assert!(is_paste_shortcut(Key::V, egui::Modifiers::MAC_CMD));
        assert!(!is_paste_shortcut(Key::Insert, egui::Modifiers::NONE));
        assert!(!is_paste_shortcut(Key::Insert, egui::Modifiers::CTRL));
        assert!(!is_paste_shortcut(
            Key::Insert,
            egui::Modifiers::CTRL | egui::Modifiers::SHIFT
        ));
        assert!(!is_paste_shortcut(Key::V, egui::Modifiers::CTRL));
        assert_eq!(
            key_bytes(Key::Insert, egui::Modifiers::NONE, false),
            Some(b"\x1b[2~".to_vec())
        );
    }
}
