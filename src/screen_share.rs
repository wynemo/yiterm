//! Screen capture and Kitty graphics encoding.
use anyhow::{Result, bail};
use clap::Args;
#[cfg(any(target_os = "linux", windows))]
mod desktop;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
mod graphics;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
mod input;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
mod unlock;
#[cfg(windows)]
pub mod windows;

#[derive(Args, Clone, Debug)]
pub struct Options {
    /// Share a screen as Kitty graphics (macOS 13+, Windows or Linux X11).
    #[arg(long, conflicts_with_all = ["ssh", "mosh", "shell"])]
    pub share_screen: bool,
    /// List available display and window IDs on the capture host.
    #[arg(long, requires = "share_screen")]
    pub share_list: bool,
    #[arg(long, requires = "share_screen", conflicts_with = "share_window")]
    pub share_display: Option<u32>,
    #[arg(long, requires = "share_screen")]
    pub share_window: Option<u32>,
    /// Maximum capture width; 0 preserves native resolution (up to 3840 pixels).
    #[arg(long, requires = "share_screen", default_value = "0", value_parser = clap::value_parser!(u32).range(0..=3840))]
    pub share_width: u32,
    #[arg(long, requires = "share_screen", default_value = "15", value_parser = clap::value_parser!(u32).range(1..=60))]
    pub share_fps: u32,
    /// Fix color quality instead of adapting to output pressure.
    #[arg(long, requires = "share_screen", value_parser = ["lossless", "auto", "565", "555", "444"])]
    pub share_quality: Option<String>,
    /// Allow automatic resolution reduction under sustained output pressure.
    #[arg(
        long,
        requires = "share_screen",
        conflicts_with = "share_fixed_resolution"
    )]
    pub share_adaptive_resolution: bool,
    /// View without remote keyboard or mouse control.
    #[arg(long, requires = "share_screen")]
    pub share_read_only: bool,
    /// Disable adaptive resolution reduction (capture width remains fixed).
    #[arg(long, requires = "share_screen")]
    pub share_fixed_resolution: bool,
    /// Print capture/encoding/output statistics to stderr every five seconds.
    #[arg(long, requires = "share_screen")]
    pub share_stats: bool,
}

