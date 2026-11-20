use super::{
    Options,
    graphics::*,
    input::{Decoder, Event},
};
use anyhow::{Context, Result, bail, ensure};
use portable_pty::PtySize;
use std::{
    io, mem,
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, RECT},
    Graphics::Gdi::*,
    System::{StationsAndDesktops::*, Threading::GetCurrentThreadId},
    UI::{
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext},
        WindowsAndMessaging::*,
    },
};
use windows_sys::core::BOOL;
use zeroize::Zeroizing;

mod control;

pub fn require_user_desktop() -> Result<()> {
    let mut name = [0u16; 32];
    let ok = unsafe {
        GetUserObjectInformationW(
            GetThreadDesktop(GetCurrentThreadId()),
            UOI_NAME,
            name.as_mut_ptr().cast(),
            mem::size_of_val(&name) as u32,
            null_mut(),
        )
    };
    let len = name
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(name.len());
    ensure!(
        ok != 0 && name[..len].iter().copied().eq("Default".encode_utf16()),
        "Windows sharing requires the user's interactive desktop"
    );
    check_desktop()
}

fn check_desktop() -> Result<()> {
    let mut active: BOOL = 0;
    let ok = unsafe {
        GetUserObjectInformationW(
            GetThreadDesktop(GetCurrentThreadId()),
            UOI_IO,
            (&mut active as *mut BOOL).cast(),
            mem::size_of::<BOOL>() as u32,
            null_mut(),
        )
    };
    ensure!(
        ok != 0 && active != 0,
        "The Windows desktop is locked or unavailable"
    );
    Ok(())
}

struct Monitor {
    handle: HMONITOR,
    id: u32,
    rect: RECT,
    primary: bool,
}

fn monitor(handle: HMONITOR) -> Option<Monitor> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = mem::size_of::<MONITORINFOEXW>() as u32;
    if unsafe { GetMonitorInfoW(handle, &mut info.monitorInfo) } == 0 {
        return None;
    }
    let len = info.szDevice.iter().position(|&c| c == 0)?;
    let name = String::from_utf16_lossy(&info.szDevice[..len]);
    let id = name.rsplit_once("DISPLAY")?.1.parse().ok()?;
    Some(Monitor {
        handle,
        id,
        rect: info.monitorInfo.rcMonitor,
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    })
}

fn monitors() -> Result<Vec<Monitor>> {
    unsafe extern "system" fn collect(
        handle: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let monitors = unsafe { &mut *(data as *mut Vec<Monitor>) };
        if let Some(monitor) = monitor(handle) {
            monitors.push(monitor);
        }
        1
    }
    let mut monitors = Vec::new();
    let ok = unsafe {
        EnumDisplayMonitors(
            null_mut(),
            null(),
            Some(collect),
            &mut monitors as *mut _ as LPARAM,
        )
    };
    ensure!(ok != 0, "Cannot enumerate Windows displays");
    Ok(monitors)
}

struct Window {
    handle: HWND,
    thread: u32,
    process: u32,
    title: String,
}

fn top_level_windows() -> Result<Vec<Window>> {
    unsafe extern "system" fn collect(handle: HWND, data: LPARAM) -> BOOL {
        if unsafe { IsWindowVisible(handle) } == 0 || unsafe { IsIconic(handle) } != 0 {
            return 1;
        }
        let mut title = [0; 512];
        let len = unsafe { GetWindowTextW(handle, title.as_mut_ptr(), title.len() as i32) };
        let mut process = 0;
        let thread = unsafe { GetWindowThreadProcessId(handle, &mut process) };
        if len > 0 && thread != 0 {
            let windows = unsafe { &mut *(data as *mut Vec<Window>) };
            windows.push(Window {
                handle,
                thread,
                process,
                title: String::from_utf16_lossy(&title[..len as usize])
                    .replace(|c: char| c.is_control(), " "),
            });
        }
        1
    }
    let mut windows = Vec::new();
    let ok = unsafe { EnumWindows(Some(collect), &mut windows as *mut _ as LPARAM) };
    ensure!(ok != 0, "Cannot enumerate Windows windows");
    Ok(windows)
}

