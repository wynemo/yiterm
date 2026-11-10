//! Read-only macOS capture over the existing terminal byte stream.
use anyhow::{Context, Result, bail};
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
    /// Maximum capture width; does not change the terminal display area.
    #[arg(long, requires = "share_screen", default_value = "1280", value_parser = clap::value_parser!(u32).range(160..=3840))]
    pub share_width: u32,
    #[arg(long, requires = "share_screen", default_value = "15", value_parser = clap::value_parser!(u32).range(1..=60))]
    pub share_fps: u32,
    /// Fix color quality instead of adapting to output pressure.
    #[arg(long, requires = "share_screen", value_parser = ["lossless", "565", "555", "444"])]
    pub share_quality: Option<String>,
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
    use sha2::{Digest, Sha256};
    use std::{
        io::{IsTerminal, Read, Write},
        os::unix::fs::PermissionsExt,
        process::{Child, Command, Stdio},
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
    struct CaptureChild(Child);
    impl Drop for CaptureChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
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
            let _ = out.write_all(b"\x1b[?2026l\x1b[?1004l\x1b[?25h\x1b[?1049l");
            let _ = out.flush();
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &self.saved);
                for &(s, handler) in &self.signals {
                    libc::signal(s, handler);
                }
            }
        }
    }
    fn helper() -> Result<std::path::PathBuf> {
        let bytes = include_bytes!(concat!(env!("OUT_DIR"), "/yiterm-screen-capture"));
        let hash = format!("{:x}", Sha256::digest(bytes));
        let dir = directories::BaseDirs::new()
            .context("No user cache directory")?
            .cache_dir()
            .join("yiterm/screen-capture");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("capture-{}", &hash[..16]));
        if !path.exists() {
            let mut file = tempfile::NamedTempFile::new_in(&dir)?;
            file.write_all(bytes)?;
            file.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o700))?;
            file.persist(&path).map_err(|e| e.error)?;
        }
        Ok(path)
    }
    struct Frame {
        w: usize,
        h: usize,
        rgb: Vec<u8>,
    }
    fn read_frame(input: &mut impl Read) -> Result<Frame> {
        let mut header = [0; 8];
        input.read_exact(&mut header)?;
        let w = u32::from_le_bytes(header[..4].try_into()?) as usize;
        let h = u32::from_le_bytes(header[4..].try_into()?) as usize;
        anyhow::ensure!(
            w > 0 && h > 0 && w <= 3840 && h <= 16384 && w * h * 3 <= 64 * 1024 * 1024,
            "Capture frame exceeds memory budget"
        );
        let mut rgb = vec![0; w * h * 3];
        input.read_exact(&mut rgb)?;
        Ok(Frame { w, h, rgb })
    }
    fn geometry(source: Option<(usize, usize)>) -> Result<(usize, usize)> {
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
            let cell_w = if size.ws_xpixel > 0 {
                f64::from(size.ws_xpixel) / f64::from(size.ws_col)
            } else {
                8.0
            };
            let cell_h = if size.ws_ypixel > 0 {
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
    pub fn run(o: &Options) -> Result<()> {
        let mut cmd = Command::new(helper()?);
        for (flag, value) in [("--display", o.share_display), ("--window", o.share_window)] {
            if let Some(value) = value {
                cmd.arg(flag).arg(value.to_string());
            }
        }
        cmd.args([
            "--width",
            &o.share_width.to_string(),
            "--fps",
            &o.share_fps.to_string(),
        ]);
        if o.share_list {
            let status = cmd.arg("--list").status()?;
            anyhow::ensure!(
                status.success(),
                "Cannot list capture sources (see permission / desktop error above)"
            );
            return Ok(());
        }
        anyhow::ensure!(
            std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            "Screen sharing requires a terminal; connect with ssh -t"
        );
        geometry(None)?;
        let mut child = CaptureChild(cmd.stdin(Stdio::null()).stdout(Stdio::piped()).spawn()?);
        let mut pipe = child.0.stdout.take().context("Missing capture pipe")?;
        // The reader always drains capture and replaces one latest-frame slot. Dropped
        // frames never advance `previous`, which represents fully written terminal state.
        let latest = Arc::new(Mutex::new((None, None)));
        let slot = latest.clone();
        std::thread::spawn(move || {
            loop {
                match read_frame(&mut pipe) {
                    Ok(frame) => slot.lock().unwrap().0 = Some(frame),
                    Err(error) => {
                        slot.lock().unwrap().1 = Some(format!("{error:#}"));
                        break;
                    }
                }
            }
        });
        let _terminal = Terminal::enter()?;
        let mut out = std::io::stdout();
        let mut previous: Option<Arc<Frame>> = None;
        let mut current: Option<Arc<Frame>> = None;
        let mut old_geometry = (0, 0);
        let mut count = 0;
        let fixed = o.share_quality.as_deref().map(|s| match s {
            "565" => 1,
            "555" => 2,
            "444" => 3,
            _ => 0,
        });
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
                        if input_pending.starts_with(b"\x1b[I") {
                            focused = true;
                            previous = None;
                            input_pending.drain(..3);
                        } else if input_pending.starts_with(b"\x1b[O") {
                            focused = false;
                            input_pending.drain(..3);
                        } else if b"\x1b[I".starts_with(&input_pending)
                            || b"\x1b[O".starts_with(&input_pending)
                        {
                            break;
                        } else {
                            if input_pending.remove(0) == b'q' {
                                STOP.store(true, Ordering::Relaxed);
                            }
                        }
                    }
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
                    bail!(
                        "Capture ended: {error}. Check Screen Recording permission and the remote desktop session"
                    );
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
                let geo = geometry(Some((frame.w, frame.h)))?;
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
                    } else if !o.share_fixed_resolution && scale_div < 4 {
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
