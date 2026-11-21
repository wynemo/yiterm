#![cfg_attr(windows, windows_subsystem = "windows")]
mod direct_share;
mod files;
mod git;
mod kitty;
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
use yiterm::{logging, mouse, paste, shell_directory, ssh};
#[cfg(windows)]
mod conpty;
use alacritty_terminal::{
    Term,
    event::{Event, EventListener},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionType},
    term::{
        ClipboardType, Config, TermMode,
        cell::{Cell, Flags},
    },
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
    #[command(flatten)]
    screen_share: yiterm::screen_share::Options,
    /// Listen for YiTerm screen sharing on this IP:PORT (macOS, Windows or Linux X11).
    #[arg(long, requires = "share_screen", conflicts_with_all = ["share_list", "share_stats"])]
    share_listen: Option<std::net::SocketAddr>,
    /// View a YiTerm host sharing its screen at this IP:PORT.
    #[arg(long, conflicts_with_all = ["ssh", "mosh", "shell", "share_screen"])]
    share_connect: Option<std::net::SocketAddr>,
    /// Connect directly to this OpenSSH host (remote shell must be POSIX).
    #[arg(long, conflicts_with = "mosh")]
    ssh: Option<String>,
    /// Connect using the bundled native Mosh client (Windows only).
    #[arg(long, conflicts_with_all = ["ssh", "shell"])]
    mosh: Option<String>,
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

