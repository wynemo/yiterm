use anyhow::Result;
use base64::{Engine, engine::general_purpose::STANDARD};
use flate2::{Compression, write::ZlibEncoder};
use std::{
    io::Write,
    sync::{Arc, Mutex},
};

pub(super) struct Frame {
    pub w: usize,
    pub h: usize,
    pub pixels: Vec<u8>, // Packed BGRA.
}
impl Frame {
    pub fn tile_rgb(&self, x: usize, y: usize, w: usize, h: usize) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(w * h * 3);
        for line in y..y + h {
            let start = (line * self.w + x) * 4;
            for pixel in self.pixels[start..start + w * 4].chunks_exact(4) {
                rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
            }
        }
        rgb
    }
}

pub(super) fn fit(
    (cols, rows): (usize, usize),
    (cell_w, cell_h): (f64, f64),
    (w, h): (usize, usize),
) -> (usize, usize) {
    let scale = (cols as f64 * cell_w / w as f64).min(rows as f64 * cell_h / h as f64);
    (
        ((w as f64 * scale / cell_w) as usize).max(1).min(cols),
        ((h as f64 * scale / cell_h) as usize).max(1).min(rows),
    )
}

pub(super) fn dirty_tiles(
    frame: &Frame,
    previous: Option<&Frame>,
    (nx, ny): (usize, usize),
) -> Vec<bool> {
    // krd-style single row scan: only inspect tile columns still
    // clean, and stop a tile band as soon as every column is dirty.
    let mut dirty_tiles = vec![previous.is_none(); nx * ny];
    if let Some(previous) = previous {
        for iy in 0..ny {
            for y in iy * frame.h / ny..(iy + 1) * frame.h / ny {
                if dirty_tiles[iy * nx..(iy + 1) * nx]
                    .iter()
                    .all(|dirty| *dirty)
                {
                    break;
                }
                let row = y * frame.w * 4;
                if previous.pixels[row..row + frame.w * 4] == frame.pixels[row..row + frame.w * 4] {
                    continue;
                }
                for ix in 0..nx {
                    let index = iy * nx + ix;
                    if dirty_tiles[index] {
                        continue;
                    }
                    let left = row + ix * frame.w / nx * 4;
                    let right = row + (ix + 1) * frame.w / nx * 4;
                    // RGB24 is what we actually transmit; alpha/padding changes
                    // must not turn an otherwise identical desktop into traffic.
                    dirty_tiles[index] = previous.pixels[left..right].chunks_exact(4)
                        .zip(frame.pixels[left..right].chunks_exact(4))
                        .any(|(old, new)| old[..3] != new[..3]);
                }
            }
        }
    }
    dirty_tiles
}

pub(super) fn delete(out: &mut impl Write, count: usize) -> Result<()> {
    for index in 0..count {
        write!(out, "\x1b_Ga=d,d=I,i={},q=2;\x1b\\", index + 1)?;
    }
    Ok(())
}
// Fixed source grid: resizing changes placements, not pixel boundaries.
pub(super) fn layout(cols: usize, rows: usize) -> (usize, usize) {
    (cols.min(16), rows.min(8))
}
pub(super) struct Tile {
    id: usize,
    col: usize,
    row: usize,
    cols: usize,
    rows: usize,
    w: usize,
    h: usize,
    canvas: Option<[usize; 4]>,
    pub rgb: Vec<u8>,
}
impl Tile {
    pub fn patch_position(&self) -> Option<[usize; 4]> {
        self.canvas.map(|[_, _, x, y]| [x, y, self.w, self.h])
    }
    pub fn restore_patch(&mut self, patch: Option<[usize; 4]>) {
        if let (Some(canvas), Some([x, y, w, h])) = (self.canvas.as_mut(), patch) {
            canvas[2] = x; canvas[3] = y; self.w = w; self.h = h;
        }
    }
    pub fn capture_patch(&mut self, frame: &Frame, previous: Option<&Frame>) -> bool {
        let (x, y) = self.canvas.map_or((0, 0), |c| (c[2], c[3]));
        if self.canvas.is_some() {
            if let Some(previous) = previous {
                let (mut left, mut top, mut right, mut bottom) = (self.w, self.h, 0, 0);
                for row in 0..self.h {
                    let start = ((y + row) * frame.w + x) * 4;
                    let old = &previous.pixels[start..start + self.w * 4];
                    let new = &frame.pixels[start..start + self.w * 4];
                    if old == new { continue; }
                    let changed = |column: usize| old[column*4..column*4+3] != new[column*4..column*4+3];
                    if let Some(first) = (0..self.w).find(|&column| changed(column)) {
                        let last = (first..self.w).rfind(|&column| changed(column)).unwrap();
                        left = left.min(first); right = right.max(last + 1);
                        top = top.min(row); bottom = bottom.max(row + 1);
                    }
                }
                if right <= left || bottom <= top { return false; }
                self.restore_patch(Some([x + left, y + top, right - left, bottom - top]));
            }
        }
        let (x, y) = self.canvas.map_or((x, y), |c| (c[2], c[3]));
        self.rgb = frame.tile_rgb(x, y, self.w, self.h);
        true
    }
}

