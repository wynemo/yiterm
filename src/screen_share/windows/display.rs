//! Temporary, driver-supported display modes owned by the exclusive control session.
use anyhow::{Result, ensure};
use std::{
    mem::size_of,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Graphics::Gdi::*,
    UI::WindowsAndMessaging::{GetSystemMetrics, SM_CMONITORS},
};

#[derive(Default)]
pub struct DesktopResolution {
    original: Option<([u16; 32], DEVMODEW)>,
    applied: Option<(u32, u32)>,
}

pub(crate) fn current(device: &[u16; 32]) -> Result<DEVMODEW> {
    let mut mode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    ensure!(
        unsafe { EnumDisplaySettingsW(device.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) } != 0,
        "无法读取 Windows 当前分辨率"
    );
    Ok(mode)
}

pub(crate) fn primary() -> Result<[u16; 32]> {
    ensure!(
        unsafe { GetSystemMetrics(SM_CMONITORS) } == 1,
        "自动分辨率目前支持单显示器；多屏继续等比例缩放画面"
    );
    let mut index = 0;
    loop {
        let mut device = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        ensure!(
            unsafe { EnumDisplayDevicesW(null(), index, &mut device, 0) } != 0,
            "无法找到 Windows 主显示器"
        );
        if device.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE != 0 {
            return Ok(device.DeviceName);
        }
        index += 1;
    }
}

fn modes(device: &[u16; 32], active: &DEVMODEW) -> Vec<DEVMODEW> {
    let mut output = Vec::new();
    for index in 0..4096 {
        let mut mode = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if unsafe { EnumDisplaySettingsW(device.as_ptr(), index, &mut mode) } == 0 {
            break;
        }
        if mode.dmBitsPerPel == active.dmBitsPerPel
            && (640..=3840).contains(&mode.dmPelsWidth)
            && (480..=3840).contains(&mode.dmPelsHeight)
            && unsafe {
                mode.Anonymous1.Anonymous2.dmDisplayOrientation
                    == active.Anonymous1.Anonymous2.dmDisplayOrientation
            }
        {
            output.push(mode);
        }
    }
    output
}

fn apply(device: &[u16; 32], mode: &DEVMODEW) -> Result<()> {
    let result = unsafe {
        ChangeDisplaySettingsExW(device.as_ptr(), mode, null_mut(), CDS_FULLSCREEN, null())
    };
    ensure!(
        result == DISP_CHANGE_SUCCESSFUL,
        "Windows 无法切换到 {}×{}（系统返回 {result}），继续缩放原画面",
        mode.dmPelsWidth,
        mode.dmPelsHeight
    );
    Ok(())
}

impl DesktopResolution {
    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        let device = primary()?;
        let active = current(&device)?;
        let mut requested = active;
        requested.dmPelsWidth = width.clamp(640, 3840);
        requested.dmPelsHeight = height.clamp(480, 3840);
        requested.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
        let supported = modes(&device, &active);
        // Some virtual displays accept arbitrary desktop sizes. Ask the driver
        // first so these can follow the window exactly, like dynamic RDP resize.
        let exact_supported = unsafe {
            ChangeDisplaySettingsExW(device.as_ptr(), &requested, null_mut(), CDS_TEST, null())
        } == DISP_CHANGE_SUCCESSFUL;
        let mode = if exact_supported {
            &requested
        } else {
            supported
                .iter()
                .min_by(|a, b| {
                    let distance = |mode: &DEVMODEW| {
                        mode_distance((mode.dmPelsWidth, mode.dmPelsHeight), (width, height))
                    };
                    distance(a).total_cmp(&distance(b)).then_with(|| {
                        a.dmDisplayFrequency
                            .abs_diff(active.dmDisplayFrequency)
                            .cmp(&b.dmDisplayFrequency.abs_diff(active.dmDisplayFrequency))
                    })
                })
                .ok_or_else(|| anyhow::anyhow!("显示器没有可用的自适应分辨率，继续缩放原画面"))?
        };
        if (mode.dmPelsWidth, mode.dmPelsHeight) == (active.dmPelsWidth, active.dmPelsHeight) {
            return Ok(());
        }
        // Retain the pre-session mode, not the previous resize step. Never write the registry.
        if self.original.is_none() {
            self.original = Some((device, active));
        }
        apply(&device, mode)?;
        self.applied = Some((mode.dmPelsWidth, mode.dmPelsHeight));
        let actual = current(&device)?;
        self.applied = Some((actual.dmPelsWidth, actual.dmPelsHeight));
        ensure!(
            self.applied == Some((mode.dmPelsWidth, mode.dmPelsHeight)),
            "显示驱动未应用请求的分辨率，实际为 {}×{}",
            actual.dmPelsWidth,
            actual.dmPelsHeight
        );
        tracing::info!(
            width = mode.dmPelsWidth,
            height = mode.dmPelsHeight,
            "Windows desktop adapted to viewer"
        );
        Ok(())
    }
}

impl Drop for DesktopResolution {
    fn drop(&mut self) {
        if let Some((device, original)) = self.original.take()
            && let Ok(active) = current(&device)
            // Preserve a subsequent local/manual display change.
            && self.applied == Some((active.dmPelsWidth, active.dmPelsHeight))
            && let Err(error) = apply(&device, &original)
        {
            tracing::warn!(%error, "恢复 Windows 原分辨率失败");
        }
    }
}

fn mode_distance(size: (u32, u32), target: (u32, u32)) -> f64 {
    (f64::from(size.0) / f64::from(target.0)).ln().powi(2)
        + (f64::from(size.1) / f64::from(target.1)).ln().powi(2)
}
