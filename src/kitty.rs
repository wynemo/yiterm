//! Static Kitty graphics over direct APC data transmission. No filesystem access.
use super::{Listener, Processor, Term};
use alacritty_terminal::{grid::Dimensions, vte::ansi::Handler};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    collections::{BTreeMap, VecDeque},
    io::Read,
    sync::{Arc, mpsc},
};

const MAX_BYTES: usize = 64 * 1024 * 1024;
const CACHE_BYTES: usize = 128 * 1024 * 1024;
const MAX_PLACEMENTS: usize = 1024;
type Controls = BTreeMap<u8, i64>;
type Decoded = Result<image::RgbaImage, &'static str>;

enum Action {
    Text(Vec<u8>),
    Command(Vec<u8>),
    Abort,
}
struct Transfer {
    controls: Controls,
    data: Vec<u8>,
}
struct Pending {
    controls: Controls,
    result: mpsc::Receiver<Result<Picture, &'static str>>,
}
pub struct Picture {
    pub width: u32,
    pub height: u32,
    #[cfg(target_os = "macos")]
    pub render: Arc<gpui::RenderImage>,
    #[cfg(target_os = "linux")]
    rgba: image::RgbaImage,
    #[cfg(target_os = "linux")]
    texture: std::sync::OnceLock<eframe::egui::TextureHandle>,
}
impl Picture {
    fn new(rgba: image::RgbaImage) -> Self {
        Self {
            width: rgba.width(),
            height: rgba.height(),
            #[cfg(target_os = "macos")]
            render: {
                // GPUI's sprite shader expects BGRA bytes.
                let mut bgra = rgba;
                for pixel in bgra.pixels_mut() {
                    pixel.0.swap(0, 2);
                }
                Arc::new(gpui::RenderImage::new(vec![image::Frame::new(bgra)]))
            },
            #[cfg(target_os = "linux")]
            rgba,
            #[cfg(target_os = "linux")]
            texture: Default::default(),
        }
    }
    #[cfg(target_os = "linux")]
    pub fn texture(&self, cx: &eframe::egui::Context) -> eframe::egui::TextureId {
        self.texture
            .get_or_init(|| {
                cx.load_texture(
                    "kitty-image",
                    eframe::egui::ColorImage::from_rgba_unmultiplied(
                        [self.width as usize, self.height as usize],
                        self.rgba.as_raw(),
                    ),
                    eframe::egui::TextureOptions::LINEAR,
                )
            })
            .id()
    }
}
struct Placement {
    anchor: u64,
    image: u32,
    id: u32,
    cols: u32,
    rows: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    dx: u32,
    dy: u32,
    z: i32,
}
pub struct Draw<'a> {
    pub picture: &'a Picture,
    /// Destination in logical pixels relative to the viewport.
    pub rect: [f32; 4],
    /// Normalized source rectangle.
    pub uv: [f32; 4],
    pub z: i32,
}
pub struct Graphics {
    pub enabled: bool,
    repaint: Arc<super::Repaint>,
    anonymous: std::collections::BTreeSet<u32>,
    state: u8,
    apc: Vec<u8>,
    overflow: bool,
    actions: VecDeque<Action>,
    transfer: Option<Transfer>,
    pending: Option<Pending>,
    resume_sync: bool,
    images: BTreeMap<u32, Arc<Picture>>,
    placements: Vec<Placement>,
    next_image: u32,
    next_anchor: u64,
    pub cell: [f32; 2],
    pub pixels: [u16; 2],
    scale: f32,
}
impl Graphics {
    pub fn new(enabled: bool, repaint: Arc<super::Repaint>) -> Self {
        Self {
            enabled,
            repaint,
            anonymous: Default::default(),
            state: 0,
            apc: Vec::new(),
            overflow: false,
            actions: VecDeque::new(),
            transfer: None,
            pending: None,
            resume_sync: false,
            images: BTreeMap::new(),
            placements: Vec::new(),
            next_image: 0x80000000,
            next_anchor: 1,
            cell: [8., 20.],
            pixels: [0, 0],
            scale: 1.,
        }
    }
    pub fn set_cell(&mut self, cell: [f32; 2], scale: f32, cols: usize, rows: usize) -> bool {
        self.cell = cell;
        self.scale = scale.max(1.);
        let pixels = [
            (cell[0] * self.scale * cols as f32)
                .round()
                .clamp(1., u16::MAX as f32) as u16,
            (cell[1] * self.scale * rows as f32)
                .round()
                .clamp(1., u16::MAX as f32) as u16,
        ];
        let changed = pixels != self.pixels;
        self.pixels = pixels;
        changed
    }
    pub fn cell_pixels(&self) -> [u16; 2] {
        self.cell
            .map(|v| (v * self.scale).round().clamp(1., u16::MAX as f32) as u16)
    }
    #[cfg(target_os = "macos")]
    pub fn render_images(&self) -> impl Iterator<Item = Arc<gpui::RenderImage>> + '_ {
        self.images.values().map(|picture| picture.render.clone())
    }
    pub fn busy(&self) -> bool {
        self.pending.is_some() || !self.actions.is_empty()
    }
    /// Preserve VT strings and byte order; APCs can span arbitrary PTY reads.
    pub fn feed(&mut self, input: &[u8]) {
        let mut text = Vec::new();
        for &b in input {
            match self.state {
                0 => {
                    if b == 0x1b {
                        self.state = 1;
                    } else {
                        text.push(b);
                    }
                }
                1 => {
                    if b == b'_' {
                        if !text.is_empty() {
                            self.actions
                                .push_back(Action::Text(std::mem::take(&mut text)));
                        }
                        self.apc.clear();
                        self.overflow = false;
                        self.state = 4;
                    } else if b == 0x1b {
                        text.push(0x1b);
                    } else {
                        text.extend_from_slice(&[0x1b, b]);
                        self.state = match b {
                            b']' => 2,
                            b'P' | b'X' | b'^' => 7,
                            b'[' => 6,
                            _ => 0,
                        };
                    }
                }
                2 | 7 => {
                    text.push(b);
                    if b == 0x1b {
                        self.state += 1;
                    } else if b == 0x18 || b == 0x1a || (b == 7 && self.state == 2) {
                        self.state = 0;
                    }
                }
                3 | 8 => {
                    text.push(b);
                    self.state = if b == b'\\' || b == 0x18 || b == 0x1a {
                        0
                    } else if b == 0x1b {
                        self.state
                    } else {
                        self.state - 1
                    };
                }
                4 => {
                    if b == 0x1b {
                        self.state = 5;
                    } else if b == 0x18 || b == 0x1a {
                        self.state = 0;
                        self.actions.push_back(Action::Abort);
                    } else if self.apc.len() < 8192 {
                        self.apc.push(b);
                    } else {
                        self.overflow = true;
                    }
                }
                5 => {
                    if b == b'\\' {
                        if !self.overflow && self.apc.first() == Some(&b'G') {
                            self.actions
                                .push_back(Action::Command(std::mem::take(&mut self.apc)));
                        } else {
                            self.actions.push_back(Action::Abort);
                        }
                        self.state = 0;
                    } else {
                        // ESC aborts the APC and starts a new VT escape sequence.
                        self.actions.push_back(Action::Abort);
                        self.state = 1;
                        if b == b'_' {
                            self.apc.clear();
                            self.overflow = false;
                            self.state = 4;
                        } else {
                            text.extend_from_slice(&[0x1b, b]);
                            self.state = match b {
                                b'[' => 6,
                                b']' => 2,
                                b'P' | b'X' | b'^' => 7,
                                0x1b => 1,
                                _ => 0,
                            };
                        }
                    }
                }
                6 => {
                    if b != 0x1b {
                        text.push(b);
                    }
                    if b == 0x1b {
                        self.state = 1;
                    } else if (0x40..=0x7e).contains(&b) || b == 0x18 || b == 0x1a {
                        self.state = 0;
                    }
                }
                _ => unreachable!(),
            }
        }
        if !text.is_empty() {
            self.actions.push_back(Action::Text(text));
        }
    }
    pub fn advance(&mut self, parser: &mut Processor, term: &mut Term<Listener>) {
        if let Some(pending) = self.pending.take() {
            match pending.result.try_recv() {
                Ok(result) => self.finish(pending.controls, result, term),
                Err(mpsc::TryRecvError::Empty) => {
                    self.pending = Some(pending);
                    return;
                }
                Err(_) => reply(term, &pending.controls, "EIO:decoder stopped"),
            }
        }
        if self.resume_sync {
            parser.advance(term, b"\x1b[?2026h");
            self.resume_sync = false;
        }
        while let Some(action) = self.actions.pop_front() {
            match action {
                Action::Text(bytes) => parser.advance(term, &bytes),
                Action::Abort => self.transfer = None,
                Action::Command(bytes) => {
                    // APCs are dispatched outside VTE. Its synchronized-update
                    // buffer must first apply all preceding cursor movements,
                    // scrolling and clears, or an image uses the previous frame's
                    // cursor. Resume buffering only after the command completes.
                    let synchronized = parser.sync_timeout().sync_timeout().is_some();
                    if synchronized {
                        tracing::debug!(target: "yiterm::graphics",
                            buffered_bytes = parser.sync_bytes_count(),
                            "Apply synchronized VT output before Kitty command");
                        parser.stop_sync(term);
                    }
                    self.command(&bytes[1..], term);
                    if synchronized {
                        if self.pending.is_some() {
                            self.resume_sync = true;
                        } else {
                            parser.advance(term, b"\x1b[?2026h");
                        }
                    }
                }
            }
            if self.pending.is_some() {
                break;
            }
        }
        self.placements
            .retain(|p| term.has_graphics_anchor(p.anchor));
        // Anonymous images have no future addressable uses.
        self.images.retain(|id, _| {
            !self.anonymous.contains(id) || self.placements.iter().any(|p| p.image == *id)
        });
        self.anonymous.retain(|id| self.images.contains_key(id));
    }
    fn command(&mut self, bytes: &[u8], term: &mut Term<Listener>) {
        let (header, payload) = bytes.split_once_byte(b';');
        let Ok(mut c) = controls(header) else {
            self.transfer = None;
            return;
        };
        if !self.enabled {
            return;
        }
        if get(&c, b'm', 0) > 1 || get(&c, b'q', 0) > 2 || get(&c, b'C', 0) > 1 {
            self.transfer = None;
            reply(term, &c, "EINVAL:control value");
            return;
        }
        let action = get(&c, b'a', b't' as i64) as u8;
        if action == b'd' {
            self.delete(&c, term);
            return;
        }
        if action == b'p' {
            let result = self.place(&c, term);
            reply(term, &c, result.err().unwrap_or("OK"));
            return;
        }
        if !matches!(action, b't' | b'T' | b'q') {
            reply(term, &c, "ENOTSUP:static images only");
            return;
        }
        if let Some(mut transfer) = self.transfer.take() {
            if c.keys().any(|key| !matches!(key, b'm' | b'q')) {
                reply(term, &transfer.controls, "EINVAL:interrupted transfer");
                return;
            }
            if let Some(q) = c.get(&b'q') {
                transfer.controls.insert(b'q', *q);
            }
            let more = get(&c, b'm', 0);
            c = transfer.controls;
            c.insert(b'm', more);
            self.transfer = Some(Transfer {
                controls: c.clone(),
                data: std::mem::take(&mut transfer.data),
            });
        }
        let medium = get(&c, b't', b'd' as i64);
        if medium != b'd' as i64 {
            self.transfer = None;
            reply(term, &c, "ENOTSUP:use direct transmission t=d");
            return;
        }
        let format = get(&c, b'f', 32);
        if !matches!(format, 24 | 32 | 100)
            || get(&c, b'U', 0) != 0
            || c.contains_key(&b'P')
            || c.contains_key(&b'I')
        {
            self.transfer = None;
            reply(term, &c, "ENOTSUP:unsupported format or placement");
            return;
        }
        let mut data = self.transfer.take().map(|t| t.data).unwrap_or_default();
        if payload.len() > 4096 || data.len().saturating_add(payload.len() * 3 / 4) > MAX_BYTES {
            reply(term, &c, "E2BIG:image data limit");
            return;
        }
        let Ok(chunk) = STANDARD.decode(payload) else {
            reply(term, &c, "EINVAL:invalid base64");
            return;
        };
        data.extend_from_slice(&chunk);
        if get(&c, b'm', 0) == 1 {
            self.transfer = Some(Transfer { controls: c, data });
            return;
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let options = c.clone();
        // A single in-flight decoder per pane, with PTY backpressure. Later VT
        // bytes wait so query replies and cursor positions remain in wire order.
        let repaint = self.repaint.clone();
        std::thread::spawn(move || {
            let _ = tx.send(decode(data, &options).map(Picture::new));
            repaint.store(true, std::sync::atomic::Ordering::Release);
            super::wake_ui();
        });
        self.pending = Some(Pending {
            controls: c,
            result: rx,
        });
    }
    fn finish(
        &mut self,
        mut c: Controls,
        result: Result<Picture, &'static str>,
        term: &mut Term<Listener>,
    ) {
        let picture = match result {
            Ok(image) => image,
            Err(error) => {
                reply(term, &c, error);
                return;
            }
        };
        if get(&c, b'a', b't' as i64) == b'q' as i64 {
            reply(term, &c, "OK");
            return;
        }
        let original_id = get(&c, b'i', 0) as u32;
        let id = if original_id == 0 {
            while self.images.contains_key(&self.next_image) {
                self.next_image = self.next_image.wrapping_add(1).max(0x80000000);
            }
            let id = self.next_image;
            self.next_image = self.next_image.wrapping_add(1).max(0x80000000);
            id
        } else {
            original_id
        };
        self.remove_image(id, term);
        let bytes = picture.width as usize * picture.height as usize * 4;
        while self.cache_size() + bytes > CACHE_BYTES || self.images.len() >= 256 {
            let Some(old) = self.images.keys().next().copied() else {
                break;
            };
            self.remove_image(old, term);
        }
        self.images.insert(id, Arc::new(picture));
        if original_id == 0 {
            self.anonymous.insert(id);
        }
        if get(&c, b'a', b't' as i64) == b'T' as i64 {
            c.insert(b'i', id as i64);
            let result = self.place(&c, term);
            c.insert(b'i', original_id as i64);
            reply(term, &c, result.err().unwrap_or("OK"));
        } else {
            reply(term, &c, "OK");
        }
    }
    fn cache_size(&self) -> usize {
        self.images
            .values()
            .map(|i| i.width as usize * i.height as usize * 4)
            .sum()
    }
    fn remove_image(&mut self, id: u32, term: &mut Term<Listener>) {
        self.placements.retain(|p| {
            if p.image == id {
                term.remove_graphics_anchor(p.anchor);
                false
            } else {
                true
            }
        });
        self.images.remove(&id);
        self.anonymous.remove(&id);
    }
    fn place(&mut self, c: &Controls, term: &mut Term<Listener>) -> Result<(), &'static str> {
        if get(c, b'U', 0) != 0 || c.contains_key(&b'P') || c.contains_key(&b'I') {
            return Err("ENOTSUP:placement mode");
        }
        let id = get(c, b'i', 0) as u32;
        let image = self.images.get(&id).ok_or("ENOENT:unknown image")?;
        let x = get(c, b'x', 0) as u32;
        let y = get(c, b'y', 0) as u32;
        if x >= image.width || y >= image.height {
            return Err("EINVAL:source rectangle");
        }
        let w = (get(c, b'w', (image.width - x) as i64) as u32).min(image.width - x);
        let h = (get(c, b'h', (image.height - y) as i64) as u32).min(image.height - y);
        if w == 0 || h == 0 {
            return Err("EINVAL:empty source");
        }
        let cols = get(c, b'c', 0) as u32;
        let rows = get(c, b'r', 0) as u32;
        let dx = get(c, b'X', 0) as u32;
        let dy = get(c, b'Y', 0) as u32;
        if cols > 65535
            || rows > 65535
            || dx as f32 / self.scale >= self.cell[0]
            || dy as f32 / self.scale >= self.cell[1]
        {
            return Err("EINVAL:placement dimensions");
        }
        let pid = get(c, b'p', 0) as u32;
        self.placements.retain(|p| {
            if p.image == id && pid != 0 && p.id == pid {
                term.remove_graphics_anchor(p.anchor);
                false
            } else {
                true
            }
        });
        if self.placements.len() >= MAX_PLACEMENTS {
            return Err("ENOSPC:placement limit");
        }
        let anchor = self.next_anchor;
        self.next_anchor = self.next_anchor.wrapping_add(1);
        let placement = Placement {
            anchor,
            image: id,
            id: pid,
            cols,
            rows,
            x,
            y,
            w,
            h,
            dx,
            dy,
            z: get(c, b'z', 0) as i32,
        };
        let [width, height] = self.extent(&placement);
        tracing::debug!(target: "yiterm::graphics", image = id, placement = pid,
            row = term.grid().cursor.point.line.0, column = term.grid().cursor.point.column.0,
            cols, rows, width, height, cursor_policy = get(c, b'C', 0),
            cell_width = self.cell[0], cell_height = self.cell[1], scale = self.scale,
            "Place Kitty image");
        term.graphics_anchors
            .insert(anchor, term.grid().cursor.point);
        if get(c, b'C', 0) == 0 {
            let column = term.grid().cursor.point.column.0;
            let row_count = ((height + dy as f32 / self.scale) / self.cell[1]).ceil() as usize;
            // linefeed honors the terminal scrolling region and updates anchors.
            for _ in 1..row_count.min(65535) {
                term.linefeed();
            }
            term.goto_col(
                (column + ((width + dx as f32 / self.scale) / self.cell[0]).ceil() as usize)
                    .min(term.columns() - 1),
            );
        }
        self.placements.push(placement);
        Ok(())
    }
    fn extent(&self, p: &Placement) -> [f32; 2] {
        match (p.cols, p.rows) {
            (0, 0) => [p.w as f32 / self.scale, p.h as f32 / self.scale],
            (c, 0) => {
                let w = c as f32 * self.cell[0];
                [w, w * p.h as f32 / p.w as f32]
            }
            (0, r) => {
                let h = r as f32 * self.cell[1];
                [h * p.w as f32 / p.h as f32, h]
            }
            (c, r) => [c as f32 * self.cell[0], r as f32 * self.cell[1]],
        }
    }
    fn delete(&mut self, c: &Controls, term: &mut Term<Listener>) {
        let mode = get(c, b'd', b'a' as i64) as u8;
        if !matches!(mode, b'a' | b'A' | b'i' | b'I') {
            reply(term, c, "ENOTSUP:delete mode");
            return;
        }
        let id = get(c, b'i', 0) as u32;
        let pid = get(c, b'p', 0) as u32;
        let mut removed = Vec::new();
        self.placements.retain(|p| {
            let remove = if matches!(mode, b'i' | b'I') {
                p.image == id && (pid == 0 || pid == p.id)
            } else {
                term.graphics_anchors.get(&p.anchor).is_some_and(|point| {
                    point.line.0 >= 0 && point.line.0 < term.screen_lines() as i32
                })
            };
            if remove {
                term.remove_graphics_anchor(p.anchor);
                removed.push(p.image);
            }
            !remove
        });
        if mode.is_ascii_uppercase() {
            if mode == b'I' {
                removed.push(id);
            }
            self.images.retain(|id, _| {
                !removed.contains(id) || self.placements.iter().any(|p| p.image == *id)
            });
        }
    }
    pub fn draws<'a>(&'a self, term: &Term<Listener>) -> Vec<Draw<'a>> {
        let offset = term.grid().display_offset() as f32;
        let mut draws = Vec::new();
        for p in &self.placements {
            let Some(point) = term.graphics_anchors.get(&p.anchor) else {
                continue;
            };
            let Some(image) = self.images.get(&p.image) else {
                continue;
            };
            let [w, h] = self.extent(p);
            let x = point.column.0 as f32 * self.cell[0] + p.dx as f32 / self.scale;
            let y = (point.line.0 as f32 + offset) * self.cell[1] + p.dy as f32 / self.scale;
            if y + h <= 0. || y >= term.screen_lines() as f32 * self.cell[1] {
                continue;
            }
            draws.push(Draw {
                picture: image,
                rect: [x, y, w, h],
                z: p.z,
                uv: [
                    p.x as f32 / image.width as f32,
                    p.y as f32 / image.height as f32,
                    (p.x + p.w) as f32 / image.width as f32,
                    (p.y + p.h) as f32 / image.height as f32,
                ],
            });
        }
        draws.sort_by_key(|d| d.z);
        draws
    }
}
fn get(c: &Controls, key: u8, default: i64) -> i64 {
    c.get(&key).copied().unwrap_or(default)
}
fn reply(term: &Term<Listener>, c: &Controls, message: &str) {
    let quiet = get(c, b'q', 0);
    if quiet == 2 || (quiet == 1 && message == "OK") {
        return;
    }
    let id = get(c, b'i', 0);
    if id == 0 && message == "OK" {
        return;
    }
    let mut header = format!("i={id}");
    if let Some(pid) = c.get(&b'p') {
        header.push_str(&format!(",p={pid}"));
    }
    term.graphics_reply(format!("\x1b_G{header};{message}\x1b\\"));
}
fn controls(header: &[u8]) -> Result<Controls, ()> {
    let mut c = Controls::new();
    if header.is_empty() {
        return Ok(c);
    }
    for part in header.split(|b| *b == b',') {
        if part.len() < 3 || part[1] != b'=' || !part[0].is_ascii_alphabetic() {
            return Err(());
        }
        let key = part[0];
        let value = &part[2..];
        let n = if matches!(key, b'a' | b't' | b'o' | b'd') {
            if value.len() != 1 {
                return Err(());
            }
            value[0] as i64
        } else {
            let n = std::str::from_utf8(value)
                .map_err(|_| ())?
                .parse::<i64>()
                .map_err(|_| ())?;
            if n > u32::MAX as i64
                || n < if matches!(key, b'z' | b'H' | b'V') {
                    i32::MIN as i64
                } else {
                    0
                }
            {
                return Err(());
            }
            if key == b'z' && n > i32::MAX as i64 {
                return Err(());
            }
            n
        };
        if c.insert(key, n).is_some() {
            return Err(());
        }
    }
    Ok(c)
}
fn decode(mut data: Vec<u8>, c: &Controls) -> Decoded {
    if let Some(compression) = c.get(&b'o') {
        if *compression != b'z' as i64 {
            return Err("ENOTSUP:compression");
        }
        let mut decoded = Vec::new();
        flate2::read::ZlibDecoder::new(data.as_slice())
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut decoded)
            .map_err(|_| "EINVAL:zlib data")?;
        if decoded.len() > MAX_BYTES {
            return Err("E2BIG:decompressed image");
        }
        data = decoded;
    }
    if get(c, b'f', 32) == 100 {
        let mut reader =
            image::ImageReader::with_format(std::io::Cursor::new(data), image::ImageFormat::Png);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(MAX_BYTES as u64);
        reader.limits(limits);
        let image = reader
            .decode()
            .map_err(|_| "EINVAL:PNG data or size limit")?;
        if image.width() as u64 * image.height() as u64 * 4 > MAX_BYTES as u64 {
            return Err("E2BIG:image dimensions");
        }
        return Ok(image.to_rgba8());
    }
    let width = get(c, b's', 0) as u32;
    let height = get(c, b'v', 0) as u32;
    if width == 0
        || height == 0
        || width > 8192
        || height > 8192
        || width as u64 * height as u64 * 4 > MAX_BYTES as u64
    {
        return Err("EINVAL:image dimensions");
    }
    let channels = if get(c, b'f', 32) == 24 { 3 } else { 4 };
    if data.len() != width as usize * height as usize * channels {
        return Err("EINVAL:pixel data length");
    }
    if get(c, b'f', 32) == 24 {
        image::RgbImage::from_raw(width, height, data)
            .map(|rgb| image::DynamicImage::ImageRgb8(rgb).to_rgba8())
            .ok_or("EINVAL:RGB data length")
    } else {
        image::RgbaImage::from_raw(width, height, data).ok_or("EINVAL:RGBA data length")
    }
}
trait SplitByte {
    fn split_once_byte(&self, b: u8) -> (&[u8], &[u8]);
}
impl SplitByte for [u8] {
    fn split_once_byte(&self, b: u8) -> (&[u8], &[u8]) {
        self.iter()
            .position(|v| *v == b)
            .map(|i| (&self[..i], &self[i + 1..]))
            .unwrap_or((self, &[]))
    }
}
