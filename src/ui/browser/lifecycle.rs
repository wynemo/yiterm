use super::*;
use std::cell::{Cell, RefCell};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
    System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
};

thread_local! {
    static INITIALIZING: Cell<usize> = const { Cell::new(0) };
    static PROCESSES: RefCell<Vec<BrowserProcess>> = const { RefCell::new(Vec::new()) };
}

pub(super) struct Initialization;

impl Initialization {
    pub(super) fn begin() -> Self {
        INITIALIZING.set(INITIALIZING.get() + 1);
        Self
    }
}

impl Drop for Initialization {
    fn drop(&mut self) {
        INITIALIZING.set(INITIALIZING.get() - 1);
    }
}

pub(super) fn track(webview: &ICoreWebView2) {
    match BrowserProcess::track(webview) {
        Ok(process) => PROCESSES.with_borrow_mut(|processes| {
            processes.retain(|process| !process.exited());
            if !processes.iter().any(|existing| existing.pid == process.pid) {
                processes.push(process);
            }
        }),
        Err(error) => tracing::warn!(%error, "Could not monitor WebView2 shutdown"),
    }
}

// Hold a process handle rather than polling a PID that Windows could reuse.
struct BrowserProcess {
    handle: HANDLE,
    pid: u32,
}

impl BrowserProcess {
    fn track(webview: &ICoreWebView2) -> anyhow::Result<Self> {
        let mut pid = 0;
        unsafe {
            webview.BrowserProcessId(&mut pid)?;
        }
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        ensure!(
            !handle.is_null(),
            "Cannot monitor WebView2 process {pid}: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self { handle, pid })
    }

    fn exited(&self) -> bool {
        unsafe { WaitForSingleObject(self.handle, 0) == WAIT_OBJECT_0 }
    }
}

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

pub(super) async fn finish_shutdown() {
    let started = std::time::Instant::now();
    // Keep the ordinary Windows message loop running. Blocking inside GPUI's
    // quit callback would prevent WebView2's COM completion callbacks running.
    while cookies::closing()
        || INITIALIZING.get() != 0
        || PROCESSES.with_borrow(|processes| processes.iter().any(|process| !process.exited()))
    {
        if started.elapsed() >= Duration::from_secs(10) {
            tracing::warn!(
                cookies_pending = cookies::closing(),
                "Timed out waiting for WebView2 shutdown; recent browser data may not be saved"
            );
            return;
        }
        gpui::Timer::after(Duration::from_millis(25)).await;
    }
    PROCESSES.with_borrow_mut(Vec::clear);
    tracing::info!("WebView2 shutdown completed");
}
