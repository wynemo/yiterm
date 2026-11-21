//! Windows has no documented per-monitor DPI setter. These runtime-checked
//! DisplayConfig packets (-3/-4) are also used by Windows DPI helper projects.
//! Unsupported systems fail explicitly; never substitute bitmap enlargement.
use anyhow::{Result, bail, ensure};
use std::mem::size_of;
use windows_sys::Win32::{Devices::Display::*, Foundation::ERROR_INSUFFICIENT_BUFFER};

#[repr(C)]
#[derive(Default)]
struct DpiGet {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    minimum: i32,
    current: i32,
    maximum: i32,
}
#[repr(C)]
struct DpiSet {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    relative: i32,
}
const _: () = assert!(size_of::<DpiGet>() == 32 && size_of::<DpiSet>() == 24);

fn source(device: &[u16; 32]) -> Result<DISPLAYCONFIG_DEVICE_INFO_HEADER> {
    for _ in 0..3 {
        let (mut path_count, mut mode_count) = (0, 0);
        let result = unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        };
        ensure!(
            result == 0 && path_count > 0 && path_count <= 64 && mode_count <= 256,
            "Windows 显示驱动不支持 DPI 协商（查询显示路径返回 {result}）"
        );
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        let result = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };
        if result == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        ensure!(result == 0, "无法读取 Windows DPI 显示路径：{result}");
        for path in paths.iter().take(path_count as usize) {
            let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                },
                ..Default::default()
            };
            if unsafe { DisplayConfigGetDeviceInfo(&mut name.header) } == 0
                && name.viewGdiDeviceName == *device
            {
                return Ok(name.header);
            }
        }
        bail!("无法匹配 Windows 主显示器的 DPI 配置");
    }
    bail!("Windows 显示路径正在变化，请重试连接")
}

fn read(device: &[u16; 32]) -> Result<DpiGet> {
    let mut packet = DpiGet {
        header: source(device)?,
        ..Default::default()
    };
    packet.header.r#type = -3;
    packet.header.size = size_of::<DpiGet>() as u32;
    let result = unsafe { DisplayConfigGetDeviceInfo(&mut packet.header) };
    ensure!(
        result == 0,
        "Windows 不支持读取桌面 DPI（系统返回 {result}）"
    );
    dpi_percent(packet.minimum, packet.current, packet.maximum)?;
    Ok(packet)
}

fn percent(packet: &DpiGet) -> Result<u32> {
    dpi_percent(packet.minimum, packet.current, packet.maximum)
}

fn set(device: &[u16; 32], scale: u32) -> Result<u32> {
    let active = read(device)?;
    if percent(&active)? == scale {
        return Ok(scale);
    }
    let relative = dpi_relative(active.minimum, active.maximum, scale)?;
    let mut packet = DpiSet {
        header: active.header,
        relative,
    };
    packet.header.r#type = -4;
    packet.header.size = size_of::<DpiSet>() as u32;
    let result = unsafe { DisplayConfigSetDeviceInfo(&packet.header) };
    ensure!(
        result == 0,
        "Windows 拒绝设置桌面缩放 {scale}%（系统返回 {result}）"
    );
    let actual = percent(&read(device)?)?;
    ensure!(
        actual == scale,
        "Windows DPI 未生效：请求 {scale}%，实际 {actual}%"
    );
    Ok(actual)
}

#[derive(Default)]
pub struct DesktopDpi {
    original: Option<([u16; 32], u32)>,
    applied: Option<u32>,
}
impl DesktopDpi {
    pub fn remember(&mut self, device: &[u16; 32]) -> Result<()> {
        if let Some((saved, _)) = self.original {
            ensure!(saved == *device, "控制期间主显示器已变化，请重新连接");
        } else {
            self.original = Some((*device, percent(&read(device)?)?));
        }
        Ok(())
    }
    pub fn apply(&mut self, device: &[u16; 32], scale: u32) -> Result<u32> {
        self.remember(device)?;
        let active = read(device)?;
        // Match the local display as closely as the remote driver permits.
        let scale = DPI_STEPS
            .iter()
            .copied()
            .filter(|candidate| dpi_relative(active.minimum, active.maximum, *candidate).is_ok())
            .min_by_key(|candidate| candidate.abs_diff(scale))
            .ok_or_else(|| anyhow::anyhow!("Windows 没有可用的 DPI 缩放档位"))?;
        dpi_relative(active.minimum, active.maximum, scale)?;
        // Save expected state before writing, so even failed verification can restore.
        self.applied = Some(scale);
        let actual = set(device, scale)?;
        tracing::info!(scale = actual, "Windows DPI adapted to viewer display");
        Ok(actual)
    }
}
impl Drop for DesktopDpi {
    fn drop(&mut self) {
        if let Some((device, original)) = self.original.take()
            && let Some(applied) = self.applied
        {
            match read(&device).and_then(|active| percent(&active)) {
                Ok(current) if current == applied => {
                    if let Err(error) = set(&device, original) {
                        tracing::warn!(%error, "恢复 Windows 原 DPI 失败");
                    }
                }
                Ok(_) => {} // Preserve a subsequent local display change.
                Err(error) => tracing::warn!(%error, "无法确认 Windows 原 DPI 恢复条件"),
            }
        }
    }
}

const DPI_STEPS: [u32; 12] = [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];
fn dpi_percent(minimum: i32, relative: i32, maximum: i32) -> Result<u32> {
    ensure!(
        (-11..=0).contains(&minimum) && maximum >= 0 && relative >= minimum && relative <= maximum,
        "Windows 返回无效的 DPI 范围"
    );
    DPI_STEPS
        .get((i64::from(relative) - i64::from(minimum)) as usize)
        .copied()
        .ok_or_else(|| anyhow::anyhow!("未知 Windows DPI 档位"))
}
fn dpi_relative(minimum: i32, maximum: i32, percent: u32) -> Result<i32> {
    let index = DPI_STEPS
        .iter()
        .position(|value| *value == percent)
        .ok_or_else(|| anyhow::anyhow!("Windows 不支持 {percent}% 缩放档位"))?
        as i32;
    let relative = index + minimum;
    dpi_percent(minimum, relative, maximum)?;
    Ok(relative)
}