pub fn list_sources() -> Result<()> {
    require_user_desktop()?;
    for display in monitors()? {
        let (w, h) = dimensions(display.rect)?;
        let primary = if display.primary { " (primary)" } else { "" };
        println!("Display {}: {w}x{h}{primary}", display.id);
    }
    for window in top_level_windows()? {
        println!("Window {}: {}", window.handle as u32, window.title);
    }
    Ok(())
}

enum Source {
    Display(Monitor),
    Window(Window),
}

impl Source {
    fn select(options: &Options) -> Result<Self> {
        if let Some(id) = options.share_window {
            let window = top_level_windows()?
                .into_iter()
                .find(|window| window.handle as u32 == id)
                .with_context(|| {
                    format!("Window {id} is unavailable. Run --share-screen --share-list")
                })?;
            return Ok(Self::Window(window));
        }
        let display = monitors()?
            .into_iter()
            .find(|display| {
                options
                    .share_display
                    .map_or(display.primary, |id| display.id == id)
            })
            .context("Requested Windows display is unavailable. Run --share-screen --share-list")?;
        Ok(Self::Display(display))
    }

    fn bounds(&self) -> Result<RECT> {
        match self {
            Self::Display(display) => monitor(display.handle)
                .filter(|current| current.id == display.id)
                .map(|current| current.rect)
                .context("Shared display is no longer available"),
            Self::Window(window) => {
                let mut process = 0;
                let mut rect = RECT::default();
                let thread = unsafe { GetWindowThreadProcessId(window.handle, &mut process) };
                ensure!(
                    thread == window.thread
                        && process == window.process
                        && unsafe { IsWindowVisible(window.handle) } != 0
                        && unsafe { IsIconic(window.handle) } == 0
                        && unsafe { GetWindowRect(window.handle, &mut rect) } != 0,
                    "Shared window was closed, hidden or minimized"
                );
                Ok(rect)
            }
        }
    }
}

fn dimensions(rect: RECT) -> Result<(usize, usize)> {
    let w = i64::from(rect.right) - i64::from(rect.left);
    let h = i64::from(rect.bottom) - i64::from(rect.top);
    ensure!(
        (1..=16384).contains(&w) && (1..=16384).contains(&h),
        "Unsupported Windows capture dimensions"
    );
    Ok((w as usize, h as usize))
}

struct ScreenDc(HDC);

impl ScreenDc {
    fn open() -> Result<Self> {
        let dc = unsafe { GetDC(null_mut()) };
        ensure!(!dc.is_null(), "Cannot open the Windows desktop for capture");
        Ok(Self(dc))
    }
}

impl Drop for ScreenDc {
    fn drop(&mut self) {
        unsafe { ReleaseDC(null_mut(), self.0) };
    }
}

struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    original: HGDIOBJ,
    bits: *mut u8,
    size: (usize, usize),
}

impl Surface {
    fn new(screen: &ScreenDc, size: (usize, usize)) -> Result<Self> {
        let (w, h) = size;
        ensure!(
            w * h * 4 <= 96 * 1024 * 1024,
            "Capture frame exceeds memory budget"
        );
        let mut surface = Self {
            dc: unsafe { CreateCompatibleDC(screen.0) },
            bitmap: null_mut(),
            original: null_mut(),
            bits: null_mut(),
            size,
        };
        ensure!(
            !surface.dc.is_null(),
            "Cannot create a capture device context"
        );
        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w as i32;
        info.bmiHeader.biHeight = -(h as i32);
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        let mut bits = null_mut();
        surface.bitmap =
            unsafe { CreateDIBSection(screen.0, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0) };
        ensure!(
            !surface.bitmap.is_null() && !bits.is_null(),
            "Cannot allocate a capture bitmap"
        );
        surface.bits = bits.cast();
        surface.original = unsafe { SelectObject(surface.dc, surface.bitmap) };
        ensure!(
            !surface.original.is_null(),
            "Cannot select the capture bitmap"
        );
        Ok(surface)
    }

