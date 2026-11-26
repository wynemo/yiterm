use super::{
    Options,
    graphics::*,
    input::{Decoder, Event},
};
use anyhow::Result;
use portable_pty::PtySize;
use std::{
    io, mem,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};
use zeroize::Zeroizing;

pub(super) trait Desktop {
    fn check(&mut self) -> Result<()> {
        Ok(())
    }
    fn changed(&mut self) -> bool {
        false
    }
    fn recoverable(&self) -> bool {
        false
    }
    #[cfg(windows)]
    fn display_scale(&mut self, _scale: u32) {}
    fn viewport(&mut self, _size: PtySize) -> bool {
        false
    }
    fn frame(&mut self, width: u32, divisor: usize) -> Result<Frame>;
    fn input(&mut self, event: Event, view: View, stopped: impl Fn() -> bool) -> Result<()>;
    fn release(&mut self);
}

pub(super) fn capture_size((w, h): (usize, usize), width: u32, divisor: usize) -> (usize, usize) {
    let width = if width == 0 { 3840 } else { width as usize };
    let scale = (width as f64 / w as f64).min(3840.0 / h as f64).min(1.0);
    (
        ((w as f64 * scale) as usize / divisor).max(1),
        ((h as f64 * scale) as usize / divisor).max(1),
    )
}

#[derive(Clone, Copy, PartialEq)]
pub(super) struct View {
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

    pub fn point(
        self,
        x: usize,
        y: usize,
        origin: (i32, i32),
        size: (i32, i32),
    ) -> Option<(i32, i32)> {
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
            origin.0 + (x * f64::from(size.0)) as i32,
            origin.1 + (y * f64::from(size.1)) as i32,
        ))
    }
}

struct Renderer {
    encoders: EncoderPool,
    previous: Option<Frame>,
    previous_view: Option<View>,
    previous_grid: (usize, usize),
    last_patches: std::collections::HashMap<usize, [usize; 4]>,
}

