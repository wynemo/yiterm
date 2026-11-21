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
        desktop.check()?;
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
                Event::Text(text) | Event::Paste(text)
                    if options.share_read_only && text.contains('q') =>
                {
                    return Ok(());
                }
                event if !options.share_read_only && focused => {
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
            let size = *sizes.borrow_and_update();
            let tick = Instant::now();
            let frame = desktop.frame(options.share_width, divisor)?;
            desktop.check()?;
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