    fn get(slot: &mut Option<Self>, screen: &ScreenDc, size: (usize, usize)) -> Result<()> {
        if slot.as_ref().is_none_or(|surface| surface.size != size) {
            *slot = None;
            *slot = Some(Self::new(screen, size)?);
        }
        Ok(())
    }

    fn scale_from(&self, dc: HDC, x: i32, y: i32, (w, h): (usize, usize), rop: u32) -> Result<()> {
        let ok = unsafe {
            StretchBlt(
                self.dc,
                0,
                0,
                self.size.0 as i32,
                self.size.1 as i32,
                dc,
                x,
                y,
                w as i32,
                h as i32,
                rop,
            )
        };
        ensure!(ok != 0, "Windows could not copy the capture frame");
        Ok(())
    }

    fn frame(&mut self) -> Frame {
        unsafe { GdiFlush() };
        let (w, h) = self.size;
        let mut pixels = unsafe { std::slice::from_raw_parts(self.bits, w * h * 4) }.to_vec();
        // The unused fourth byte is undefined in a 32-bit GDI DIB.
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[3] = 0;
        }
        Frame { w, h, pixels }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            if !self.original.is_null() {
                SelectObject(self.dc, self.original);
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
            if !self.dc.is_null() {
                DeleteDC(self.dc);
            }
        }
    }
}

struct Capture {
    scaled: Option<Surface>,
    window: Option<Surface>,
    screen: ScreenDc,
}

impl Capture {
    fn frame(&mut self, source: &Source, rect: RECT, width: u32, divisor: usize) -> Result<Frame> {
        let native = dimensions(rect)?;
        let (w, h) = native;
        let width = if width == 0 { 3840 } else { width as usize };
        let scale = (width as f64 / w as f64).min(3840.0 / h as f64).min(1.0);
        let size = (
            ((w as f64 * scale) as usize / divisor).max(1),
            ((h as f64 * scale) as usize / divisor).max(1),
        );
        if let Source::Window(window) = source {
            Surface::get(&mut self.window, &self.screen, native)?;
            let surface = self
                .window
                .as_mut()
                .context("Missing window capture surface")?;
            // A window that only paints part of its DC cannot leave old pixels behind.
            unsafe {
                GdiFlush();
                surface.bits.write_bytes(0, w * h * 4);
            }
            let ok = unsafe {
                SendMessageTimeoutW(
                    window.handle,
                    WM_PRINT,
                    surface.dc as usize,
                    (PRF_CHECKVISIBLE | PRF_NONCLIENT | PRF_CLIENT | PRF_ERASEBKGND | PRF_CHILDREN)
                        as LPARAM,
                    SMTO_BLOCK | SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
                    200,
                    null_mut(),
                )
            };
            ensure!(ok != 0, "Shared window did not respond to capture");
            if size == native {
                return Ok(surface.frame());
            }
            Surface::get(&mut self.scaled, &self.screen, size)?;
            let scaled = self
                .scaled
                .as_mut()
                .context("Missing scaled capture surface")?;
            scaled.scale_from(surface.dc, 0, 0, native, SRCCOPY)?;
            return Ok(scaled.frame());
        }
        Surface::get(&mut self.scaled, &self.screen, size)?;
        let scaled = self
            .scaled
            .as_mut()
            .context("Missing display capture surface")?;
        scaled.scale_from(
            self.screen.0,
            rect.left,
            rect.top,
            native,
            SRCCOPY | CAPTUREBLT,
        )?;
        Ok(scaled.frame())
    }
}

#[derive(Clone, Copy, PartialEq)]
struct View {
    canvas: bool,
    cells: (usize, usize),
    origin: (usize, usize),
    cell: (f64, f64),
    source: (usize, usize),
}

