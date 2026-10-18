#![cfg_attr(windows, windows_subsystem = "windows")]
#[cfg(not(target_os = "linux"))]
mod profiling;
#[cfg(target_os = "linux")]
#[path = "profiling_egui.rs"]
mod profiling;
#[cfg(not(target_os = "linux"))]
mod ui;
#[cfg(target_os = "linux")]
#[path = "ui_egui.rs"]
mod ui;
use portable_pty::{MasterPty, PtySize};
use yiterm::settings::{self, CursorSettings, CursorShape};
use yiterm::theme::ThemeMode;
use yiterm::{logging, mouse, paste, ssh};
#[cfg(windows)]
mod conpty;
use alacritty_terminal::{
    Term,
    event::{Event, EventListener},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionType},
    term::{ClipboardType, Config, TermMode, cell::Flags},
    vte::ansi::{Color, NamedColor, Processor},
};
use anyhow::{Context, Result};
use clap::Parser;
use std::{
    io::{Read, Write},
    sync::{Arc, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};
use yiterm::backend::{Color as Rgba, PointerButton, Repaint, rgb};
const TERMINAL_FONT_SIZE: f32 = 16.0;
const TERMINAL_LINE_HEIGHT: f32 = 1.3;
const MAX_OSC52_CLIPBOARD_BYTES: usize = 1024 * 1024;
fn rgb_bytes(r: u8, g: u8, b: u8) -> Rgba {
    rgb((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b))
}
// Coalesce bursts of PTY data without keeping the UI awake while idle.
#[cfg(not(target_os = "linux"))]
static UI_WAKE: std::sync::LazyLock<(async_channel::Sender<()>, async_channel::Receiver<()>)> =
    std::sync::LazyLock::new(|| async_channel::bounded(1));
#[cfg(not(target_os = "linux"))]
fn wake_ui() {
    let _ = UI_WAKE.0.try_send(());
}
#[cfg(target_os = "linux")]
fn wake_ui() {}
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
    repaint: Arc<Repaint>,
    events: mpsc::Receiver<Event>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    integration: ssh::Integration,
    #[cfg(windows)]
    clipboard_requests: yiterm::native_ssh::ClipboardRequests,
    paste_result: mpsc::Receiver<Result<PasteResult>>,
    paste_sender: mpsc::Sender<Result<PasteResult>>,
    paste_busy: bool,
    ime_preedit: String,
    mouse_button: Option<PointerButton>,
    selecting: bool,
    pending_clipboard: Option<String>,
    cursor_blink_started: Instant,
    cursor_settings: CursorSettings,
    font_settings: settings::FontSettings,
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
        #[cfg(windows)]
        let clipboard_requests = integration.listen_for_osc52()?;
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
        let repaint = Arc::new(Repaint::new());
        let output_repaint = repaint.clone();
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
                        profiling::pty_read(n);
                        if tx.send(buffer[..n].to_vec()).is_err() {
                            break;
                        }
                        output_repaint.store(true, Ordering::Release);
                        wake_ui();
                    }
                }
            }
            wake_ui();
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
            repaint,
            events,
            writer,
            master: pair.master,
            child,
            integration,
            #[cfg(windows)]
            clipboard_requests,
            paste_result,
            paste_sender,
            paste_busy: false,
            ime_preedit: String::new(),
            mouse_button: None,
            selecting: false,
            pending_clipboard: None,
            cursor_blink_started: Instant::now(),
            cursor_settings: CursorSettings::default(),
            font_settings: settings::FontSettings::default(),
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
    #[cfg(not(target_os = "linux"))]
    fn copy_selection(&mut self) {
        if let Some(text) = self.term.selection_to_string().filter(|s| !s.is_empty()) {
            self.pending_clipboard = Some(text);
            wake_ui();
            self.term.selection = None;
            self.selecting = false;
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
                            paste::upload_detected(&path, &detected, &scp)?,
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
            wake_ui();
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

fn color(color: Color, foreground: bool, theme: ThemeMode) -> Rgba {
    let index = match color {
        Color::Spec(rgb) => return rgb_bytes(rgb.r, rgb.g, rgb.b),
        Color::Named(NamedColor::Foreground) => return theme.foreground(),
        Color::Named(NamedColor::Background) => return theme.background(),
        Color::Named(named) => named as usize,
        Color::Indexed(index) => index as usize,
    };
    if index < 16 {
        // Shell syntax highlighting can explicitly request ANSI white instead
        // of the default foreground. Keep it readable on a light background
        // without changing explicitly requested white backgrounds.
        if foreground && theme == ThemeMode::Light && matches!(index, 7 | 15) {
            theme.foreground()
        } else {
            theme.palette(index)
        }
    } else if index < 232 {
        let n = index - 16;
        let level = |n| if n == 0 { 0 } else { 55 + n as u8 * 40 };
        rgb_bytes(level(n / 36), level(n / 6 % 6), level(n % 6))
    } else if index < 256 {
        {
            let v = 8 + (index - 232) as u8 * 10;
            rgb_bytes(v, v, v)
        }
    } else if foreground {
        theme.foreground()
    } else {
        theme.background()
    }
}

impl TerminalSession {
    fn poll(&mut self) -> bool {
        for batch in 0..128 {
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
            // Continue draining bounded batches even if the producer has stopped.
            if batch == 127 {
                self.repaint.store(true, Ordering::Release);
                wake_ui();
            }
        }
        #[cfg(windows)]
        while let Ok(sequence) = self.clipboard_requests.try_recv() {
            tracing::info!(
                bytes = sequence.len(),
                "Received OSC 52 request from built-in SSH"
            );
            self.parser.advance(&mut self.term, &sequence);
        }
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::PtyWrite(text) => self.write(text.as_bytes()),
                Event::Title(title) => self.title = title,
                Event::ResetTitle => self.title = "Terminal".into(),
                Event::ClipboardStore(ClipboardType::Clipboard, text) => {
                    if text.len() > MAX_OSC52_CLIPBOARD_BYTES {
                        self.pending_clipboard = None;
                        tracing::warn!(
                            bytes = text.len(),
                            limit = MAX_OSC52_CLIPBOARD_BYTES,
                            "Ignored oversized OSC 52 clipboard request"
                        );
                        self.status = "Remote clipboard request exceeds 1 MiB".into();
                    } else {
                        tracing::info!(
                            bytes = text.len(),
                            chars = text.chars().count(),
                            "Received OSC 52 host clipboard request"
                        );
                        self.pending_clipboard = Some(text);
                        self.status = "Writing remote text to host clipboard…".into();
                    }
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

    fn flush_clipboard_with(&mut self, write: impl FnOnce(String) -> Result<()>) {
        if let Some(text) = self.pending_clipboard.take() {
            let chars = text.chars().count();
            match write(text) {
                Ok(()) => {
                    self.status = format!("Copied {chars} chars to host clipboard");
                    tracing::info!(chars, "Remote application copied text to host clipboard");
                }
                Err(error) => {
                    tracing::warn!(chars, error = %format!("{error:#}"), "Host clipboard write failed");
                    self.status = format!("Host clipboard write failed: {error:#}");
                }
            }
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
        previous.selecting = false;
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
}
struct TerminalApp {
    #[cfg(target_os = "linux")]
    profile: Option<profiling::Profile>,
    window_title: String,
    tabs: Vec<TerminalTab>,
    active: usize,
    args: Args,
    log_directory: std::path::PathBuf,
    warning: Option<String>,
    error: Option<String>,
    about_open: bool,
    about_licenses: bool,
    updater: yiterm::update::Controller,
    settings_open: bool,
    theme: ThemeMode,
    cursor_settings: CursorSettings,
    font_settings: settings::FontSettings,
}

impl TerminalApp {
    fn new(args: Args, logging: &logging::Logging) -> Result<Self> {
        let session = TerminalSession::new(&args, logging.warning.as_deref())?;
        let path = args
            .config
            .clone()
            .map(Ok)
            .unwrap_or_else(logging::config_path)?;
        let cursor_settings = settings::load(&path)?;
        let updater = yiterm::update::Controller::default();
        let update_failed = matches!(updater.state, yiterm::update::State::Failed(_));
        Ok(Self {
            #[cfg(target_os = "linux")]
            profile: profiling::Profile::new(),
            window_title: String::new(),
            tabs: vec![TerminalTab::new(session)],
            active: 0,
            args,
            log_directory: logging.directory.clone(),
            warning: logging.warning.clone(),
            error: None,
            about_open: update_failed,
            about_licenses: false,
            updater,
            settings_open: false,
            theme: settings::load_theme(&path)?,
            cursor_settings,
            font_settings: settings::load_font(&path)?,
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

struct TerminalLink {
    url: String,
    points: Vec<Point>,
}

fn terminal_link<T: EventListener>(term: &Term<T>, point: Point) -> Option<String> {
    terminal_link_details(term, point).map(|link| link.url)
}

fn terminal_link_details<T: EventListener>(term: &Term<T>, point: Point) -> Option<TerminalLink> {
    let grid = term.grid();
    if let Some(link) = grid[point].hyperlink() {
        let url = browser_url(link.uri())?;
        let columns = grid.columns() as i32;
        let index = point.line.0 * columns + point.column.0 as i32;
        let to_point = |index: i32| {
            Point::new(
                Line(index.div_euclid(columns)),
                Column(index.rem_euclid(columns) as usize),
            )
        };
        let mut start = index;
        let mut end = index;
        while start > grid.topmost_line().0 * columns
            && grid[to_point(start - 1)].hyperlink().as_ref() == Some(&link)
        {
            start -= 1;
        }
        while end < (grid.bottommost_line().0 + 1) * columns - 1
            && grid[to_point(end + 1)].hyperlink().as_ref() == Some(&link)
        {
            end += 1;
        }
        return Some(TerminalLink {
            url,
            points: (start..=end).map(to_point).collect(),
        });
    }
    let last = Column(grid.columns() - 1);
    let mut start = point.line;
    while start > grid.topmost_line()
        && grid[Line(start.0 - 1)][last]
            .flags
            .contains(Flags::WRAPLINE)
    {
        start -= 1;
    }
    let mut end = point.line;
    while end < grid.bottommost_line() && grid[end][last].flags.contains(Flags::WRAPLINE) {
        end += 1;
    }
    let mut text = String::new();
    let mut target = None;
    let mut positions = Vec::new();
    for line in start.0..=end.0 {
        for column in 0..grid.columns() {
            let current = Point::new(Line(line), Column(column));
            let cell = &grid[current];
            positions.push((text.len(), current));
            if current == point {
                target = Some(text.len());
            }
            if !cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                text.push(cell.c);
                if let Some(extra) = cell.zerowidth() {
                    text.extend(extra);
                }
            }
        }
    }
    let target = target?;
    for (start, _) in text.match_indices("http") {
        let candidate = &text[start..];
        if !(candidate.starts_with("https://") || candidate.starts_with("http://")) {
            continue;
        }
        let candidate = link_text(candidate);
        if target >= start && target < start + candidate.len() {
            let url = browser_url(candidate)?;
            let points = positions
                .iter()
                .filter_map(|(offset, point)| {
                    (*offset >= start && *offset < start + candidate.len()).then_some(*point)
                })
                .collect();
            return Some(TerminalLink { url, points });
        }
    }
    None
}

/// Stop at prose delimiters, while retaining balanced brackets inside URLs.
fn link_text(text: &str) -> &str {
    let mut brackets = Vec::new();
    let mut end = text.len();
    for (offset, ch) in text.char_indices() {
        if ch.is_whitespace()
            || matches!(
                ch,
                '<' | '>'
                    | '"'
                    | '\''
                    | '`'
                    | '（'
                    | '）'
                    | '【'
                    | '】'
                    | '「'
                    | '」'
                    | '。'
                    | '，'
                    | '；'
                    | '！'
                    | '？'
            )
        {
            end = offset;
            break;
        }
        match ch {
            '(' => brackets.push(')'),
            '[' => brackets.push(']'),
            '{' => brackets.push('}'),
            ')' | ']' | '}' if brackets.pop() != Some(ch) => {
                end = offset;
                break;
            }
            _ => {}
        }
    }
    text[..end].trim_end_matches(['.', ',', ';', ':', '!', '?'])
}

fn browser_url(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host_str().is_some()).then(|| value.to_owned())
}

fn main() -> Result<()> {
    if let Some(result) = yiterm::update::run_helper_if_requested() {
        return result;
    }
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
    ui::run(app)
}
// GPUI shapes complete cell contents, including combining and joining sequences.
fn terminal_cell_text(cell: &alacritty_terminal::term::cell::Cell) -> String {
    std::iter::once(cell.c)
        .chain(cell.zerowidth().into_iter().flatten().copied())
        .filter(|ch| {
            #[cfg(target_os = "linux")]
            {
                !ui::is_default_ignorable(*ch)
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = ch;
                true
            }
        })
        .collect()
}

#[cfg(test)]
mod terminal_tests;

#[cfg(windows)]
fn windows_clipboard_owner(window: &gpui::Window) -> Result<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    match HasWindowHandle::window_handle(window)
        .context("getting the YiTerm window handle for clipboard ownership")?
        .as_raw()
    {
        RawWindowHandle::Win32(handle) => Ok(handle.hwnd.get()),
        handle => anyhow::bail!("Expected a Win32 clipboard owner window, got {handle:?}"),
    }
}

#[cfg(windows)]
fn write_windows_clipboard(text: String, owner: isize) -> Result<()> {
    use clipboard_win::{Clipboard, Getter, Setter, formats::Unicode};

    let owner = owner as clipboard_win::types::HWND;
    let clipboard = Clipboard::new_attempts_for(owner, 5)
        .context("opening Windows host clipboard with YiTerm window owner")?;
    Unicode
        .write_clipboard(&text)
        .context("writing Unicode text to Windows host clipboard")?;
    drop(clipboard);

    let clipboard = Clipboard::new_attempts_for(owner, 5)
        .context("reopening Windows host clipboard to verify the write")?;
    let mut actual = String::new();
    Unicode
        .read_clipboard(&mut actual)
        .context("reading back Windows host clipboard text")?;
    if actual != text {
        anyhow::bail!("Windows host clipboard read-back did not match the requested text");
    }
    drop(clipboard);
    Ok(())
}
