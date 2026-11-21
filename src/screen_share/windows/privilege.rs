//! Administrator sharing process launch; no resident helper or system service.
//! Only dedicated capture/input workers may enter this identity. Never retain
//! an impersonation across an await or use it on a network/runtime thread.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::OsStr,
    marker::PhantomData,
    mem::{size_of, size_of_val},
    os::windows::ffi::OsStrExt,
    ptr::null_mut,
    rc::Rc,
};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    System::{Diagnostics::ToolHelp::*, RemoteDesktop::ProcessIdToSessionId, Threading::*},
};

struct Handle(HANDLE);
impl Handle {
    fn new(value: HANDLE) -> Result<Self> {
        ensure!(
            !value.is_null() && value != INVALID_HANDLE_VALUE,
            "{}",
            std::io::Error::last_os_error()
        );
        Ok(Self(value))
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn process_token() -> Result<Handle> {
    let mut handle = null_mut();
    ensure!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) } != 0,
        "无法读取进程权限：{}",
        std::io::Error::last_os_error()
    );
    Handle::new(handle)
}
fn system_token(token: HANDLE) -> Result<bool> {
    // TOKEN_USER is followed by variable SID data; keep the buffer aligned.
    let mut data = [0usize; 64];
    let mut length = 0;
    ensure!(
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                data.as_mut_ptr().cast(),
                size_of_val(&data) as u32,
                &mut length,
            )
        } != 0,
        "无法读取令牌身份：{}",
        std::io::Error::last_os_error()
    );
    let user = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() };
    Ok(unsafe { IsWellKnownSid(user.User.Sid, WinLocalSystemSid) } != 0)
}
pub fn effective_system() -> bool {
    let mut token = null_mut();
    if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) } != 0 {
        let token = Handle(token);
        return system_token(token.0).unwrap_or(false);
    }
    if unsafe { GetLastError() } != ERROR_NO_TOKEN {
        return false;
    }
    process_token()
        .and_then(|token| system_token(token.0))
        .unwrap_or(false)
}
pub fn elevated() -> Result<bool> {
    let token = process_token()?;
    let mut info: TOKEN_ELEVATION = unsafe { std::mem::zeroed() };
    let mut length = 0;
    ensure!(
        unsafe {
            GetTokenInformation(
                token.0,
                TokenElevation,
                (&mut info as *mut TOKEN_ELEVATION).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut length,
            )
        } != 0,
        "无法读取管理员状态：{}",
        std::io::Error::last_os_error()
    );
    Ok(info.TokenIsElevated != 0)
}