impl View {
    fn new(size: PtySize, canvas: bool, source: (usize, usize)) -> Self {
        let pitch = |pixels: u16, cells: u16, fallback: f64| {
            if pixels == 0 {
                fallback
            } else {
                f64::from(pixels) / f64::from(cells)
            }
        };
        let cell = (
            pitch(size.pixel_width, size.cols, 8.0),
            pitch(size.pixel_height, size.rows, 16.0),
        );
        let grid = (
            usize::from(size.cols).min(1000),
            usize::from(size.rows).min(500),
        );
        let cells = fit(grid, cell, source);
        let origin = ((grid.0 - cells.0) / 2, (grid.1 - cells.1) / 2);
        Self {
            canvas,
            cells,
            origin,
            cell,
            source,
        }
    }

    fn point(self, x: usize, y: usize, rect: RECT) -> Option<(i32, i32)> {
        let (cw, ch) = self.cell;
        let (sw, sh) = (self.source.0 as f64, self.source.1 as f64);
        let mut scale = self.cells.0 as f64 * cw / sw;
        if self.canvas {
            scale = scale.min(self.cells.1 as f64 * ch / sh);
        }
        let x = (x as f64 - 0.5 - self.origin.0 as f64 * cw) / (sw * scale);
        let y = (y as f64 - 0.5 - self.origin.1 as f64 * ch) / (sh * scale);
        if !(0.0..1.0).contains(&x) || !(0.0..1.0).contains(&y) {
            return None;
        }
        Some((
            rect.left + (x * f64::from(rect.right - rect.left)) as i32,
            rect.top + (y * f64::from(rect.bottom - rect.top)) as i32,
        ))
    }
}

struct Renderer {
    encoders: EncoderPool,
    previous: Option<Frame>,
    previous_view: Option<View>,
    previous_grid: (usize, usize),
}

impl Renderer {
    fn new() -> Self {
        Self {
            encoders: EncoderPool::new(),
            previous: None,
            previous_view: None,
            previous_grid: (0, 0),
        }
    }

    fn frame(
        &mut self,
        frame: Frame,
        view: View,
        quality: usize,
        output: &mpsc::Sender<Vec<u8>>,
    ) -> Result<()> {
        let (w, h) = view.source;
        let grid = if view.canvas {
            layout(view.cells.0.min(w), view.cells.1.min(h))
        } else {
            (1, 1)
        };
        let (nx, ny) = grid;
        let reset = grid != self.previous_grid
            || self
                .previous_view
                .is_none_or(|old| old.source != view.source || old.canvas != view.canvas);
        let moved = self.previous_view != Some(view);
        let mut out = Vec::new();
        if reset {
            delete(&mut out, self.previous_grid.0 * self.previous_grid.1)?;
            self.previous = None;
        }
        let dirty = dirty_tiles(&frame, self.previous.as_ref(), grid);
        let mut batch = Vec::with_capacity(4);
        for (index, needs_encoding) in dirty.into_iter().enumerate() {
            let (ix, iy) = (index % nx, index / nx);
            let mut tile = tile_layout(
                index + 1,
                ix,
                iy,
                grid,
                view.cells,
                view.origin,
                view.source,
                view.canvas,
            );
            if needs_encoding {
                let (x, y) = (ix * w / nx, iy * h / ny);
                tile.rgb = frame.tile_rgb(x, y, (ix + 1) * w / nx - x, (iy + 1) * h / ny - y);
                batch.push(tile);
                if batch.len() == 4 {
                    self.flush(&mut batch, quality, &mut out, output)?;
                }
            } else if moved {
                place(&mut out, &tile)?;
            }
        }
        self.flush(&mut batch, quality, &mut out, output)?;
        self.previous = Some(frame);
        self.previous_view = Some(view);
        self.previous_grid = grid;
        Ok(())
    }

    fn flush(
        &self,
        batch: &mut Vec<Tile>,
        quality: usize,
        out: &mut Vec<u8>,
        output: &mpsc::Sender<Vec<u8>>,
    ) -> Result<()> {
        if !batch.is_empty() {
            out.extend_from_slice(b"\x1b[?2026h");
            for image in self.encoders.batch(batch, quality)? {
                send(out, image)?;
            }
            out.extend_from_slice(b"\x1b[?2026l");
            batch.clear();
        }
        if !out.is_empty() {
            output
                .blocking_send(mem::take(out))
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        }
        Ok(())
    }
}