impl Args {
    fn local_shell(&self) -> bool {
        self.ssh.is_none() && self.mosh.is_none() && self.share_connect.is_none()
    }
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
struct TerminalOutput {
    bytes: Vec<u8>,
    directory: Option<Option<std::path::PathBuf>>,
}
struct TerminalSession {
    #[cfg(unix)]
    git_session: Arc<git::Session>,
    log_id: uuid::Uuid,
    output_revision: u64,
    default_title: String,
    title: String,
    term: Term<Listener>,
    parser: Processor,
    graphics: kitty::Graphics,
    mouse_modes: mouse::MouseModes,
    mouse_wheel: mouse::Wheel,
    output: mpsc::Receiver<TerminalOutput>,
    reported_directory: Option<Option<std::path::PathBuf>>,
    repaint: Arc<Repaint>,
    events: mpsc::Receiver<Event>,
    transport: TerminalTransport,
    integration: ssh::Integration,
    #[cfg(windows)]
    clipboard_requests: yiterm::native_ssh::ClipboardRequests,
    #[cfg(windows)]
    direct_ssh_graphics: Option<bool>,
    paste_result: mpsc::Receiver<Result<PasteResult>>,
    paste_sender: mpsc::Sender<Result<PasteResult>>,
    paste_busy: bool,
    ime_preedit: String,
    mouse_button: Option<PointerButton>,
    selecting: bool,
    pending_clipboard: Option<String>,
    cursor_blink_started: Instant,
    reported_focus: Option<bool>,
    cursor_settings: CursorSettings,
    font_settings: settings::FontSettings,
    status: String,
    size: Size,
}
enum TerminalTransport {
    Pty {
        writer: Box<dyn Write + Send>,
        master: Box<dyn MasterPty + Send>,
        child: Box<dyn portable_pty::Child + Send + Sync>,
    },
    Direct(direct_share::Client),
}

enum TerminalReader {
    Pty(Box<dyn Read + Send>),
    Direct(tokio::sync::mpsc::Receiver<Vec<u8>>),
}

impl TerminalReader {
    fn next(&mut self, buffer: &mut [u8]) -> std::io::Result<Option<Vec<u8>>> {
        match self {
            Self::Pty(reader) => {
                let n = reader.read(buffer)?;
                Ok((n > 0).then(|| buffer[..n].to_vec()))
            }
            Self::Direct(reader) => Ok(reader.blocking_recv()),
        }
    }
}

impl TerminalSession {
    fn new(
        args: &Args,
        warning: Option<&str>,
        directory: Option<&std::path::Path>,
        shell_settings: &settings::ShellSettings,
    ) -> Result<Self> {
        let log_id = uuid::Uuid::new_v4();
        tracing::trace!(target: "yiterm::mouse",
            "build={} transport=raw_vt",
            option_env!("YITERM_BUILD_REV").unwrap_or("local")
        );
        tracing::info!(
            session_id = %log_id,
            session_kind = if args.share_connect.is_some() { "direct_share" } else if args.mosh.is_some() { "mosh" } else if args.ssh.is_some() { "ssh" } else { "local" },
            "Starting terminal session"
        );
        let integration = ssh::Integration::new(&std::env::current_exe()?)?;
        #[cfg(windows)]
        let clipboard_requests = integration.listen_for_osc52(wake_ui)?;
        if let Some(host) = args.ssh.as_ref().or(args.mosh.as_ref()) {
            ssh::parse(std::slice::from_ref(host))?;
        }
        #[cfg(windows)]
        let graphics_enabled =
            args.share_connect.is_some() || (conpty::initialize()? && args.mosh.is_none());
        #[cfg(unix)]
        let graphics_enabled = args.mosh.is_none();
        let initial_size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut directory_token = None;
        let mut shell_title = None;
        #[cfg(unix)]
        let mut session_shell = settings::default_shell();
        let (transport, mut reader) = if let Some(address) = args.share_connect {
            let (client, reader) = direct_share::Client::start(address, initial_size)?;
            (
                TerminalTransport::Direct(client),
                TerminalReader::Direct(reader),
            )
        } else {
            let pair = portable_pty::native_pty_system().openpty(initial_size)?;
            let mut command = if let Some(host) = &args.ssh {
                integration.direct(host)?
            } else if let Some(host) = &args.mosh {
                integration.direct_mosh(host)?
            } else {
                let (shell, arguments) = shell_settings.command(args.shell.as_deref())?;
                shell_title = std::path::Path::new(&shell)
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned);
                #[cfg(unix)]
                {
                    session_shell = shell.clone();
                }
                if arguments.is_empty() {
                    directory_token = shell_directory::token_for(&shell);
                }
                integration.shell_with_directory(&shell, &arguments, directory_token.as_deref())?
            };
            if let Some(directory) = directory {
                command.cwd(directory);
            }
            if args.local_shell() {
                for (name, value) in shell_settings.environment_variables()? {
                    command.env(name, value);
                }
            }
            command.env("TERM", "xterm-256color");
            #[cfg(windows)]
            {
                let path = args
                    .config
                    .clone()
                    .map(Ok)
                    .unwrap_or_else(logging::config_path)?;
                command.env("YITERM_CONFIG_PATH", std::path::absolute(path)?);
            }
            let child = pair
                .slave
                .spawn_command(command)
                .context("Could not start shell/ssh")?;
            drop(pair.slave);
            let reader = TerminalReader::Pty(pair.master.try_clone_reader()?);
            let writer = pair.master.take_writer()?;
            (
                TerminalTransport::Pty {
                    writer,
                    master: pair.master,
                    child,
                },
                reader,
            )
        };
        let (tx, output) = mpsc::sync_channel(64);
        let repaint = Arc::new(Repaint::new());
        let output_repaint = repaint.clone();
        std::thread::spawn(move || {
            let mut directories = shell_directory::Reports::new(directory_token);
            let mut buffer = [0; 16384];
            loop {
                match reader.next(&mut buffer) {
                    Ok(None) => {
                        tracing::info!("PTY output closed");
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "PTY read failed");
                        break;
                    }
                    Ok(Some(bytes)) => {
                        profiling::pty_read(bytes.len());
                        let output = TerminalOutput {
                            directory: directories.observe(&bytes),
                            bytes,
                        };
                        if tx.send(output).is_err() {
                            break;
                        }
                        output_repaint.store(true, Ordering::Release);
                        wake_ui();
                    }
                }
            }
            wake_ui();
        });
        let child_pid = match &transport {
            TerminalTransport::Pty { child, .. } => child.process_id(),
            TerminalTransport::Direct(_) => None,
        };
        tracing::info!(session_id = %log_id, ?child_pid, "Terminal session started");
        let (tx, events) = mpsc::channel();
        let size = Size { cols: 80, rows: 24 };
        let mut config = Config::default();
        config.default_cursor_style.blinking = true;
        let term = Term::new(config, &size, Listener(tx));
        let (paste_sender, paste_result) = mpsc::channel();
        let default_title = args
            .share_connect
            .map(|address| address.to_string())
            .or_else(|| args.ssh.clone())
            .or_else(|| args.mosh.clone())
            .or(shell_title)
            .unwrap_or_else(|| "Terminal".into());
        Ok(Self {
            #[cfg(unix)]
            git_session: Arc::new(git::Session::new(session_shell)),
            log_id,
            output_revision: 0,
            title: default_title.clone(),
            default_title,
            term,
            parser: Processor::new(),
            graphics: kitty::Graphics::new(graphics_enabled, repaint.clone()),
            mouse_modes: mouse::MouseModes::default(),
            mouse_wheel: mouse::Wheel::default(),
            output,
            reported_directory: None,
            repaint,
            events,
            transport,
            integration,
            #[cfg(windows)]
            clipboard_requests,
            #[cfg(windows)]
            direct_ssh_graphics: None,
            paste_result,
            paste_sender,
            paste_busy: false,
            ime_preedit: String::new(),
            mouse_button: None,
            selecting: false,
            pending_clipboard: None,
            cursor_blink_started: Instant::now(),
            reported_focus: None,
            cursor_settings: CursorSettings::default(),
            font_settings: settings::FontSettings::default(),
            status: warning
                .map(str::to_owned)
                .unwrap_or_else(|| "Ready · paste: Cmd+V / Ctrl+Shift+V / Shift+Insert".into()),
            size,
        })
    }
    fn send_input(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let writer: &mut dyn Write = match &mut self.transport {
            TerminalTransport::Pty { writer, .. } => writer,
            TerminalTransport::Direct(client) => client,
        };
        writer.write_all(bytes)?;
        writer.flush()
    }

    fn resize_transport(&self, size: PtySize) -> Result<()> {
        match &self.transport {
            TerminalTransport::Pty { master, .. } => master.resize(size),
            TerminalTransport::Direct(client) => {
                client.resize(size);
                Ok(())
            }
        }
    }

    fn is_direct_share(&self) -> bool {
        matches!(self.transport, TerminalTransport::Direct(_))
    }

    fn process_id(&self) -> Option<u32> {
        match &self.transport {
            TerminalTransport::Pty { child, .. } => child.process_id(),
            TerminalTransport::Direct(_) => None,
        }
    }

    fn close_transport(&mut self) {
        match &mut self.transport {
            TerminalTransport::Pty { child, .. } => {
                let _ = child.kill();
            }
            TerminalTransport::Direct(client) => client.close(),
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        if let Err(error) = self.send_input(bytes) {
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
        match self.send_input(report) {
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
        let direct = self.is_direct_share();
        let root = self.process_id();
        let group = self.foreground_group();
        let scp = self.integration.real_scp.clone();
        let ssh_context = self.integration.detection_context();
        std::thread::spawn(move || {
            let started = Instant::now();
            tracing::debug!("Clipboard paste started");
            let result = (|| -> Result<PasteResult> {
                let mut clipboard = arboard::Clipboard::new()?;
                if direct {
                    return Ok(PasteResult {
                        text: clipboard.get_text().context("Clipboard has no text")?,
                        target: None,
                        image_group: None,
                    });
                }
                let path = if let Ok(image) = clipboard.get_image() {
                    paste::save_image(image)?
                } else {
                    let text = clipboard
                        .get_text()
                        .context("Clipboard has no image or text")?;
                    match paste::save_base64_png(&text)? {
                        Some(path) => path,
                        None => {
                            return Ok(PasteResult {
                                text,
                                target: None,
                                image_group: None,
                            });
                        }
                    }
                };
                let root = root.context("Shell process is unavailable")?;
                let detected = ssh::detect(root, group, &ssh_context)?;
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
            match &self.transport {
                TerminalTransport::Pty { master, .. } => master.process_group_leader(),
                TerminalTransport::Direct(_) => None,
            }
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
        self.close_transport();
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
        theme.palette(index)
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

fn color_luminance(color: Rgba) -> f32 {
    #[cfg(target_os = "linux")]
    let channels = [color.r(), color.g(), color.b()].map(|value| f32::from(value) / 255.);
    #[cfg(not(target_os = "linux"))]
    let channels = [color.r, color.g, color.b];
    let [r, g, b] = channels.map(|value| {
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    });
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

fn color_contrast(a: f32, b: f32) -> f32 {
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

// Resolve reverse video first, then keep neutral foregrounds readable against
// the actual background (including PSReadLine's explicit black search region).
fn terminal_colors(cell: &Cell, theme: ThemeMode) -> (Rgba, Rgba) {
    let mut fg = color(cell.fg, true, theme);
    let mut bg = color(cell.bg, false, theme);
    let foreground = if cell.flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
        cell.bg
    } else {
        cell.fg
    };
    if theme == ThemeMode::Light
        && bg == theme.background()
        && matches!(
            foreground,
            Color::Named(NamedColor::White | NamedColor::BrightWhite) | Color::Indexed(7 | 15)
        )
    {
        fg = theme.foreground();
    }
    if theme == ThemeMode::Light
        && bg != theme.background()
        && matches!(
            foreground,
            Color::Named(NamedColor::Foreground | NamedColor::White | NamedColor::BrightWhite)
                | Color::Indexed(7 | 15)
        )
    {
        let background_luminance = color_luminance(bg);
        if color_contrast(color_luminance(fg), background_luminance) < 4.5 {
            let dark = theme.foreground();
            let white = rgb(0xFFFFFF);
            fg = if color_contrast(color_luminance(dark), background_luminance)
                > color_contrast(1., background_luminance)
            {
                dark
            } else {
                white
            };
        }
    }
    (fg, bg)
}

impl TerminalSession {
    fn poll(&mut self, theme: ThemeMode) -> bool {
        // VTE relies on its caller to expire synchronized updates. A client
        // waiting for a buffered terminal query must not stall the pane forever.
        if self
            .parser
            .sync_timeout()
            .sync_timeout()
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            self.parser.stop_sync(&mut self.term);
            self.repaint.store(true, Ordering::Release);
            wake_ui();
        }
        self.graphics.advance(&mut self.parser, &mut self.term);
        for batch in 0..128 {
            if self.graphics.busy() {
                break;
            }
            let Ok(output) = self.output.try_recv() else {
                break;
            };
            if let Some(directory) = output.directory {
                self.reported_directory = Some(directory);
            }
            let bytes = output.bytes;
            #[cfg(windows)]
            if self.direct_ssh_graphics.is_some() {
                // Console resizes/repaints belong to the local shell. They must
                // never be interleaved into the direct SSH VT/APC stream.
                continue;
            }
            self.output_revision = self.output_revision.wrapping_add(1);
            let previous_mode = *self.term.mode();
            let requests = self.mouse_modes.observe(&bytes);
            self.graphics.feed(&bytes);
            self.graphics.advance(&mut self.parser, &mut self.term);
            if tracing::enabled!(target: "yiterm::cursor", tracing::Level::DEBUG) {
                let visible = self.term.mode().contains(TermMode::SHOW_CURSOR);
                if previous_mode.contains(TermMode::SHOW_CURSOR) != visible {
                    tracing::debug!(target: "yiterm::cursor",
                        session_id = %self.log_id, source = "terminal_parser", visible,
                        "Terminal cursor visibility changed");
                }
                let cursor = self.term.renderable_content().cursor.point;
                tracing::trace!(target: "yiterm::cursor",
                    session_id = %self.log_id, source = "terminal_parser",
                    revision = self.output_revision, visible,
                    row = cursor.line.0, column = cursor.column.0,
                    display_offset = self.term.grid().display_offset(),
                    "Terminal cursor state after PTY output");
            }
            for (mode, enabled) in requests {
                tracing::debug!(target: "yiterm::mouse",
                    session_id = %self.log_id, source = "pty_output",
                    mode, name = mouse::mode_name(mode), enabled,
                    "Mouse mode request"
                );
            }
            if previous_mode != *self.term.mode() {
                tracing::debug!(target: "yiterm::mouse",
                    session_id = %self.log_id, source = "terminal_parser",
                    previous = ?previous_mode, current = ?self.term.mode(),
                    mouse_reporting = self.term.mode().intersects(TermMode::MOUSE_MODE),
                    "Terminal mode changed"
                );
            }
            self.cursor_blink_started = Instant::now();
            // Continue draining bounded batches even if the producer has stopped.
            if batch == 127 {
                self.repaint.store(true, Ordering::Release);
                wake_ui();
            }
        }
        #[cfg(windows)]
        while !self.graphics.busy() {
            let Ok(request) = self.clipboard_requests.try_recv() else {
                break;
            };
            if request.direct {
                if !request.bytes.is_empty() && self.direct_ssh_graphics.is_none() {
                    self.direct_ssh_graphics = Some(self.graphics.enabled);
                    self.graphics.enabled = true;
                }
                self.mouse_modes.observe(&request.bytes);
                self.graphics.feed(&request.bytes);
                self.graphics.advance(&mut self.parser, &mut self.term);
            } else {
                self.parser.advance(&mut self.term, &request.bytes);
            }
            if let Some(drained) = request.drained {
                // The SSH shim waits for this barrier before it exits, so no
                // real shell prompt can be in the PTY queue yet.
                while self.output.try_recv().is_ok() {}
                self.graphics.end_stream();
                self.parser.stop_sync(&mut self.term);
                let reset=b"\x18\x1b[?2026l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?1016l\x1b[?2004l\x1b[?25h\x1b[?1049l";
                self.mouse_modes.observe(reset);
                self.parser.advance(&mut self.term, reset);
                if let Some(enabled) = self.direct_ssh_graphics.take() {
                    self.graphics.enabled = enabled;
                }
                let _ = drained.send(());
            }
            self.repaint.store(true, Ordering::Release);
            wake_ui();
        }
        let direct_share = self.is_direct_share();
        let mut replies = String::new();
        while let Ok(event) = self.events.try_recv() {
            let reply = match event {
                Event::PtyWrite(text) => text,
                Event::ColorRequest(index, format) => {
                    // Use the same palette as the renderer so applications can
                    // detect a light background through OSC 10/11 queries.
                    let value = match index {
                        0..=255 => color(Color::Indexed(index as u8), true, theme),
                        index if index == NamedColor::Foreground as usize => theme.foreground(),
                        index if index == NamedColor::Background as usize => theme.background(),
                        index if index == NamedColor::Cursor as usize => theme.cursor(),
                        _ => continue,
                    };
                    #[cfg(target_os = "linux")]
                    let [r, g, b] = [value.r(), value.g(), value.b()];
                    #[cfg(not(target_os = "linux"))]
                    let [r, g, b] =
                        [value.r, value.g, value.b].map(|channel| (channel * 255.).round() as u8);
                    format(alacritty_terminal::vte::ansi::Rgb { r, g, b })
                }
                Event::TextAreaSizeRequest(format) => {
                    let [cell_width, cell_height] = self.graphics.cell_pixels();
                    format(alacritty_terminal::event::WindowSize {
                        num_lines: self.size.rows as u16,
                        num_cols: self.size.cols as u16,
                        cell_width: cell_width.min(u16::MAX / self.size.cols as u16),
                        cell_height: cell_height.min(u16::MAX / self.size.rows as u16),
                    })
                }
                Event::Title(title) => {
                    self.title = title;
                    continue;
                }
                Event::ResetTitle => {
                    self.title.clone_from(&self.default_title);
                    continue;
                }
                Event::ClipboardStore(ClipboardType::Clipboard, text) if !direct_share => {
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
                    continue;
                }
                _ => continue,
            };
            if direct_share {
                // Keep terminal replies ordered while sharing a transport frame.
                replies.push_str(&reply);
            } else {
                self.write(reply.as_bytes());
            }
        }
        if !replies.is_empty() {
            self.write(replies.as_bytes());
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
        if let TerminalTransport::Pty { child, .. } = &mut self.transport
            && let Ok(Some(status)) = child.try_wait()
        {
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

const TAB_HEIGHT: f32 = 26.;
const TAB_MIN_WIDTH: f32 = 176.;
const TAB_TITLE_MAX_WIDTH: f32 = 184.;
const TAB_ACTIVE_LINE_HEIGHT: f32 = 2.;

struct TabDrag(u64);

struct TerminalTab {
    files: files::FileState,
    id: u64,
    custom_title: Option<String>,
    panes: Vec<TerminalSession>,
    layout: PaneLayout,
    active: usize,
    #[cfg(windows)]
    browser: Option<gpui::Entity<ui::browser::BrowserUi>>,
}

impl TerminalTab {
    fn new(session: TerminalSession) -> Self {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            custom_title: None,
            files: Default::default(),
            panes: vec![session],
            layout: PaneLayout::Leaf(0),
            active: 0,
            #[cfg(windows)]
            browser: None,
        }
    }

    fn title(&self) -> &str {
        self.custom_title
            .as_deref()
            .unwrap_or(&self.focused().title)
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

    fn poll(&mut self, theme: ThemeMode) -> bool {
        for index in (0..self.panes.len()).rev() {
            if !self.panes[index].poll(theme) {
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
    rename_tab: Option<(u64, String)>,
    share_address: Option<String>,
    share_recent: Vec<std::net::SocketAddr>,
    #[cfg(target_os = "linux")]
    share_host_dialog: bool,
    #[cfg(target_os = "linux")]
    share_host: Option<direct_share::host::Host>,
    theme: ThemeMode,
    cursor_settings: CursorSettings,
    font_settings: settings::FontSettings,
    browser_settings: settings::BrowserSettings,
    shell_settings: settings::ShellSettings,
    files_settings: settings::FilesSettings,
    git: git::Controller,
    #[cfg(target_os = "linux")]
    files_focused: bool,
}

impl TerminalApp {
    fn begin_rename_tab(&mut self, id: u64) {
        self.rename_tab = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .map(|tab| (id, tab.title().to_owned()));
    }

    fn finish_rename_tab(&mut self) {
        if let Some((id, title)) = self.rename_tab.take()
            && let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id)
        {
            let title = title.trim();
            tab.custom_title = (!title.is_empty()).then(|| title.to_owned());
        }
    }

    fn report_terminal_focus(&mut self, window_focused: bool) {
        for (tab_index, tab) in self.tabs.iter_mut().enumerate() {
            for (pane_index, pane) in tab.panes.iter_mut().enumerate() {
                if !pane.term.mode().contains(TermMode::FOCUS_IN_OUT) {
                    pane.reported_focus = None;
                    continue;
                }
                let focused =
                    window_focused && tab_index == self.active && pane_index == tab.active;
                if pane.reported_focus != Some(focused) {
                    let bytes: &[u8] = if focused { b"\x1b[I" } else { b"\x1b[O" };
                    if pane.send_input(bytes).is_ok() {
                        pane.reported_focus = Some(focused);
                    }
                }
            }
        }
    }

    fn new(args: Args, logging: &logging::Logging) -> Result<Self> {
        let path = args
            .config
            .clone()
            .map(Ok)
            .unwrap_or_else(logging::config_path)?;
        let shell_settings = settings::load_shell(&path)?;
        let session =
            TerminalSession::new(&args, logging.warning.as_deref(), None, &shell_settings)?;
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
            rename_tab: None,
            share_address: None,
            share_recent: Vec::new(),
            #[cfg(target_os = "linux")]
            share_host_dialog: false,
            #[cfg(target_os = "linux")]
            share_host: None,
            theme: settings::load_theme(&path)?,
            cursor_settings,
            font_settings: settings::load_font(&path)?,
            browser_settings: settings::load_browser(&path)?,
            shell_settings,
            files_settings: settings::load_files(&path)?,
            git: Default::default(),
            #[cfg(target_os = "linux")]
            files_focused: false,
        })
    }

    fn save_files_settings(&mut self) {
        let result = self
            .args
            .config
            .clone()
            .map(Ok)
            .unwrap_or_else(logging::config_path)
            .and_then(|path| settings::save_files(&path, self.files_settings));
        if let Err(error) = result {
            self.error = Some(format!("Cannot save file panel settings: {error:#}"));
        }
    }
    fn toggle_files(&mut self) {
        self.files_settings.visible = !self.files_settings.visible;
        if self.files_settings.visible {
            self.tabs[self.active].files.ensure_loaded();
        } else {
            for tab in &mut self.tabs {
                tab.files.cancel();
            }
            self.git.sync(
                None,
                None,
                #[cfg(unix)]
                None,
            );
        }
        self.save_files_settings();
    }
    fn poll_files(&mut self, presented: bool) -> bool {
        let visible =
            self.files_settings.visible && presented && !self.settings_open && !self.about_open;
        // Update the target before accepting replies, so a late reply from the
        // previously focused pane cannot replace the newly focused pane's root.
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            if index != self.active || !visible {
                tab.files.pause_follow();
            }
        }
        if visible && !self.tabs.is_empty() {
            let tab = &mut self.tabs[self.active];
            tab.files.ensure_loaded();
            // All sidebars follow the active local terminal, including when
            // upgrading a configuration with the legacy follow flag disabled.
            let pane = tab.focused();
            if self.args.ssh.is_none() && self.args.mosh.is_none() && !pane.is_direct_share() {
                if let Some(directory) = pane.reported_directory.clone() {
                    // A prompt report is authoritative for PowerShell's logical
                    // location. Stop PID queries before applying it so a late
                    // process-cwd reply cannot put the previous directory back.
                    tab.files.pause_follow();
                    if let Some(path) = directory {
                        if path != tab.files.root {
                            tab.files.set_root(path);
                        }
                    }
                } else {
                    // Unix foreground groups cover nested local shells. Other
                    // local shells keep the existing host-process fallback.
                    let pid = pane
                        .foreground_group()
                        .and_then(|pid| u32::try_from(pid).ok())
                        .or_else(|| pane.process_id());
                    tab.files.follow_directory(pid);
                }
            } else {
                tab.files.pause_follow();
            }
        }
        let mut changed = false;
        for tab in &mut self.tabs {
            changed |= tab.files.poll();
        }
        let active = visible && !self.tabs.is_empty();
        let folder = active.then(|| self.tabs[self.active].files.root.clone());
        let activity = active.then(|| {
            let pane = self.tabs[self.active].focused();
            (pane.log_id, pane.output_revision)
        });
        #[cfg(unix)]
        let session = active.then(|| &self.tabs[self.active].focused().git_session);
        changed |= self.git.sync(
            folder.as_deref(),
            activity,
            #[cfg(unix)]
            session,
        );
        let acted = self.git.take_acted();
        if !acted.is_empty() {
            for (index, tab) in self.tabs.iter_mut().enumerate() {
                if acted.iter().any(|root| tab.files.root.starts_with(root)) {
                    if visible && index == self.active {
                        tab.files.refresh();
                    } else {
                        tab.files.invalidate();
                    }
                }
            }
        }
        changed
    }
    fn set_sidebar_mode(&mut self, mode: settings::FilesPanelMode) {
        self.files_settings.mode = mode;
        self.git.back();
        self.save_files_settings();
    }
    fn new_session(&self, directory: Option<&std::path::Path>) -> Result<TerminalSession> {
        let mut args = self.args.clone();
        args.share_connect = None;
        TerminalSession::new(
            &args,
            self.warning.as_deref(),
            directory,
            &self.shell_settings,
        )
    }

    fn begin_connect_share(&mut self) {
        self.share_recent = direct_share::recent_addresses().unwrap_or_else(|error| {
            tracing::warn!(%error, "Cannot load YiTerm connection history");
            Vec::new()
        });
        self.share_address = Some(
            self.share_recent
                .first()
                .map(ToString::to_string)
                .unwrap_or_default(),
        );
    }

    fn new_share_session(&self, address: std::net::SocketAddr) -> Result<TerminalSession> {
        let mut args = self.args.clone();
        args.ssh = None;
        args.mosh = None;
        args.shell = None;
        args.share_connect = Some(address);
        TerminalSession::new(&args, None, None, &self.shell_settings)
    }

    fn reconnect_share(&mut self) {
        let pane = self.tabs[self.active].focused();
        let TerminalTransport::Direct(client) = &pane.transport else {
            return;
        };
        match self.new_share_session(client.address) {
            Ok(mut session) => {
                session.parser.advance(
                    &mut session.term,
                    format!("Connecting to {}…\r\n", client.address).as_bytes(),
                );
                *self.tabs[self.active].focused_mut() = session;
                self.error = None;
            }
            Err(error) => self.error = Some(format!("Cannot reconnect: {error:#}")),
        }
    }

    fn connect_share(&mut self) {
        let Some(address) = self.share_address.take() else {
            return;
        };
        let result = address
            .trim()
            .parse()
            .context("Enter an IP address and port, for example 192.168.1.20:9443")
            .and_then(|address| self.new_share_session(address));
        match result {
            Ok(session) => {
                self.tabs.push(TerminalTab::new(session));
                self.select(self.tabs.len() - 1);
                self.error = None;
            }
            Err(error) => self.error = Some(format!("{error:#}")),
        }
    }

    fn new_tab(&mut self) {
        match self.new_session(None) {
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
        let directory = if self.args.local_shell() {
            let pane = self.tabs[self.active].focused();
            match &pane.reported_directory {
                // A non-filesystem PowerShell location must not fall back to
                // the shell process's stale Win32 working directory.
                Some(directory) => directory.clone(),
                None => pane
                    .foreground_group()
                    .and_then(|pid| u32::try_from(pid).ok())
                    .and_then(files::process_directory)
                    .or_else(|| pane.process_id().and_then(files::process_directory)),
            }
        } else {
            None
        };
        match self.new_session(directory.as_deref()) {
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
        if self
            .rename_tab
            .as_ref()
            .is_some_and(|(id, _)| *id == self.tabs[index].id)
        {
            self.rename_tab = None;
        }
        let was_active = index == self.active;
        self.tabs.remove(index);
        self.active = active_after_close(self.active, index, self.tabs.len());
        if was_active && !self.tabs.is_empty() {
            self.select(self.active);
        }
    }

    fn move_tab(&mut self, source_id: u64, target_id: u64, before: bool) {
        if source_id == target_id {
            return;
        }
        let Some(source) = self.tabs.iter().position(|tab| tab.id == source_id) else {
            return;
        };
        let Some(target) = self.tabs.iter().position(|tab| tab.id == target_id) else {
            return;
        };
        let insertion = target + usize::from(!before) - usize::from(source < target);
        if insertion == source {
            return;
        }

        let active_id = self.tabs[self.active].id;
        let tab = self.tabs.remove(source);
        self.tabs.insert(insertion, tab);
        self.active = self
            .tabs
            .iter()
            .position(|tab| tab.id == active_id)
            .unwrap();
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
    #[cfg(windows)]
    if std::env::args_os()
        .skip(1)
        .any(|arg| arg == "--share-screen")
    {
        use windows_sys::Win32::System::Console::{
            ATTACH_PARENT_PROCESS, AttachConsole, SetConsoleCtrlHandler,
        };
        // The GUI-subsystem binary needs the caller's console for sharing CLI output and Ctrl+C.
        unsafe {
            AttachConsole(ATTACH_PARENT_PROCESS);
            SetConsoleCtrlHandler(None, 0);
        }
    }
    let args = Args::parse();
    if let Some(address) = args.share_listen {
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        return direct_share::host::run(address, &args.screen_share);
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        anyhow::bail!(
            "YiTerm direct sharing requires a macOS, Windows or Linux X11 host ({address})"
        );
    }
    if args.screen_share.share_screen {
        return yiterm::screen_share::run(&args.screen_share);
    }
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