/// !Send + !Sync: this guard must be dropped on the thread that entered it.
/// Windows requires process shutdown if reverting impersonation fails.
pub struct ThreadIdentity {
    active: bool,
    _thread_only: PhantomData<Rc<()>>,
}
impl ThreadIdentity {
    pub fn enter() -> Result<Self> {
        let mut guard = Self {
            active: false,
            _thread_only: PhantomData,
        };
        let mut existing = null_mut();
        if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut existing) } != 0 {
            let _existing = Handle(existing);
            anyhow::bail!("桌面工作线程已有模拟身份，拒绝覆盖");
        }
        ensure!(
            unsafe { GetLastError() } == ERROR_NO_TOKEN,
            "无法检查桌面线程身份"
        );
        if system_token(process_token()?.0)? {
            return Ok(guard);
        }
        ensure!(elevated()?, "锁屏共享需要以管理员身份启动");
        // Enable SeDebugPrivilege on a private copy of this thread's token;
        // never change the shared process token's privileges.
        ensure!(
            unsafe { ImpersonateSelf(SecurityImpersonation) } != 0,
            "无法创建桌面线程身份：{}",
            std::io::Error::last_os_error()
        );
        guard.active = true;
        let mut raw = null_mut();
        ensure!(
            unsafe {
                OpenThreadToken(
                    GetCurrentThread(),
                    TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES,
                    1,
                    &mut raw,
                )
            } != 0,
            "无法打开桌面线程令牌"
        );
        let token = Handle::new(raw)?;
        let mut desired: TOKEN_PRIVILEGES = unsafe { std::mem::zeroed() };
        desired.PrivilegeCount = 1;
        let name: Vec<u16> = "SeDebugPrivilege".encode_utf16().chain(Some(0)).collect();
        ensure!(
            unsafe {
                LookupPrivilegeValueW(null_mut(), name.as_ptr(), &mut desired.Privileges[0].Luid)
            } != 0,
            "无法查询桌面访问权限"
        );
        desired.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
        ensure!(
            unsafe { AdjustTokenPrivileges(token.0, 0, &desired, 0, null_mut(), null_mut()) } != 0
                && unsafe { GetLastError() } != ERROR_NOT_ALL_ASSIGNED,
            "系统策略不允许桌面线程取得锁屏访问权限"
        );
        let system = current_session_token(false)?;
        // The duplicated token contains only TOKEN_QUERY/TOKEN_IMPERSONATE
        // handle access, and cannot be used by this code to launch a process.
        ensure!(
            unsafe { SetThreadToken(null_mut(), system.0) } != 0,
            "系统拒绝桌面线程模拟：{}",
            std::io::Error::last_os_error()
        );
        ensure!(effective_system(), "桌面线程身份校验失败");
        Ok(guard)
    }
}
impl Drop for ThreadIdentity {
    fn drop(&mut self) {
        if self.active && unsafe { RevertToSelf() } == 0 {
            std::process::abort();
        }
    }
}
fn current_session_token(primary: bool) -> Result<Handle> {
    let mut session = 0;
    ensure!(
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } != 0 && session != 0,
        "需要在交互式 Windows 登录会话中启动"
    );
    let snapshot = Handle::new(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) })?;
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
    while more {
        let end = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        let mut candidate_session = 0;
        if String::from_utf16_lossy(&entry.szExeFile[..end]).eq_ignore_ascii_case("winlogon.exe")
            && unsafe { ProcessIdToSessionId(entry.th32ProcessID, &mut candidate_session) } != 0
            && candidate_session == session
        {
            let process = Handle::new(unsafe {
                OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID)
            })?;
            let mut raw = null_mut();
            ensure!(
                unsafe { OpenProcessToken(process.0, TOKEN_DUPLICATE | TOKEN_QUERY, &mut raw) }
                    != 0,
                "系统拒绝锁屏令牌访问：{}",
                std::io::Error::last_os_error()
            );
            let source = Handle::new(raw)?;
            if !system_token(source.0)? {
                more = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
                continue;
            }
            let mut impersonation = null_mut();
            ensure!(
                unsafe {
                    DuplicateTokenEx(
                        source.0,
                        if primary {
                            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY
                        } else {
                            TOKEN_QUERY | TOKEN_IMPERSONATE
                        },
                        null_mut(),
                        SecurityImpersonation,
                        if primary {
                            TokenPrimary
                        } else {
                            TokenImpersonation
                        },
                        &mut impersonation,
                    )
                } != 0,
                "无法创建桌面线程身份：{}",
                std::io::Error::last_os_error()
            );
            return Handle::new(impersonation);
        }
        more = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
    }
    None.context("未找到当前会话的 Windows 登录进程")
}

/// Owns only the dedicated sharing process; stopping a GUI host terminates it.
pub struct SharingProcess(Handle);
unsafe impl Send for SharingProcess {}
impl SharingProcess {
    pub fn detach(self) {
        let raw = self.0.0;
        std::mem::forget(self);
        unsafe {
            CloseHandle(raw);
        }
    }

    pub fn is_finished(&self) -> bool {
        unsafe { WaitForSingleObject(self.0.0, 0) != WAIT_TIMEOUT }
    }
}
impl Drop for SharingProcess {
    fn drop(&mut self) {
        unsafe {
            TerminateProcess(self.0.0, 0);
        }
    }
}

