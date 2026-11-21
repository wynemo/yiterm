//! Startup-only RDP to console handoff. No child process or installed service.
use anyhow::{Result, ensure};
use std::{
    mem::size_of,
    ptr::null_mut,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    System::{RemoteDesktop::*, Shutdown::LockWorkStation, Threading::*},
};

fn session_id() -> Result<u32> {
    let mut id = 0;
    ensure!(
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut id) } != 0,
        "无法读取当前会话：{}",
        std::io::Error::last_os_error()
    );
    ensure!(
        id != 0,
        "锁屏共享必须从交互用户会话启动，不能在 Session 0 启动"
    );
    Ok(id)
}

fn protocol(id: u32) -> Result<u16> {
    let mut buffer = null_mut();
    let mut bytes = 0;
    ensure!(
        unsafe {
            WTSQuerySessionInformationW(
                null_mut(),
                id,
                WTSClientProtocolType,
                &mut buffer,
                &mut bytes,
            )
        } != 0,
        "无法读取会话 {id} 的显示连接：{}",
        std::io::Error::last_os_error()
    );
    let value = if !buffer.is_null() && bytes as usize >= size_of::<u16>() {
        Some(unsafe { buffer.read_unaligned() })
    } else {
        None
    };
    unsafe { WTSFreeMemory(buffer.cast()) };
    value.ok_or_else(|| anyhow::anyhow!("会话 {id} 的显示连接信息不完整"))
}

// Enable only on our own SYSTEM token, and restore before serving the network.
struct ConnectPrivilege {
    token: HANDLE,
    previous: TOKEN_PRIVILEGES,
}
impl ConnectPrivilege {
    fn enter() -> Result<Self> {
        let mut token = null_mut();
        ensure!(
            unsafe {
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES,
                    &mut token,
                )
            } != 0,
            "无法打开会话交接权限：{}",
            std::io::Error::last_os_error()
        );
        let mut guard = Self {
            token,
            previous: unsafe { std::mem::zeroed() },
        };
        let mut desired: TOKEN_PRIVILEGES = unsafe { std::mem::zeroed() };
        desired.PrivilegeCount = 1;
        let name: Vec<u16> = "SeTcbPrivilege\0".encode_utf16().collect();
        ensure!(
            unsafe {
                LookupPrivilegeValueW(null_mut(), name.as_ptr(), &mut desired.Privileges[0].Luid)
            } != 0,
            "无法查询会话交接权限"
        );
        desired.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
        let mut length = 0;
        ensure!(
            unsafe {
                AdjustTokenPrivileges(
                    token,
                    0,
                    &desired,
                    size_of::<TOKEN_PRIVILEGES>() as u32,
                    &mut guard.previous,
                    &mut length,
                )
            } != 0
                && unsafe { GetLastError() } != ERROR_NOT_ALL_ASSIGNED,
            "无法启用会话交接权限：{}",
            std::io::Error::last_os_error()
        );
        Ok(guard)
    }
}
impl Drop for ConnectPrivilege {
    fn drop(&mut self) {
        unsafe {
            if self.previous.PrivilegeCount != 0
                && AdjustTokenPrivileges(self.token, 0, &self.previous, 0, null_mut(), null_mut())
                    == 0
            {
                std::process::abort();
            }
            CloseHandle(self.token);
        }
    }
}

fn locked() -> bool {
    super::current_desktop_name().is_some_and(|name| name.eq_ignore_ascii_case("Winlogon"))
}
fn lock_session() -> Result<()> {
    if locked() {
        return Ok(());
    }
    ensure!(
        unsafe { LockWorkStation() } != 0,
        "无法锁定交接会话：{}",
        std::io::Error::last_os_error()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if locked() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("会话锁定未完成，未启动远程共享")
}

/// Called after the one-time SYSTEM restart, before capture/input or TCP bind.
pub fn prepare() -> Result<()> {
    let source = session_id()?;
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    if source == console && protocol(source)? == 0 {
        println!("已在控制台会话 {source}，无需交接。");
        return Ok(());
    }
    ensure!(
        super::privilege::effective_system(),
        "自动会话交接需要锁屏模式的 SYSTEM 自重启"
    );
    ensure!(
        protocol(source)? == 2,
        "会话 {source} 不是 RDP 或活动控制台，无法自动交接"
    );
    ensure!(
        console != u32::MAX && console != source,
        "控制台正在切换，请稍后重新启动共享"
    );
    println!(
        "正在将 RDP 会话 {source} 交给控制台 {console}；RDP 将断开，交接后保持锁屏。请保持显示器或 HDMI 显示模拟器连接。"
    );
    // Lock first as well as afterwards: never deliberately expose an unlocked
    // user desktop on the physical display during the handoff.
    lock_session()?;
    {
        let _privilege = ConnectPrivilege::enter()?;
        let password = [0u16];
        ensure!(
            unsafe { WTSConnectSessionW(source, console, password.as_ptr(), 1) } != 0,
            "RDP 会话 {source} 交给控制台 {console} 失败（会话已锁定）：{}",
            std::io::Error::last_os_error()
        );
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if unsafe { WTSGetActiveConsoleSessionId() } == source && protocol(source)? == 0 {
            lock_session()?;
            println!("会话 {source} 已交给控制台并确认锁屏，可从客户端连接解锁。");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("会话 {source} 尚未成为活动控制台，未启动远程共享")
}