impl Renderer {
    fn new() -> Self {
        Self {
            encoders: EncoderPool::new(),
            previous: None,
            previous_view: None,
            previous_grid: (0, 0),
            last_patches: Default::default(),
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
        out.extend_from_slice(b"\x1b[?2026h");
        if reset {
            delete(&mut out, self.previous_grid.0 * self.previous_grid.1)?;
            self.previous = None;
            self.last_patches.clear();
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
                if !tile.capture_patch(&frame, self.previous.as_ref()) { continue; }
                if let Some(patch) = tile.patch_position() { self.last_patches.insert(index + 1, patch); }
                batch.push(tile);
                if batch.len() == 4 {
                    self.flush(&mut batch, quality, &mut out, output)?;
                }
            } else if moved {
                tile.restore_patch(self.last_patches.get(&(index + 1)).copied());
                place(&mut out, &tile)?;
            }
        }
        self.flush(&mut batch, quality, &mut out, output)?;
        out.extend_from_slice(b"\x1b[?2026l");
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
            for image in self.encoders.batch(batch, quality)? {
                send(out, image)?;
            }
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

pub(super) fn stream(
    mut desktop: impl Desktop,
    options: &Options,
    mut sizes: watch::Receiver<PtySize>,
    mut input: mpsc::Receiver<Zeroizing<Vec<u8>>>,
    output: &mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let mut renderer = Renderer::new();
    let mut decoder = Decoder::new();
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
    let mut fps_limit = options.share_fps;
    let mut stream_width = options.share_width;
    let mut minimum_quality = 0;
    let mut frame_sequence = 0usize;
    let mut acknowledged_frame = 0usize;
    let mut acknowledgment_baseline = 0usize;
    let mut acknowledge_pictures = false;
    let period = Duration::from_secs_f64(1.0 / f64::from(fps));
    let started = Instant::now();
    let mut last_change = started;
    let mut next_frame = started;
    output
        .blocking_send(b"\x1b]777;yiterm;quality=2\x1b\\\x1b[?1049h\x1b[?25l\x1b[?1004h\x1b[2J\x1b[H\x1b[?9002$p".to_vec())
        .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
    if !options.share_read_only {
        output
            .blocking_send(b"\x1b[?1003h\x1b[?1006h\x1b[?1016h\x1b[?2004h".to_vec())
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
    }
    let mut recovering = None;
    while !output.is_closed() {
        let checked = desktop.check();
        let changed = desktop.changed();
        if changed || checked.is_err() && desktop.recoverable() {
            shown = None;
            decoder = Decoder::new();
            while input.try_recv().is_ok() {}
            renderer.previous = None;
            renderer.previous_view = None;
            let mut clear = Vec::new();
            delete(
                &mut clear,
                renderer.previous_grid.0 * renderer.previous_grid.1,
            )?;
            renderer.previous_grid = (0, 0);
            if !clear.is_empty() {
                output
                    .blocking_send(clear)
                    .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
            }
            next_frame = Instant::now();
        }
        if let Err(error) = checked {
            if !desktop.recoverable() {
                return Err(error);
            }
            let since = recovering.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_secs(5) {
                return Err(error);
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
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
                        desktop.release();
                    }
                }
                Event::Canvas(value) => canvas = Some(value),
                Event::FrameAcknowledged(sequence) => acknowledged_frame = acknowledged_frame.max(sequence),
                Event::StreamQuality([width, rate, quantization, acknowledgments]) => {
                    acknowledge_pictures = acknowledgments != 0;
                    acknowledgment_baseline = frame_sequence;
                    stream_width = if width == 0 { options.share_width } else if options.share_width == 0 { width as u32 } else { (width as u32).min(options.share_width) };
                    fps_limit = if rate == 0 { options.share_fps } else { (rate as u32).min(options.share_fps) };
                    fps = fps_limit;
                    minimum_quality = quantization;
                    quality = if quantization == 0 { fixed.unwrap_or(0) } else { quantization };
                    renderer.previous = None;
                }
                #[cfg(windows)]
                Event::DisplayScale(scale) if !options.share_read_only => {
                    desktop.display_scale(scale)
                }
                Event::Text(text) | Event::Paste(text)
                    if options.share_read_only && text.contains('q') =>
                {
                    return Ok(());
                }
                // The viewer routes input only to the selected pane. A stale focus
                // notification must not discard subsequent explicit user input.
                event if !options.share_read_only => {
                    if let Some(view) = shown {
                        desktop.input(event, view, || output.is_closed())?;
                    }
                }
                _ => {}
            }
        }
        let resized = sizes.has_changed().unwrap_or(false);
        if (resized || Instant::now() >= next_frame)
            && (canvas.is_some() || started.elapsed() >= Duration::from_millis(300))
        {
            if acknowledge_pictures && frame_sequence.saturating_sub(acknowledged_frame.max(acknowledgment_baseline)) >= 2 {
                std::thread::sleep(Duration::from_millis(4));
                continue;
            }
            let size = *sizes.borrow_and_update();
            let tick = Instant::now();
            if !options.share_read_only && desktop.viewport(size) {
                // Coordinates from the preceding mode are no longer usable.
                desktop.release();
                shown = None;
                renderer.previous = None;
                renderer.previous_view = None;
            }
            let captured = desktop
                .frame(stream_width, divisor)
                .and_then(|frame| {
                    desktop.check()?;
                    anyhow::ensure!(!desktop.changed(), "Windows desktop changed during capture");
                    Ok(frame)
                });
            let frame = match captured {
                Ok(frame) => {
                    recovering = None;
                    frame
                }
                Err(error) if desktop.recoverable() => {
                    shown = None;
                    decoder = Decoder::new();
                    while input.try_recv().is_ok() {}
                    renderer.previous = None;
                    let mut clear = Vec::new();
                    delete(
                        &mut clear,
                        renderer.previous_grid.0 * renderer.previous_grid.1,
                    )?;
                    renderer.previous_grid = (0, 0);
                    if !clear.is_empty() {
                        output
                            .blocking_send(clear)
                            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
                    }
                    let since = recovering.get_or_insert_with(Instant::now);
                    if since.elapsed() >= Duration::from_secs(5) {
                        return Err(error);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Err(error) => return Err(error),
            };
            let view = View::new(size, canvas.unwrap_or(false), (frame.w, frame.h));
            renderer.frame(frame, view, quality, output)?;
            frame_sequence = frame_sequence.wrapping_add(1);
            if acknowledge_pictures {
                output.blocking_send(format!("\x1b]777;yiterm;frame={frame_sequence}\x1b\\").into_bytes())
                    .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
            }
            shown = Some(view);
            let elapsed = tick.elapsed();
            if elapsed > period * 2 && last_change.elapsed() >= Duration::from_secs(2) {
                if (fixed.is_none() || stream_width != options.share_width) && quality < 3 {
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
                } else if fps < fps_limit {
                    fps += 1;
                } else if (fixed.is_none() || stream_width != options.share_width) && quality > minimum_quality {
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