pub(super) struct Encoded {
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
pub(super) struct EncoderPool {
    jobs: Option<std::sync::mpsc::SyncSender<(Tile, usize)>>,
    results: std::sync::mpsc::Receiver<Result<Encoded>>,
    workers: Vec<std::thread::JoinHandle<()>>,
}
impl EncoderPool {
    pub fn new() -> Self {
        let (jobs, work) = std::sync::mpsc::sync_channel::<(Tile, usize)>(4);
        let work = Arc::new(Mutex::new(work));
        let (done, results) = std::sync::mpsc::sync_channel(4);
        let workers = (0..4)
            .map(|_| {
                let work = work.clone();
                let done = done.clone();
                std::thread::spawn(move || {
                    loop {
                        let job = work.lock().unwrap().recv();
                        let Ok((tile, quality)) = job else { break };
                        if done.send(encode(tile, quality)).is_err() {
                            break;
                        }
                    }
                })
            })
            .collect();
        Self {
            jobs: Some(jobs),
            results,
            workers,
        }
    }
    pub fn batch(&self, tiles: &mut [Tile], quality: usize) -> Result<Vec<Encoded>> {
        for t in tiles.iter_mut() {
            let tile = Tile {
                id: t.id,
                col: t.col,
                row: t.row,
                cols: t.cols,
                rows: t.rows,
                w: t.w,
                h: t.h,
                canvas: t.canvas,
                rgb: std::mem::take(&mut t.rgb),
            };
            self.jobs
                .as_ref()
                .unwrap()
                .send((tile, quality))
                .map_err(|_| anyhow::anyhow!("Encoder stopped"))?;
        }
        let mut encoded = (0..tiles.len())
            .map(|_| {
                self.results
                    .recv()
                    .map_err(|_| anyhow::anyhow!("Encoder stopped"))?
            })
            .collect::<Result<Vec<_>>>()?;
        encoded.sort_by_key(|e| e.tile.id);
        Ok(encoded)
    }
}
impl Drop for EncoderPool {
    fn drop(&mut self) {
        self.jobs.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

pub(super) fn tile_layout(
    id: usize,
    ix: usize,
    iy: usize,
    grid: (usize, usize),
    geo: (usize, usize),
    origin: (usize, usize),
    source: (usize, usize),
    canvas: bool,
) -> Tile {
    let (nx, ny) = grid;
    let (cols, rows) = geo;
    let (sw, sh) = source;
    let (x, y) = (ix * sw / nx, iy * sh / ny);
    let (col, row) = (ix * cols / nx, iy * rows / ny);
    Tile {
        id,
        col: origin.0 + if canvas { 0 } else { col },
        row: origin.1 + if canvas { 0 } else { row },
        cols: if canvas {
            cols
        } else {
            (ix + 1) * cols / nx - col
        },
        rows: if canvas { rows } else { 0 },
        w: (ix + 1) * sw / nx - x,
        h: (iy + 1) * sh / ny - y,
        canvas: canvas.then_some([sw, sh, x, y]),
        rgb: Vec::new(),
    }
}
pub(super) fn place(out: &mut impl Write, t: &Tile) -> Result<()> {
    write!(
        out,
        "\x1b[{};{}H\x1b_Ga=p,i={},p=1,c={},r={},C=1,q=2",
        t.row + 1,
        t.col + 1,
        t.id,
        t.cols,
        t.rows
    )?;
    if let Some([w, h, x, y]) = t.canvas {
        write!(out, ",D={w},E={h},N={x},O={y}")?;
    }
    out.write_all(b";\x1b\\")?;
    Ok(())
}
pub(super) fn send(out: &mut impl Write, image: Encoded) -> Result<usize> {
    let t = image.tile;
    let chunks = image.data.as_bytes().chunks(3072);
    let n = chunks.len();
    let mut sequence = Vec::with_capacity(4096);
    for (index, chunk) in chunks.enumerate() {
        sequence.clear();
        if index == 0 {
            write!(
                sequence,
                "\x1b_Ga=t,f=24,s={},v={},i={},q=2,m={}{};",
                t.w,
                t.h,
                t.id,
                usize::from(n > 1),
                if image.compressed { ",o=z" } else { "" }
            )?;
        } else {
            write!(sequence, "\x1b_Gm={},q=2;", usize::from(index + 1 < n))?;
        }
        sequence.extend_from_slice(chunk);
        sequence.extend_from_slice(b"\x1b\\");
        out.write_all(&sequence)?;
    }
    place(out, &t)?;
    Ok(image.data.len())
}
