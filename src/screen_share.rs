//! macOS capture over the existing terminal byte stream.
use anyhow::{Result, bail};
use clap::Args;

#[derive(Args, Clone, Debug)]
pub struct Options {
    /// Stream a remote Mac's screen as Kitty graphics (macOS 13+).
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
    /// View without remote mouse clicks or Accessibility permission.
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
    #[cfg(not(target_os = "macos"))]
    {
        let _ = options;
        bail!("Screen capture requires macOS 13 or later; viewing uses normal Kitty graphics")
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use flate2::{Compression, write::ZlibEncoder};
    use std::{
        io::{IsTerminal, Read, Write},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    static STOP: AtomicBool = AtomicBool::new(false);
    extern "C" fn stop(_: libc::c_int) {
        STOP.store(true, Ordering::Relaxed);
    }
    struct Terminal {
        saved: libc::termios,
        signals: Vec<(i32, libc::sighandler_t)>,
    }
    impl Terminal {
        fn enter() -> Result<Self> {
            let mut saved = unsafe { std::mem::zeroed::<libc::termios>() };
            anyhow::ensure!(
                unsafe { libc::tcgetattr(0, &mut saved) } == 0,
                "Cannot read terminal mode"
            );
            let mut mode = saved;
            mode.c_lflag &= !(libc::ICANON | libc::ECHO);
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
                b"\x1b[?2026l\x1b[?1000l\x1b[?1006l\x1b[?1016l\x1b[?1004l\x1b[?25h\x1b[?1049l",
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
    struct Frame {
        w: usize,
        h: usize,
        rgb: Vec<u8>,
    }
    fn geometry(
        source: Option<(usize, usize)>,
        measured_cell: Option<[usize; 2]>,
        measured_area: Option<[usize; 2]>,
    ) -> Result<(usize, usize)> {
        let mut size = unsafe { std::mem::zeroed::<libc::winsize>() };
        anyhow::ensure!(
            unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut size) } == 0
                && size.ws_col > 0
                && size.ws_row > 0,
            "Cannot determine terminal dimensions; use ssh -t"
        );
        let cols = usize::from(size.ws_col).min(1000);
        let rows = usize::from(size.ws_row).min(500);
        if let Some((w, h)) = source {
            // PTYs can omit pixel dimensions; use a conventional 1:2 cell as fallback.
            let cell_w = if let Some(area) = measured_area {
                area[0] as f64 / f64::from(size.ws_col)
            } else if let Some(cell) = measured_cell {
                cell[0] as f64
            } else if size.ws_xpixel > 0 {
                f64::from(size.ws_xpixel) / f64::from(size.ws_col)
            } else {
                8.0
            };
            let cell_h = if let Some(area) = measured_area {
                area[1] as f64 / f64::from(size.ws_row)
            } else if let Some(cell) = measured_cell {
                cell[1] as f64
            } else if size.ws_ypixel > 0 {
                f64::from(size.ws_ypixel) / f64::from(size.ws_row)
            } else {
                16.0
            };
            let scale = (cols as f64 * cell_w / w as f64).min(rows as f64 * cell_h / h as f64);
            return Ok((
                ((w as f64 * scale / cell_w) as usize).max(1).min(cols),
                ((h as f64 * scale / cell_h) as usize).max(1).min(rows),
            ));
        }
        Ok((cols, rows))
    }
    fn delete(out: &mut impl Write, count: usize) -> Result<()> {
        for index in 0..count {
            write!(out, "\x1b_Ga=d,d=I,i={},q=2;\x1b\\", index + 1)?;
        }
        Ok(())
    }
    // Tiles are aligned to character cells. Enlarge tiles if necessary to stay below
    // the receiver's 256-image budget (with room for unrelated terminal graphics).
    fn layout(cols: usize, rows: usize) -> (usize, usize) {
        let (mut cw, mut ch) = (8, 4);
        while cols.div_ceil(cw) * rows.div_ceil(ch) > 240 {
            cw *= 2;
            ch *= 2;
        }
        (cw, ch)
    }
    struct Tile {
        id: usize,
        col: usize,
        row: usize,
        cols: usize,
        rows: usize,
        w: usize,
        h: usize,
        rgb: Vec<u8>,
    }
    struct Encoded {
        tile: Tile,
        data: String,
        compressed: bool,
    }
    fn encode(mut tile: Tile, quality: usize) -> Result<Encoded> {
        if quality > 0 {
            let bits = match quality {
                1 => [5, 6, 5],
                2 => [5, 5, 5],
                3 => [4, 4, 4],
                _ => [8, 8, 8],
            };
            // Preserve flat/UI text tiles even when photographic tiles are quantized.
            let mut colors = std::collections::HashSet::new();
            for pixel in tile.rgb.chunks_exact(3) {
                colors.insert([pixel[0], pixel[1], pixel[2]]);
                if colors.len() > 128 {
                    break;
                }
            }
            let bits = if colors.len() <= 128 { [8, 8, 8] } else { bits };
            for pixel in tile.rgb.chunks_exact_mut(3) {
                for c in 0..3 {
                    let shift = 8 - bits[c];
                    let value = pixel[c] >> shift;
                    pixel[c] = ((u16::from(value) * 255) / ((1u16 << bits[c]) - 1)) as u8;
                }
            }
        }
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&tile.rgb)?;
        let zipped = encoder.finish()?;
        let compressed = zipped.len() < tile.rgb.len();
        let data = STANDARD.encode(if compressed { &zipped } else { &tile.rgb });
        Ok(Encoded {
            tile,
            data,
            compressed,
        })
    }
    fn send(out: &mut impl Write, image: Encoded) -> Result<usize> {
        let t = image.tile;
        write!(out, "\x1b[{};{}H", t.row + 1, t.col + 1)?;
        let chunks = image.data.as_bytes().chunks(4096);
        let n = chunks.len();
        for (index, chunk) in chunks.enumerate() {
            if index == 0 {
                write!(
                    out,
                    "\x1b_Ga=t,f=24,s={},v={},i={},q=2,m={}{};",
                    t.w,
                    t.h,
                    t.id,
                    usize::from(n > 1),
                    if image.compressed { ",o=z" } else { "" }
                )?;
            } else {
                write!(out, "\x1b_Gm={},q=2;", usize::from(index + 1 < n))?;
            }
            out.write_all(chunk)?;
            out.write_all(b"\x1b\\")?;
        }
        write!(
            out,
            "\x1b_Ga=p,i={},p=1,c={},r={},C=1,q=2;\x1b\\",
            t.id, t.cols, t.rows
        )?;
        Ok(image.data.len())
    }
    type Latest = Mutex<(Option<Frame>, Option<String>)>;
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
        fn yiterm_capture_click(x: f64, y: f64, button: i32, modifiers: i32);
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
        } else if !data.is_null() {
            let (w, h) = (w as usize, h as usize);
            if w == 0
                || h == 0
                || w > 3840
                || h > 16384
                || len > 64 * 1024 * 1024
                || len != w * h * 3
            {
                slot.1 = Some("Capture frame exceeds memory budget".into());
                return;
            }
            slot.0 = Some(Frame {
                w,
                h,
                rgb: unsafe { std::slice::from_raw_parts(data, len) }.to_vec(),
            });
        }
    }
    pub fn run(o: &Options) -> Result<()> {
        if !o.share_list {
            anyhow::ensure!(
                std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
                "Screen sharing requires a terminal; connect with ssh -t"
            );
            geometry(None, None, None)?;
        }
        let mut args = Vec::new();
        for (flag, value) in [("--display", o.share_display), ("--window", o.share_window)] {
            if let Some(value) = value {
                args.extend([flag.to_owned(), value.to_string()]);
            }
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
        let latest = Arc::new(Mutex::new((None, None)));
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
    fn render(o: &Options, latest: Arc<Latest>) -> Result<()> {
        let _terminal = Terminal::enter()?;
        let mut out = std::io::stdout();
        out.write_all(b"\x1b[14t\x1b[16t")?;
        if !o.share_read_only {
            out.write_all(b"\x1b[?1000h\x1b[?1006h\x1b[?1016h")?;
        }
        out.flush()?;
        let query_started = Instant::now();
        let mut last_query = Instant::now();
        let mut measured_cell: Option<[usize; 2]> = None;
        let mut measured_area: Option<[usize; 2]> = None;
        let mut previous: Option<Arc<Frame>> = None;
        let mut current: Option<Arc<Frame>> = None;
        let mut old_geometry = (0, 0);
        let mut count = 0;
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
        let mut focused = true;
        let mut input_pending = Vec::new();
        let mut source: Option<Arc<Frame>> = None;
        let result = (|| -> Result<()> {
            while !STOP.load(Ordering::Relaxed) {
                let mut poll = libc::pollfd {
                    fd: 0,
                    events: libc::POLLIN,
                    revents: 0,
                };
                unsafe {
                    libc::poll(&mut poll, 1, 10);
                }
                if poll.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                    break;
                }
                if poll.revents & libc::POLLIN != 0 {
                    let mut input = [0; 128];
                    let n = std::io::stdin().read(&mut input)?;
                    input_pending.extend_from_slice(&input[..n]);
                    while !input_pending.is_empty() {
                        if input_pending[0] != 27 {
                            if input_pending.remove(0) == b'q' {
                                STOP.store(true, Ordering::Relaxed);
                            }
                            continue;
                        }
                        if input_pending.len() == 1 {
                            break;
                        }
                        if input_pending[1] != b'[' {
                            input_pending.drain(..2);
                            continue;
                        }
                        let Some(end) = input_pending
                            .iter()
                            .enumerate()
                            .skip(2)
                            .find_map(|(i, b)| (0x40..=0x7e).contains(b).then_some(i))
                        else {
                            if input_pending.len() > 128 {
                                input_pending.clear();
                            }
                            break;
                        };
                        let sequence: Vec<_> = input_pending.drain(..=end).collect();
                        match sequence.last().copied() {
                            Some(b'I') => {
                                focused = true;
                                previous = None;
                            }
                            Some(b'O') => focused = false,
                            Some(b't') => {
                                if let Ok(text) = std::str::from_utf8(&sequence[2..end]) {
                                    let parts: Vec<_> = text
                                        .split(';')
                                        .filter_map(|n| n.parse::<usize>().ok())
                                        .collect();
                                    if let [4, h, w] = parts.as_slice() {
                                        if (1..=65535).contains(w) && (1..=65535).contains(h) {
                                            let area = [*w, *h];
                                            if measured_area != Some(area) {
                                                previous = None;
                                            }
                                            measured_area = Some(area);
                                        }
                                    }
                                    if let [6, h, w] = parts.as_slice() {
                                        if (1..=512).contains(w) && (1..=512).contains(h) {
                                            let cell = [*w, *h];
                                            if measured_cell != Some(cell) {
                                                previous = None;
                                            }
                                            measured_cell = Some(cell);
                                        }
                                    }
                                }
                            }
                            Some(b'M' | b'm')
                                if !o.share_read_only && sequence.get(2) == Some(&b'<') =>
                            {
                                if sequence[end] == b'M' {
                                    if let Ok(text) = std::str::from_utf8(&sequence[3..end]) {
                                        let parts: Vec<_> = text
                                            .split(';')
                                            .filter_map(|n| n.parse::<usize>().ok())
                                            .collect();
                                        if let [button, x, y] = parts.as_slice() {
                                            if focused
                                                && previous.is_some()
                                                && button & 0b1110_0000 == 0
                                                && button & 3 < 3
                                            {
                                                if let Some([cw, ch]) = measured_cell {
                                                    let (cols, rows) = old_geometry;
                                                    let (all_cols, all_rows) =
                                                        geometry(None, None, None)?;
                                                    let (cw, ch) = measured_area
                                                        .map(|area| {
                                                            (
                                                                area[0] as f64 / all_cols as f64,
                                                                area[1] as f64 / all_rows as f64,
                                                            )
                                                        })
                                                        .unwrap_or((cw as f64, ch as f64));
                                                    let (width, height) =
                                                        (cols as f64 * cw, rows as f64 * ch);
                                                    if *x > 0
                                                        && *y > 0
                                                        && (*x as f64) <= width
                                                        && (*y as f64) <= height
                                                    {
                                                        unsafe {
                                                            yiterm_capture_click(
                                                                (*x as f64 - 0.5) / width as f64,
                                                                (*y as f64 - 0.5) / height as f64,
                                                                (button & 3) as i32,
                                                                (button & 28) as i32,
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if last_query.elapsed() >= Duration::from_secs(2) {
                    out.write_all(b"\x1b[14t\x1b[16t")?;
                    out.flush()?;
                    last_query = Instant::now();
                }
                if measured_cell.is_none() && query_started.elapsed() < Duration::from_millis(750) {
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
                            let mut rgb = vec![0; w * h * 3];
                            for y in 0..h {
                                for x in 0..w {
                                    let from =
                                        ((y * source.h / h) * source.w + x * source.w / w) * 3;
                                    let to = (y * w + x) * 3;
                                    rgb[to..to + 3].copy_from_slice(&source.rgb[from..from + 3]);
                                }
                            }
                            Arc::new(Frame { w, h, rgb })
                        });
                        applied_scale = scale_div;
                    }
                }
                let Some(frame) = current.as_ref() else {
                    continue;
                };
                let geo = geometry(Some((frame.w, frame.h)), measured_cell, measured_area)?;
                let reset = geo != old_geometry
                    || previous
                        .as_ref()
                        .is_none_or(|p| p.w != frame.w || p.h != frame.h);
                if !new_frame && !reset {
                    continue;
                }
                let started = Instant::now();
                if reset {
                    delete(&mut out, count)?;
                    previous = None;
                    old_geometry = geo;
                }
                let (cols, rows) = geo;
                let (cw, ch) = layout(cols, rows);
                let changed_rows: Vec<bool> = (0..frame.h)
                    .map(|y| {
                        previous.as_ref().is_none_or(|p| {
                            p.rgb[y * frame.w * 3..(y + 1) * frame.w * 3]
                                != frame.rgb[y * frame.w * 3..(y + 1) * frame.w * 3]
                        })
                    })
                    .collect();
                let mut tiles = Vec::new();
                let mut id = 0;
                for row in (0..rows).step_by(ch) {
                    for col in (0..cols).step_by(cw) {
                        id += 1;
                        let right = (col + cw).min(cols);
                        let bottom = (row + ch).min(rows);
                        let x = col * frame.w / cols;
                        let y = row * frame.h / rows;
                        let w = right * frame.w / cols - x;
                        let h = bottom * frame.h / rows - y;
                        if w == 0 || h == 0 {
                            continue;
                        }
                        let dirty = previous.as_ref().is_none_or(|p| {
                            (y..y + h).any(|line| {
                                if !changed_rows[line] {
                                    return false;
                                }
                                let start = (line * frame.w + x) * 3;
                                p.rgb[start..start + w * 3] != frame.rgb[start..start + w * 3]
                            })
                        });
                        if !dirty {
                            continue;
                        }
                        let mut rgb = Vec::with_capacity(w * h * 3);
                        for line in y..y + h {
                            let start = (line * frame.w + x) * 3;
                            rgb.extend_from_slice(&frame.rgb[start..start + w * 3]);
                        }
                        tiles.push(Tile {
                            id,
                            col,
                            row,
                            cols: right - col,
                            rows: bottom - row,
                            w,
                            h,
                            rgb,
                        });
                    }
                }
                count = id;
                // At most four workers and four compressed tiles resident at once.
                for batch in tiles.chunks_mut(4) {
                    let encoded = std::thread::scope(|scope| {
                        let jobs: Vec<_> = batch
                            .iter_mut()
                            .map(|tile| {
                                let tile = Tile {
                                    id: tile.id,
                                    col: tile.col,
                                    row: tile.row,
                                    cols: tile.cols,
                                    rows: tile.rows,
                                    w: tile.w,
                                    h: tile.h,
                                    rgb: std::mem::take(&mut tile.rgb),
                                };
                                scope.spawn(move || encode(tile, quality))
                            })
                            .collect();
                        jobs.into_iter()
                            .map(|j| j.join().expect("tile encoder panicked"))
                            .collect::<Result<Vec<_>>>()
                    })?;
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
                // Commit only after every dirty tile was written successfully.
                previous = current.clone();
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
                        previous.as_ref().map_or(0, |p| p.rgb.len())
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