pub fn run(options: &Options) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::run(options)
    }
    #[cfg(windows)]
    {
        if options.share_list {
            return windows::list_sources();
        }
        bail!("Use --share-screen --share-listen IP:PORT to share from Windows");
    }
    #[cfg(target_os = "linux")]
    {
        if options.share_list {
            return linux::list_sources();
        }
        bail!("Use --share-screen --share-listen IP:PORT to share from Linux");
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = options;
        bail!("Screen capture requires macOS 13 or later, Windows or Linux X11")
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::graphics::*;
    use super::*;
    use std::{
        io::{IsTerminal, Read, Write},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    pub(super) static STOP: AtomicBool = AtomicBool::new(false);
    pub(super) extern "C" fn stop(_: libc::c_int) {
        STOP.store(true, Ordering::Relaxed);
    }
    struct Terminal {
        saved: libc::termios,
        signals: Vec<(i32, libc::sighandler_t)>,
    }
    impl Terminal {
        fn enter(read_only: bool) -> Result<Self> {
            let mut saved = unsafe { std::mem::zeroed::<libc::termios>() };
            anyhow::ensure!(
                unsafe { libc::tcgetattr(0, &mut saved) } == 0,
                "Cannot read terminal mode"
            );
            let mut mode = saved;
            mode.c_lflag &= !(libc::ICANON | libc::ECHO);
            if !read_only {
                mode.c_lflag &= !(libc::ISIG | libc::IEXTEN);
                mode.c_iflag &= !(libc::IXON
                    | libc::IXOFF
                    | libc::ICRNL
                    | libc::INLCR
                    | libc::IGNCR
                    | libc::ISTRIP);
            }
            mode.c_cc[libc::VMIN] = 0;
            mode.c_cc[libc::VTIME] = 0;
            anyhow::ensure!(
                unsafe { libc::tcsetattr(0, libc::TCSANOW, &mode) } == 0,
                "Cannot set terminal mode"
            );
            STOP.store(false, Ordering::Relaxed);
            let signals = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGPIPE]
                .into_iter()
                .map(|s| {
                    (s, unsafe {
                        libc::signal(s, stop as *const () as libc::sighandler_t)
                    })
                })
                .collect();
            let terminal = Self { saved, signals };
            std::io::stdout().write_all(b"\x1b[?1049h\x1b[?25l\x1b[?1004h\x1b[2J\x1b[H")?;
            Ok(terminal)
        }
    }
    impl Drop for Terminal {
        fn drop(&mut self) {
            let mut out = std::io::stdout();
            let _ = out.write_all(
                b"\x1b[?2026l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1016l\x1b[?1004l\x1b[?2004l\x1b[?25h\x1b[?1049l",
            );
            let _ = out.flush();
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &self.saved);
                for &(s, handler) in &self.signals {
                    libc::signal(s, handler);
                }
            }
        }
    }
    fn geometry(
        source: Option<(usize, usize)>,
        measured_cell: Option<[usize; 2]>,
        measured_area: Option<[usize; 2]>,
        measured_grid: Option<[usize; 2]>,
    ) -> Result<(usize, usize)> {
        let mut size = unsafe { std::mem::zeroed::<libc::winsize>() };
        anyhow::ensure!(
            unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut size) } == 0
                && size.ws_col > 0
                && size.ws_row > 0,
            "Cannot determine terminal dimensions; use ssh -t"
        );
        let [grid_cols, grid_rows] =
            measured_grid.unwrap_or([usize::from(size.ws_col), usize::from(size.ws_row)]);
        let cols = grid_cols.min(1000);
        let rows = grid_rows.min(500);
        if let Some((w, h)) = source {
            // PTYs can omit pixel dimensions; use a conventional 1:2 cell as fallback.
            let cell_w = if let Some(area) = measured_area {
                area[0] as f64 / grid_cols as f64
            } else if let Some(cell) = measured_cell {
                cell[0] as f64
            } else if size.ws_xpixel > 0 {
                f64::from(size.ws_xpixel) / f64::from(size.ws_col)
            } else {
                8.0
            };
            let cell_h = if let Some(area) = measured_area {
                area[1] as f64 / grid_rows as f64
            } else if let Some(cell) = measured_cell {
                cell[1] as f64
            } else if size.ws_ypixel > 0 {
                f64::from(size.ws_ypixel) / f64::from(size.ws_row)
            } else {
                16.0
            };
            return Ok(fit((cols, rows), (cell_w, cell_h), (w, h)));
        }
        Ok((cols, rows))
    }
    type Latest = Mutex<(Option<Frame>, Option<String>, bool)>;
    type FrameCallback =
        unsafe extern "C" fn(*const u8, usize, i32, i32, *const libc::c_char, *mut libc::c_void);
    unsafe extern "C" {
        fn yiterm_capture_run(
            argc: i32,
            argv: *const *const libc::c_char,
            callback: Option<FrameCallback>,
            context: *mut libc::c_void,
        ) -> i32;
        fn yiterm_capture_stop();
        fn yiterm_capture_resize_desktop(w: i32, h: i32, scale: i32);
        fn yiterm_capture_pointer(x: f64, y: f64, button: i32, phase: i32, modifiers: i32);
        fn yiterm_capture_key(code: u32, modifiers: u32);
        fn yiterm_capture_text(bytes: *const u8, len: usize);
    }
    unsafe extern "C" fn captured(
        data: *const u8,
        len: usize,
        w: i32,
        h: i32,
        error: *const libc::c_char,
        context: *mut libc::c_void,
    ) {
        // Swift calls synchronously; copy before it reuses its pixel buffer.
        // `latest` lives until the native run loop and encoder have both stopped.
        let latest = unsafe { &*context.cast::<Latest>() };
        let Ok(mut slot) = latest.lock() else { return };
        if !error.is_null() {
            slot.1 = Some(
                unsafe { std::ffi::CStr::from_ptr(error) }
                    .to_string_lossy()
                    .into_owned(),
            );
        } else if w == -1 {
            slot.2 = h == 1;
            if slot.2 {
                slot.0 = None;
            }
        } else if !data.is_null() {
            let (w, h) = (w as usize, h as usize);
            if w == 0
                || h == 0
                || w > 3840
                || h > 16384
                || len > 128 * 1024 * 1024
                || len % h != 0
                || len / h < w * 4
                || w * h * 4 > 96 * 1024 * 1024
            {
                slot.1 = Some("Capture frame exceeds memory budget".into());
                return;
            }
            let bytes = unsafe { std::slice::from_raw_parts(data, len) };
            let stride = len / h;
            let pixels = if stride == w * 4 {
                bytes.to_vec()
            } else {
                let mut packed = Vec::with_capacity(w * h * 4);
                for row in bytes.chunks_exact(stride).take(h) {
                    packed.extend_from_slice(&row[..w * 4]);
                }
                packed
            };
            slot.0 = Some(Frame { w, h, pixels });
        }
    }

    pub fn run(o: &Options) -> Result<()> {
        if !o.share_list {
            anyhow::ensure!(
                std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
                "Screen sharing requires a terminal; connect with ssh -t"
            );
            geometry(None, None, None, None)?;
        }
        if super::unlock::locked() {
            anyhow::ensure!(
                !o.share_list,
                "Mac is locked; run --share-screen in an interactive SSH terminal to unlock before listing sources"
            );
            super::unlock::submit_once(o.share_read_only)?;
        }
        let mut args = Vec::new();
        for (flag, value) in [("--display", o.share_display), ("--window", o.share_window)] {
            if let Some(value) = value {
                args.extend([flag.to_owned(), value.to_string()]);
            }
        }
        if let Some(id) = std::env::var_os("YITERM_VIRTUAL_DISPLAY_ID") {
            let id = id.to_str().and_then(|s| s.parse::<u32>().ok())
                .filter(|id| *id != 0).ok_or_else(|| anyhow::anyhow!("Invalid virtual display identity"))?;
            args.extend(["--virtual-display-id".into(), id.to_string()]);
        }
        args.extend([
            "--width".into(),
            o.share_width.to_string(),
            "--fps".into(),
            o.share_fps.to_string(),
        ]);
        if o.share_list {
            args.push("--list".into());
        }
        if !o.share_read_only {
            args.push("--control".into());
        }
        let args: Vec<_> = args
            .into_iter()
            .map(std::ffi::CString::new)
            .collect::<std::result::Result<_, _>>()?;
        let pointers: Vec<_> = args.iter().map(|s| s.as_ptr()).collect();
        let latest = Arc::new(Mutex::new((None, None, false)));
        let context = Arc::as_ptr(&latest).cast_mut().cast::<libc::c_void>();
        std::thread::scope(|scope| -> Result<()> {
            let encoder = if o.share_list {
                None
            } else {
                let latest = latest.clone();
                Some(scope.spawn(move || {
                    let result = render(o, latest);
                    unsafe {
                        yiterm_capture_stop();
                    }
                    result
                }))
            };
            // This is the original process main thread, not a spawned executable.
            let status = unsafe {
                yiterm_capture_run(
                    pointers.len() as i32,
                    pointers.as_ptr(),
                    Some(captured),
                    context,
                )
            };
            unsafe {
                libc::fflush(std::ptr::null_mut());
            }
            STOP.store(true, Ordering::Relaxed);
            if let Some(encoder) = encoder {
                encoder
                    .join()
                    .map_err(|_| anyhow::anyhow!("Screen encoder panicked"))??;
            }
            if status != 0 {
                let error = latest
                    .lock()
                    .unwrap()
                    .1
                    .take()
                    .unwrap_or_else(|| "Screen capture failed".into());
                bail!("{error}");
            }
            Ok(())
        })
    }
    #[derive(Clone, Default)]
    struct InputView {
        focused: bool,
        ready: bool,
        canvas: Option<bool>,
        source: (usize, usize),
        geo: (usize, usize),
        origin: (usize, usize),
        grid: Option<[usize; 2]>,
        staged_area: Option<[usize; 2]>,
        staged_cell: Option<[usize; 2]>,
        cell: Option<[usize; 2]>,
        area: Option<[usize; 2]>,
    }
    struct InputPump {
        view: Arc<Mutex<InputView>>,
        gate: Arc<Mutex<()>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
        reset: Arc<std::sync::atomic::AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }
    impl InputPump {
        fn new(read_only: bool) -> Self {
            let view = Arc::new(Mutex::new(InputView {
                focused: true,
                ..Default::default()
            }));
            let gate = Arc::new(Mutex::new(()));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let reset = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (v, g, s, r) = (view.clone(), gate.clone(), stop.clone(), reset.clone());
            let worker = std::thread::spawn(move || {
                let mut decoder = super::input::Decoder::new();
                while !s.load(Ordering::Relaxed) && !STOP.load(Ordering::Relaxed) {
                    {
                        let _gate = g.lock().unwrap();
                        if r.swap(false, Ordering::Relaxed) {
                            decoder = super::input::Decoder::new();
                        }
                        let mut poll = libc::pollfd {
                            fd: 0,
                            events: libc::POLLIN,
                            revents: 0,
                        };
                        unsafe {
                            libc::poll(&mut poll, 1, 5);
                        }
                        if poll.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                            STOP.store(true, Ordering::Relaxed);
                            break;
                        }
                        if poll.revents & libc::POLLIN != 0 {
                            let mut bytes = [0; 4096];
                            match std::io::stdin().read(&mut bytes) {
                                Ok(n) => decoder.push(&bytes[..n]),
                                Err(_) => {
                                    STOP.store(true, Ordering::Relaxed);
                                    break;
                                }
                            }
                        }
                        while let Some(event) = decoder.next() {
                            use super::input::Event;
                            let state = v.lock().unwrap().clone();
                            let control = !read_only && state.focused && state.ready;
                            match event {
                                Event::Exit => STOP.store(true, Ordering::Relaxed),
                                Event::Text(text) | Event::Paste(text) if read_only => {
                                    if text.contains('q') {
                                        STOP.store(true, Ordering::Relaxed);
                                    }
                                }
                                Event::Text(text) | Event::Paste(text) if control => unsafe {
                                    yiterm_capture_text(text.as_ptr(), text.len());
                                },
                                Event::Key(code, mods) if control => unsafe {
                                    yiterm_capture_key(code, mods);
                                },
                                Event::Focus(focused) => {
                                    v.lock().unwrap().focused = focused;
                                    if !focused {
                                        unsafe {
                                            yiterm_capture_pointer(0., 0., 0, 3, 0);
                                        }
                                    }
                                }
                                Event::DesktopSize(w, h, scale) => unsafe {
                                    yiterm_capture_resize_desktop(w as i32, h as i32, scale as i32);
                                },
                                Event::Canvas(supported) => {
                                    v.lock().unwrap().canvas = Some(supported)
                                }
                                Event::Size(kind, w, h) => {
                                    let limit = if kind == 4 {
                                        65535
                                    } else if kind == 8 {
                                        4096
                                    } else {
                                        512
                                    };
                                    if (1..=limit).contains(&w) && (1..=limit).contains(&h) {
                                        let mut view = v.lock().unwrap();
                                        match kind {
                                            4 => {
                                                view.staged_area = Some([w, h]);
                                                if view.grid.is_none() {
                                                    view.area = Some([w, h]);
                                                }
                                            }
                                            6 => {
                                                view.staged_cell = Some([w, h]);
                                                if view.grid.is_none() {
                                                    view.cell = Some([w, h]);
                                                }
                                            }
                                            8 => {
                                                // Publish pixel area, cell pitch and cell count
                                                // from one ordered query batch together.
                                                view.grid = Some([w, h]);
                                                if let Some(area) = view.staged_area.take() {
                                                    view.area = Some(area);
                                                }
                                                if let Some(cell) = view.staged_cell.take() {
                                                    view.cell = Some(cell);
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                Event::Mouse(button, x, y, pressed) if control => {
                                    let wheel = super::input::vertical_wheel(button, pressed);
                                    if wheel.is_none() && button & !(3 | 28 | 32) != 0 {
                                        continue;
                                    }
                                    if let Some([cw, ch]) = state.cell {
                                        if let Ok((cols, rows)) =
                                            geometry(None, None, None, state.grid)
                                        {
                                            let (cw, ch) = state
                                                .area
                                                .map(|a| {
                                                    (
                                                        a[0] as f64 / cols as f64,
                                                        a[1] as f64 / rows as f64,
                                                    )
                                                })
                                                .unwrap_or((cw as f64, ch as f64));
                                            let (box_w, box_h) =
                                                (state.geo.0 as f64 * cw, state.geo.1 as f64 * ch);
                                            let (sw, sh) = state.source;
                                            if sw == 0 || sh == 0 {
                                                continue;
                                            }
                                            let scale = if state.canvas == Some(true) {
                                                (box_w / sw as f64).min(box_h / sh as f64)
                                            } else {
                                                box_w / sw as f64
                                            };
                                            let (w, h) = (sw as f64 * scale, sh as f64 * scale);
                                            let local_x =
                                                x as f64 - 0.5 - state.origin.0 as f64 * cw;
                                            let local_y =
                                                y as f64 - 0.5 - state.origin.1 as f64 * ch;
                                            let motion = button & 32 != 0;
                                            let phase = if wheel.is_some() {
                                                4
                                            } else if motion {
                                                0
                                            } else if pressed {
                                                1
                                            } else {
                                                2
                                            };
                                            if w > 0.
                                                && h > 0.
                                                && (phase == 2
                                                    || (local_x >= 0.
                                                        && local_y >= 0.
                                                        && local_x < w
                                                        && local_y < h))
                                            {
                                                unsafe {
                                                    yiterm_capture_pointer(
                                                        (local_x / w).clamp(0., 0.999999),
                                                        (local_y / h).clamp(0., 0.999999),
                                                        wheel.unwrap_or((button & 3) as i32),
                                                        phase,
                                                        (button & 28) as i32,
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            Self {
                view,
                gate,
                stop,
                reset,
                worker: Some(worker),
            }
        }
    }
    impl Drop for InputPump {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            unsafe {
                yiterm_capture_pointer(0., 0., 0, 3, 0);
            }
        }
    }

    fn render(o: &Options, latest: Arc<Latest>) -> Result<()> {
        let _terminal = Terminal::enter(o.share_read_only)?;
        let mut out = std::io::stdout();
        let encoders = EncoderPool::new();
        out.write_all(b"\x1b[14t\x1b[16t\x1b[18t\x1b[?9002$p")?;
        if !o.share_read_only {
            out.write_all(b"\x1b[?1003h\x1b[?1006h\x1b[?1016h\x1b[?2004h")?;
        }
        out.flush()?;
        let query_started = Instant::now();
        let mut last_query = Instant::now();
        let mut queried_terminal_size = geometry(None, None, None, None)?;
        let mut previous: Option<Arc<Frame>> = None;
        let mut current: Option<Arc<Frame>> = None;
        let mut old_geometry = (0, 0);
        let mut old_origin = (0, 0);
        let mut count = 0;
        let mut old_grid = (0, 0);
        let mut old_canvas = false;
        let fixed = match o.share_quality.as_deref().unwrap_or("lossless") {
            "auto" => None,
            "565" => Some(1),
            "555" => Some(2),
            "444" => Some(3),
            _ => Some(0),
        };
        let mut quality = fixed.unwrap_or(0);
        let mut last_adjust = Instant::now();
        let mut last_stats = Instant::now();
        let mut last_frame = Instant::now() - Duration::from_secs(1);
        let mut total_bytes = 0usize;
        let mut total_tiles = 0usize;
        let mut fps = o.share_fps;
        let mut scale_div = 1;
        let mut applied_scale = 1;
        let input_pump = InputPump::new(o.share_read_only);
        let mut source: Option<Arc<Frame>> = None;
        let mut unlock_wait: Option<Instant> = None;
        let mut unlock_fresh_since: Option<Instant> = None;
        let result = (|| -> Result<()> {
            while !STOP.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(2));
                let input = input_pump.view.lock().unwrap().clone();
                let focused = input.focused;
                let measured_cell = input.cell;
                let measured_area = input.area;
                let locked = latest.lock().unwrap().2;
                if locked && unlock_wait.is_none() {
                    delete(&mut out, count)?;
                    out.write_all(b"\x1b[2J\x1b[H\x1b[?2004l")?;
                    out.flush()?;
                    previous = None;
                    current = None;
                    source = None;
                    input_pump.view.lock().unwrap().ready = false;
                    {
                        let _exclusive = input_pump.gate.lock().unwrap();
                        super::unlock::submit_once(o.share_read_only)?;
                    }
                    input_pump.reset.store(true, Ordering::Relaxed);
                    unlock_wait = Some(Instant::now());
                    out.write_all(b"\x1b[?2004h")?;
                    out.flush()?;
                }
                if let Some(wait) = unlock_wait {
                    if wait.elapsed() > Duration::from_secs(15) {
                        bail!(
                            "Mac unlock did not restore fresh capture in time; no password resubmitted"
                        );
                    }
                    if locked {
                        unlock_fresh_since = None;
                        continue;
                    }
                    let fresh_since = unlock_fresh_since.get_or_insert_with(Instant::now);
                    if fresh_since.elapsed() < Duration::from_secs(1) {
                        continue;
                    }
                    unlock_wait = None;
                    unlock_fresh_since = None;
                    out.write_all(b"\x1b[2J\x1b[H")?;
                    out.flush()?;
                    input_pump.reset.store(true, Ordering::Relaxed);
                }
                let next_terminal_size = geometry(None, None, None, None)?;
                let query_age = last_query.elapsed();
                // Keep the latest resize pending until the next query can be sent.
                if query_age >= Duration::from_secs(2)
                    || (next_terminal_size != queried_terminal_size
                        && query_age >= Duration::from_millis(100))
                {
                    out.write_all(b"\x1b[14t\x1b[16t\x1b[18t\x1b[?9002$p")?;
                    out.flush()?;
                    last_query = Instant::now();
                    queried_terminal_size = next_terminal_size;
                }
                if (measured_cell.is_none() || input.canvas.is_none())
                    && query_started.elapsed() < Duration::from_millis(750)
                {
                    continue;
                }
                if last_frame.elapsed()
                    < Duration::from_secs_f64(1.0 / f64::from(if focused { fps } else { 1 }))
                {
                    continue;
                }
                let (frame, error) = {
                    let mut slot = latest.lock().unwrap();
                    (slot.0.take(), slot.1.take())
                };
                if let Some(error) = error {
                    bail!("Capture ended: {error}");
                }
                let new_frame = frame.is_some();
                if let Some(frame) = frame {
                    source = Some(Arc::new(frame));
                }
                if new_frame || applied_scale != scale_div {
                    if let Some(source) = source.as_ref() {
                        current = Some(if scale_div == 1 {
                            source.clone()
                        } else {
                            let w = (source.w / scale_div).max(1);
                            let h = (source.h / scale_div).max(1);
                            let mut pixels = vec![0; w * h * 4];
                            for y in 0..h {
                                for x in 0..w {
                                    let from =
                                        ((y * source.h / h) * source.w + x * source.w / w) * 4;
                                    let to = (y * w + x) * 4;
                                    pixels[to..to + 4]
                                        .copy_from_slice(&source.pixels[from..from + 4]);
                                }
                            }
                            Arc::new(Frame { w, h, pixels })
                        });
                        applied_scale = scale_div;
                    }
                }
                let Some(frame) = current.as_ref() else {
                    continue;
                };
                let geo = geometry(
                    Some((frame.w, frame.h)),
                    measured_cell,
                    measured_area,
                    input.grid,
                )?;
                let canvas = input.canvas == Some(true);
                let grid = if canvas { layout(geo.0, geo.1) } else { (1, 1) };
                let (available_cols, available_rows) = geometry(None, None, None, input.grid)?;
                let origin = (
                    available_cols.saturating_sub(geo.0) / 2,
                    available_rows.saturating_sub(geo.1) / 2,
                );
                let layout_changed = geo != old_geometry || origin != old_origin;
                if layout_changed && o.share_stats {
                    eprintln!(
                        "share layout: source={}x{} viewer_grid={}x{} viewport={:?} cell={:?} picture_cells={}x{} origin={},{}",
                        frame.w,
                        frame.h,
                        available_cols,
                        available_rows,
                        measured_area,
                        measured_cell,
                        geo.0,
                        geo.1,
                        origin.0,
                        origin.1
                    );
                }
                let reset = canvas != old_canvas
                    || grid != old_grid
                    || previous
                        .as_ref()
                        .is_none_or(|p| p.w != frame.w || p.h != frame.h);
                if !new_frame && !reset && !layout_changed {
                    let mut view = input_pump.view.lock().unwrap();
                    view.geo = geo;
                    view.source = (frame.w, frame.h);
                    view.origin = origin;
                    view.ready = true;
                    continue;
                }
                let started = Instant::now();
                if reset {
                    delete(&mut out, count)?;
                    previous = None;
                }
                old_geometry = geo;
                old_origin = origin;
                old_grid = grid;
                old_canvas = canvas;
                let (nx, ny) = grid;
                if layout_changed && !new_frame && !reset {
                    // A pure viewport resize only reuses cached image IDs.
                    let mut placements = Vec::with_capacity(nx * ny * 64);
                    placements.extend_from_slice(b"\x1b[?2026h");
                    for iy in 0..ny {
                        for ix in 0..nx {
                            let tile = tile_layout(
                                iy * nx + ix + 1,
                                ix,
                                iy,
                                grid,
                                geo,
                                origin,
                                (frame.w, frame.h),
                                canvas,
                            );
                            place(&mut placements, &tile)?;
                        }
                    }
                    placements.extend_from_slice(b"\x1b[?2026l");
                    out.write_all(&placements)?;
                    out.flush()?;
                    let mut view = input_pump.view.lock().unwrap();
                    view.geo = geo;
                    view.source = (frame.w, frame.h);
                    view.origin = origin;
                    view.ready = true;
                    continue;
                }
                let dirty_tiles = dirty_tiles(frame, previous.as_deref(), grid);
                let mut tiles = Vec::new();
                let mut id = 0;
                for iy in 0..ny {
                    for ix in 0..nx {
                        id += 1;
                        let x = ix * frame.w / nx;
                        let y = iy * frame.h / ny;
                        let w = (ix + 1) * frame.w / nx - x;
                        let h = (iy + 1) * frame.h / ny - y;
                        if w == 0 || h == 0 {
                            continue;
                        }
                        let dirty = dirty_tiles[iy * nx + ix];
                        if !dirty {
                            if layout_changed {
                                let tile = tile_layout(
                                    id,
                                    ix,
                                    iy,
                                    grid,
                                    geo,
                                    origin,
                                    (frame.w, frame.h),
                                    canvas,
                                );
                                place(&mut out, &tile)?;
                            }
                            continue;
                        }
                        let mut tile =
                            tile_layout(id, ix, iy, grid, geo, origin, (frame.w, frame.h), canvas);
                        tile.rgb = frame.tile_rgb(x, y, w, h);
                        tiles.push(tile);
                    }
                }
                count = id;
                // At most four workers and four compressed tiles resident at once.
                for batch in tiles.chunks_mut(4) {
                    let encoded = encoders.batch(batch, quality)?;
                    out.write_all(b"\x1b[?2026h")?;
                    for image in encoded {
                        total_bytes += send(&mut out, image)?;
                        total_tiles += 1;
                    }
                    out.write_all(b"\x1b[?2026l")?;
                    out.flush()?;
                    if STOP.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                }
                out.flush()?;
                // Commit only after every dirty tile was written successfully.
                previous = current.clone();
                {
                    let mut view = input_pump.view.lock().unwrap();
                    view.geo = geo;
                    view.source = (frame.w, frame.h);
                    view.origin = origin;
                    view.ready = true;
                }
                let elapsed = started.elapsed();
                let budget = Duration::from_secs_f64(1.0 / f64::from(o.share_fps));
                if elapsed > budget * 2 && last_adjust.elapsed() > Duration::from_secs(2) {
                    if fixed.is_none() && quality < 3 {
                        quality += 1;
                        previous = None;
                    } else if fps > 3 {
                        fps = (fps * 3 / 4).max(1);
                    } else if o.share_adaptive_resolution && scale_div < 4 {
                        scale_div += 1;
                    }
                    last_adjust = Instant::now();
                } else if elapsed < budget && last_adjust.elapsed() > Duration::from_secs(15) {
                    if scale_div > 1 {
                        scale_div -= 1;
                    } else if fps < o.share_fps {
                        fps = (fps + 1).min(o.share_fps);
                    } else if fixed.is_none() && quality > 0 {
                        quality -= 1;
                        previous = None;
                    }
                    last_adjust = Instant::now();
                }
                if o.share_stats && last_stats.elapsed() >= Duration::from_secs(5) {
                    eprintln!(
                        "share: base64_bytes={total_bytes} tiles={total_tiles} encode_write_ms={} fps={fps} quality={quality} scale_div={scale_div} frame_bytes={} latest_queue<=1",
                        elapsed.as_millis(),
                        previous.as_ref().map_or(0, |p| p.pixels.len())
                    );
                    last_stats = Instant::now();
                }
                last_frame = Instant::now();
            }
            Ok(())
        })();
        let _ = delete(&mut out, count);
        let _ = out.flush();
        result
    }
}
