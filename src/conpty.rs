//! Load the release's matched ConPTY runtime before portable-pty initializes.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
};
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
    LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};

pub fn initialize() -> Result<()> {
    let executable = std::env::current_exe()?;
    let directory = executable.parent().context("Executable has no directory")?;
    let dll = directory.join("conpty.dll");
    if !dll.exists() && option_env!("YITERM_BUILD_REV").is_none() {
        tracing::info!(runtime = "system", "ConPTY initialized (development build)");
        return Ok(());
    }
    ensure!(
        dll.is_file() && directory.join("OpenConsole.exe").is_file(),
        "Missing ConPTY runtime: extract the complete Windows release ZIP alongside yiterm.exe"
    );
    let path: Vec<u16> = dll.as_os_str().encode_wide().chain(Some(0)).collect();
    // Pin the app-local module for process lifetime. portable-pty subsequently
    // loads this same module by basename, rather than falling back to kernel32.
    let module = unsafe {
        LoadLibraryExW(
            path.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if module.is_null() {
        return Err(std::io::Error::last_os_error()).context("Could not load bundled ConPTY");
    }
    for symbol in [
        c"CreatePseudoConsole",
        c"ResizePseudoConsole",
        c"ClosePseudoConsole",
    ] {
        ensure!(
            unsafe { GetProcAddress(module, symbol.as_ptr().cast()) }.is_some(),
            "Bundled ConPTY is missing {}",
            symbol.to_string_lossy()
        );
    }
    let mut actual = vec![0u16; 32768];
    let length = unsafe { GetModuleFileNameW(module, actual.as_mut_ptr(), actual.len() as u32) };
    ensure!(
        length > 0 && (length as usize) < actual.len(),
        "Cannot identify loaded ConPTY"
    );
    tracing::info!(
        "ConPTY runtime=bundled path={}",
        OsString::from_wide(&actual[..length as usize]).to_string_lossy()
    );
    Ok(())
}
