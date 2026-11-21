use super::{
    Options,
    desktop::{self, Desktop, View, capture_size},
    graphics::Frame,
    input::Event,
};
use anyhow::{Context, Result, bail, ensure};
use portable_pty::PtySize;
use std::{
    mem,
    ptr::{null, null_mut},
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

mod console;
mod control;
mod display;
mod dpi;

static RESOLUTION_OWNER: std::sync::Mutex<()> = std::sync::Mutex::new(());
pub mod privilege;

pub fn prepare() -> Result<()> {
    if privilege::effective_system() {
        console::prepare()?;
        Ok(())
    } else {
        require_user_desktop()
    }
}

// Bind only the calling worker to the current input desktop. Network/runtime
// threads never retain a secure-desktop binding across operations.
struct DesktopBinding {
    desktop: HDESK,
    previous: HDESK,
    name: String,
}
impl DesktopBinding {
    fn open() -> Result<Self> {
        unsafe {
            let previous = GetThreadDesktop(GetCurrentThreadId());
            ensure!(!previous.is_null(), "无法保存原线程桌面");
            let desktop = OpenInputDesktop(
                0,
                0,
                DESKTOP_READOBJECTS
                    | DESKTOP_WRITEOBJECTS
                    | DESKTOP_SWITCHDESKTOP
                    | DESKTOP_JOURNALPLAYBACK,
            );
            ensure!(
                !desktop.is_null(),
                "无法访问 Windows 当前桌面：{}",
                std::io::Error::last_os_error()
            );
            let mut binding = Self {
                desktop,
                previous,
                name: String::new(),
            };
            binding.name = desktop_name(desktop)?;
            ensure!(
                binding.name.eq_ignore_ascii_case("Default")
                    || (privilege::effective_system()
                        && binding.name.eq_ignore_ascii_case("Winlogon")),
                "当前进程无法访问 Windows 安全桌面，请以管理员身份启动共享"
            );
            ensure!(
                SetThreadDesktop(desktop) != 0,
                "无法切换 Windows 采集/输入线程桌面：{}",
                std::io::Error::last_os_error()
            );
            Ok(binding)
        }
    }
}
impl Drop for DesktopBinding {
    fn drop(&mut self) {
        unsafe {
            if SetThreadDesktop(self.previous) == 0 {
                std::process::abort();
            }
            CloseDesktop(self.desktop);
        }
    }
}
fn desktop_name(desktop: HDESK) -> Result<String> {
    let mut name = [0u16; 256];
    let mut needed = 0;
    ensure!(
        unsafe {
            GetUserObjectInformationW(
                desktop,
                UOI_NAME,
                name.as_mut_ptr().cast(),
                mem::size_of_val(&name) as u32,
                &mut needed,
            )
        } != 0,
        "无法读取 Windows 桌面身份"
    );
    Ok(String::from_utf16_lossy(
        &name[..name.iter().position(|c| *c == 0).unwrap_or(name.len())],
    ))
}
pub(crate) fn current_desktop_name() -> Option<String> {
    unsafe {
        let desktop = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
        if desktop.is_null() {
            return None;
        }
        let name = desktop_name(desktop).ok();
        CloseDesktop(desktop);
        name
    }
}

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
    fn frame(
        &mut self,
        source: &Source,
        rect: RECT,
        width: u32,
        divisor: usize,
        secure: bool,
    ) -> Result<Frame> {
        let native = dimensions(rect)?;
        let (w, h) = native;
        let size = capture_size(native, width, divisor);
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
        // Secure desktop drivers may reject StretchBlt, including equal-size copies.
        // Copy native pixels with BitBlt and resize in CPU memory instead.
        if secure {
            Surface::get(&mut self.window, &self.screen, native)?;
            let surface = self
                .window
                .as_mut()
                .context("Missing native capture surface")?;
            let ok = unsafe {
                BitBlt(
                    surface.dc,
                    0,
                    0,
                    w as i32,
                    h as i32,
                    self.screen.0,
                    rect.left,
                    rect.top,
                    SRCCOPY,
                )
            };
            ensure!(ok != 0, "Windows could not copy the lock screen");
            let frame = surface.frame();
            if size == native {
                return Ok(frame);
            }
            let mut pixels = vec![0; size.0 * size.1 * 4];
            for y in 0..size.1 {
                for x in 0..size.0 {
                    let from = ((y * h / size.1) * w + x * w / size.0) * 4;
                    let to = (y * size.0 + x) * 4;
                    pixels[to..to + 4].copy_from_slice(&frame.pixels[from..from + 4]);
                }
            }
            return Ok(Frame {
                w: size.0,
                h: size.1,
                pixels,
            });
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

struct WindowsDesktop {
    source: Source,
    capture: Option<Capture>,
    control: control::Control,
    binding: Option<DesktopBinding>,
    lock_source: Source,
    changed: bool,
    resolution: display::DesktopResolution,
    dpi: dpi::DesktopDpi,
    display_scale: Option<u32>,
    resolution_owner: Option<std::sync::MutexGuard<'static, ()>>,
    viewport: Option<((u16, u16), std::time::Instant)>,
    applied_viewport: Option<(u16, u16)>,
}

impl Desktop for WindowsDesktop {
    fn check(&mut self) -> Result<()> {
        let current = current_desktop_name().context("Windows 当前桌面暂不可访问")?;
        if self.capture.is_none()
            || self
                .binding
                .as_ref()
                .is_none_or(|binding| binding.name != current)
        {
            self.control.abandon();
            self.capture.take();
            self.binding.take();
            self.changed = true;
            let binding = DesktopBinding::open()?;
            self.binding = Some(binding);
            self.capture = Some(Capture {
                screen: ScreenDc::open()?,
                scaled: None,
                window: None,
            });
        }
        check_desktop()
    }

    fn changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    fn recoverable(&self) -> bool {
        privilege::effective_system()
    }

    fn display_scale(&mut self, scale: u32) {
        if self.display_scale != Some(scale) {
            self.display_scale = Some(scale);
            self.viewport = None;
            self.applied_viewport = None;
        }
    }

    fn viewport(&mut self, size: PtySize) -> bool {
        if self.resolution_owner.is_none()
            || !matches!(&self.source, Source::Display(display) if display.primary)
            || self
                .binding
                .as_ref()
                .is_none_or(|binding| binding.name != "Default")
        {
            return false;
        }
        let size = (size.pixel_width, size.pixel_height);
        if size.0 < 320 || size.1 < 200 {
            self.viewport = None;
            return false;
        }
        let size = (size.0.clamp(640, 3840), size.1.clamp(480, 3840));
        let now = std::time::Instant::now();
        if self.viewport.is_none_or(|(previous, _)| previous != size) {
            self.viewport = Some((size, now));
            return false;
        }
        if self.applied_viewport == Some(size)
            || self.viewport.is_some_and(|(_, since)| {
                now.duration_since(since) < std::time::Duration::from_millis(500)
            })
        {
            return false;
        }
        self.applied_viewport = Some(size);
        self.release();
        // Remember DPI before resolution changes can alter the driver's range.
        if let Some(scale) = self.display_scale {
            match display::primary() {
                Ok(device) => {
                    if let Err(error) = self.dpi.remember(&device) {
                        tracing::warn!(%error, "Windows DPI 查询不可用");
                    }
                    if let Err(error) = self.resolution.resize(u32::from(size.0), u32::from(size.1))
                    {
                        tracing::warn!(%error, "Windows 自动分辨率不可用，继续缩放画面");
                    }
                    if let Err(error) = self.dpi.apply(&device, scale) {
                        tracing::warn!(%error, scale, "Windows DPI 同步不可用，保留当前缩放");
                    }
                }
                Err(error) => tracing::warn!(%error, "Windows 自动显示配置不可用"),
            }
        } else if let Err(error) = self.resolution.resize(u32::from(size.0), u32::from(size.1)) {
            tracing::warn!(%error, "Windows 自动分辨率不可用，继续缩放画面");
        }
        // Invalidate old coordinates even if a driver only partially applied a mode.
        true
    }

    fn frame(&mut self, width: u32, divisor: usize) -> Result<Frame> {
        let source = if self
            .binding
            .as_ref()
            .is_some_and(|binding| binding.name == "Winlogon")
        {
            &self.lock_source
        } else {
            &self.source
        };
        let secure = self
            .binding
            .as_ref()
            .is_some_and(|binding| binding.name == "Winlogon");
        let result = self
            .capture
            .as_mut()
            .context("Windows capture is unavailable")?
            .frame(source, source.bounds()?, width, divisor, secure);
        if result.is_err() {
            self.capture.take();
        }
        result
    }

    fn input(&mut self, event: Event, view: View, stopped: impl Fn() -> bool) -> Result<()> {
        // Never deliver input against coordinates from a different desktop.
        if self.binding.as_ref().map(|binding| binding.name.as_str())
            != current_desktop_name().as_deref()
        {
            return Ok(());
        }
        let source = if self
            .binding
            .as_ref()
            .is_some_and(|binding| binding.name == "Winlogon")
        {
            &self.lock_source
        } else {
            &self.source
        };
        self.control.handle(event, source, view, stopped)
    }

    fn release(&mut self) {
        if self.binding.as_ref().map(|binding| binding.name.as_str())
            == current_desktop_name().as_deref()
        {
            self.control.release();
        } else {
            self.control.abandon();
        }
    }
}

impl Drop for WindowsDesktop {
    fn drop(&mut self) {
        self.release();
        self.control.abandon();
        // Restore while we still own both the desktop binding and resize lock.
        drop(std::mem::take(&mut self.resolution));
        drop(std::mem::take(&mut self.dpi));
    }
}

pub fn stream(
    options: &Options,
    sizes: watch::Receiver<PtySize>,
    input: mpsc::Receiver<Zeroizing<Vec<u8>>>,
    output: &mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    // This worker owns the capture DCs and uses physical coordinates throughout.
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let binding = DesktopBinding::open()?;
    let source = Source::select(options)?;
    let lock_source = Source::Display(
        monitors()?
            .into_iter()
            .find(|monitor| monitor.primary)
            .context("Windows primary display is unavailable")?,
    );
    let capture = Capture {
        screen: ScreenDc::open()?,
        scaled: None,
        window: None,
    };
    desktop::stream(
        WindowsDesktop {
            source,
            capture: Some(capture),
            control: control::Control::default(),
            binding: Some(binding),
            lock_source,
            changed: false,
            resolution: display::DesktopResolution::default(),
            dpi: dpi::DesktopDpi::default(),
            display_scale: None,
            resolution_owner: if !options.share_read_only && options.share_window.is_none() {
                RESOLUTION_OWNER.try_lock().ok()
            } else {
                None
            },
            viewport: None,
            applied_viewport: None,
        },
        options,
        sizes,
        input,
        output,
    )
}