pub fn stream(
    options: &Options,
    mut sizes: watch::Receiver<PtySize>,
    mut input: mpsc::Receiver<Zeroizing<Vec<u8>>>,
    output: &mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    // This worker owns the capture DCs and uses physical coordinates throughout.
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    require_user_desktop()?;
    let source = Source::select(options)?;
    let mut capture = Capture {
        screen: ScreenDc::open()?,
        scaled: None,
        window: None,
    };
    let mut renderer = Renderer::new();
    let mut decoder = Decoder::new();
    let mut control = control::Control::default();
    let mut focused = true;
    let mut canvas = None;
    let mut shown = None;
    let fixed = match options.share_quality.as_deref() {
        Some("auto") => None,
        Some("565") => Some(1),
        Some("555") => Some(2),
        Some("444") => Some(3),
        _ => Some(0),
    };
    let mut quality = fixed.unwrap_or(0);
    let mut divisor = 1;
    let mut fps = options.share_fps;
    let period = Duration::from_secs_f64(1.0 / f64::from(fps));
    let started = Instant::now();
    let mut last_change = started;
    let mut next_frame = started;
    output
        .blocking_send(b"\x1b[?1049h\x1b[?25l\x1b[?1004h\x1b[2J\x1b[H\x1b[?9002$p".to_vec())
        .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
    if !options.share_read_only {
        output
            .blocking_send(b"\x1b[?1003h\x1b[?1006h\x1b[?1016h\x1b[?2004h".to_vec())
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
    }
    while !output.is_closed() {
        check_desktop()?;
        for _ in 0..4 {
            let Ok(bytes) = input.try_recv() else { break };
            decoder.push(&bytes);
        }
        while let Some(event) = decoder.next() {
            if output.is_closed() {
                return Ok(());
            }
            match event {
                Event::Exit => return Ok(()),
                Event::Focus(value) => {
                    focused = value;
                    if !focused {
                        control.release();
                    }
                }
                Event::Canvas(value) => canvas = Some(value),
                Event::Text(text) if options.share_read_only && text.contains('q') => return Ok(()),
                event if !options.share_read_only && focused => {
                    if let Some(view) = shown {
                        control.handle(event, &source, view, || output.is_closed())?;
                    }
                }
                _ => {}
            }
        }
        let resized = sizes.has_changed().unwrap_or(false);
        if (resized || Instant::now() >= next_frame)
            && (canvas.is_some() || started.elapsed() >= Duration::from_millis(300))
        {
            let size = *sizes.borrow_and_update();
            let rect = source.bounds()?;
            let tick = Instant::now();
            let frame = capture.frame(&source, rect, options.share_width, divisor)?;
            check_desktop()?;
            let view = View::new(size, canvas.unwrap_or(false), (frame.w, frame.h));
            renderer.frame(frame, view, quality, output)?;
            shown = Some(view);
            let elapsed = tick.elapsed();
            if elapsed > period * 2 && last_change.elapsed() >= Duration::from_secs(2) {
                if fixed.is_none() && quality < 3 {
                    quality += 1;
                    renderer.previous = None;
                } else if fps > 3 {
                    fps = (fps * 3 / 4).max(1);
                } else if options.share_adaptive_resolution && divisor < 4 {
                    divisor += 1;
                }
                last_change = Instant::now();
            } else if elapsed < period && last_change.elapsed() >= Duration::from_secs(15) {
                if divisor > 1 {
                    divisor -= 1;
                } else if fps < options.share_fps {
                    fps += 1;
                } else if fixed.is_none() && quality > 0 {
                    quality -= 1;
                    renderer.previous = None;
                }
                last_change = Instant::now();
            }
            next_frame =
                tick + Duration::from_secs_f64(1.0 / f64::from(if focused { fps } else { 1 }));
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    Ok(())
}