pub fn launch(
    arguments: Vec<std::ffi::OsString>,
    identity_dir: std::path::PathBuf,
) -> Result<SharingProcess> {
    ensure!(
        elevated()?,
        "请以管理员身份启动 YiTerm，以启用 Windows 锁屏解锁"
    );
    let process = std::thread::spawn(move || -> Result<SharingProcess> {
        let _identity = ThreadIdentity::enter()?;
        let primary = current_session_token(true)?;
        enable_thread_privilege("SeAssignPrimaryTokenPrivilege")?;
        enable_thread_privilege("SeIncreaseQuotaPrivilege")?;
        let executable = std::env::current_exe()?;
        let arguments = std::iter::once(executable.as_os_str().to_owned()).chain(arguments);
        let mut command = Vec::new();
        for arg in arguments {
            if !command.is_empty() {
                command.push(b' ' as u16);
            }
            command.extend(quote(&arg));
        }
        command.push(0);
        let directory = wide(std::env::current_dir()?);
        let mut environment = std::collections::BTreeMap::new();
        for (key, value) in std::env::vars_os() {
            if !key
                .to_string_lossy()
                .eq_ignore_ascii_case("YITERM_SHARE_IDENTITY_DIR")
            {
                environment.insert(key, value);
            }
        }
        environment.insert(
            "YITERM_SHARE_IDENTITY_DIR".into(),
            identity_dir.into_os_string(),
        );
        let mut environment: Vec<_> = environment.into_iter().collect();
        environment.sort_by_cached_key(|(key, _)| key.to_string_lossy().to_uppercase());
        let mut environment_block = Vec::new();
        for (key, value) in environment {
            environment_block.extend(key.encode_wide());
            environment_block.push(b'=' as u16);
            environment_block.extend(value.encode_wide());
            environment_block.push(0);
        }
        environment_block.push(0);
        let mut desktop = wide("winsta0\\default");
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = size_of::<STARTUPINFOW>() as u32;
        startup.lpDesktop = desktop.as_mut_ptr();
        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe {
                CreateProcessAsUserW(
                    primary.0,
                    wide(&executable).as_ptr(),
                    command.as_mut_ptr(),
                    null_mut(),
                    null_mut(),
                    0,
                    CREATE_NEW_CONSOLE | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
                    environment_block.as_ptr().cast(),
                    directory.as_ptr(),
                    &startup,
                    &mut info,
                )
            } != 0,
            "无法以 SYSTEM 身份重新启动当前程序：{}",
            std::io::Error::last_os_error()
        );
        let process = Handle::new(info.hProcess)?;
        let thread = Handle::new(info.hThread)?;
        if unsafe { ResumeThread(thread.0) } == u32::MAX {
            unsafe {
                TerminateProcess(process.0, 1);
            }
            anyhow::bail!("无法启动替代进程：{}", std::io::Error::last_os_error());
        }
        Ok(SharingProcess(process))
    })
    .join()
    .map_err(|_| anyhow::anyhow!("SYSTEM 自重启线程异常退出"))??;
    Ok(process)
}

fn enable_thread_privilege(name: &str) -> Result<()> {
    let mut raw = null_mut();
    ensure!(
        unsafe {
            OpenThreadToken(
                GetCurrentThread(),
                TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES,
                0,
                &mut raw,
            )
        } != 0,
        "Unable to open SYSTEM launch token: {}",
        std::io::Error::last_os_error()
    );
    let token = Handle::new(raw)?;
    let mut privilege: TOKEN_PRIVILEGES = unsafe { std::mem::zeroed() };
    privilege.PrivilegeCount = 1;
    ensure!(
        unsafe {
            LookupPrivilegeValueW(
                null_mut(),
                wide(name).as_ptr(),
                &mut privilege.Privileges[0].Luid,
            )
        } != 0,
        "Unable to find launch privilege"
    );
    privilege.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
    ensure!(
        unsafe { AdjustTokenPrivileges(token.0, 0, &privilege, 0, null_mut(), null_mut()) } != 0
            && unsafe { GetLastError() } != ERROR_NOT_ALL_ASSIGNED,
        "SYSTEM launch privilege unavailable: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}
fn quote(value: &OsStr) -> Vec<u16> {
    let mut out = vec![b'"' as u16];
    let mut slashes = 0;
    for c in value.encode_wide() {
        if c == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        if c == b'"' as u16 {
            out.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2 + 1));
        } else {
            out.extend(std::iter::repeat_n(b'\\' as u16, slashes));
        }
        slashes = 0;
        out.push(c);
    }
    out.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    out.push(b'"' as u16);
    out
}
