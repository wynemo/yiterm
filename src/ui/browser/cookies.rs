use super::*;
use serde::{Deserialize, Serialize};
use std::{cell::Cell, io::Write};
use webview2_com::GetCookiesCompletedHandler;
use windows::core::{HSTRING, Interface};
use zeroize::{Zeroize, Zeroizing};

thread_local! {
    // All ordinary pages use one profile on the UI thread. Restore only when
    // a new browser process starts, never over a live page's newer cookies.
    static RESTORED_PROCESS: Cell<u32> = const { Cell::new(0) };
    static CLOSING: Cell<usize> = const { Cell::new(0) };
    static NEXT_SNAPSHOT: Cell<u64> = const { Cell::new(0) };
    static SAVED_SNAPSHOT: Cell<u64> = const { Cell::new(0) };
}

pub(super) fn closing() -> bool {
    CLOSING.get() != 0
}

struct ClosingController(ICoreWebView2Controller);

impl Drop for ClosingController {
    fn drop(&mut self) {
        let _ = unsafe { self.0.Close() };
        CLOSING.set(CLOSING.get() - 1);
    }
}

#[derive(Serialize, Deserialize)]
struct SessionCookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    http_only: bool,
    secure: bool,
    same_site: i32,
}

impl Drop for SessionCookie {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}

pub(super) struct CookiePersistence {
    manager: ICoreWebView2CookieManager,
    path: PathBuf,
}

impl CookiePersistence {
    pub(super) fn new(webview: &ICoreWebView2, directory: &Path) -> anyhow::Result<Rc<Self>> {
        let manager = unsafe { webview.cast::<ICoreWebView2_2>()?.CookieManager()? };
        let this = Rc::new(Self {
            manager,
            path: directory.join("session-cookies.dpapi"),
        });
        let mut process = 0;
        unsafe {
            webview.BrowserProcessId(&mut process)?;
        }
        if RESTORED_PROCESS.get() != process {
            if let Err(error) = this.restore() {
                tracing::warn!(%error, "Could not restore WebView2 session cookies");
            }
            RESTORED_PROCESS.set(process);
        }
        tracing::info!(profile = %directory.display(), process, "Using persistent WebView2 profile");
        Ok(this)
    }

    fn restore(&self) -> anyhow::Result<()> {
        let data = match std::fs::read(&self.path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let data = protect(&data, false)?;
        let cookies: Vec<SessionCookie> = serde_json::from_slice(&data)?;
        for saved in &cookies {
            unsafe {
                let cookie = self.manager.CreateCookie(
                    &HSTRING::from(&saved.name),
                    &HSTRING::from(&saved.value),
                    &HSTRING::from(&saved.domain),
                    &HSTRING::from(&saved.path),
                )?;
                // Keep these as session cookies; do not invent an expiry date.
                cookie.SetExpires(-1.)?;
                cookie.SetIsHttpOnly(saved.http_only)?;
                cookie.SetIsSecure(saved.secure)?;
                cookie.SetSameSite(COREWEBVIEW2_COOKIE_SAME_SITE_KIND(saved.same_site))?;
                self.manager.AddOrUpdateCookie(&cookie)?;
            }
        }
        tracing::info!(count = cookies.len(), "Restored WebView2 session cookies");
        Ok(())
    }

    pub(super) fn checkpoint(self: &Rc<Self>) {
        self.capture(None);
    }

    pub(super) fn close(self: &Rc<Self>, controller: ICoreWebView2Controller) {
        CLOSING.set(CLOSING.get() + 1);
        self.capture(Some(ClosingController(controller)));
    }

    fn capture(self: &Rc<Self>, closing: Option<ClosingController>) {
        let sequence = NEXT_SNAPSHOT.get() + 1;
        NEXT_SNAPSHOT.set(sequence);
        let this = self.clone();
        let handler = GetCookiesCompletedHandler::create(Box::new(move |result, cookies| {
            // Keep the controller alive until the final snapshot is on disk.
            let _closing = closing;
            if sequence < SAVED_SNAPSHOT.get() {
                return Ok(());
            }
            let result = result.map_err(anyhow::Error::from).and_then(|()| {
                let cookies = cookies.context("WebView2 returned no cookie list")?;
                this.save(&cookies)?;
                SAVED_SNAPSHOT.set(sequence);
                Ok(())
            });
            if let Err(error) = result {
                tracing::warn!(%error, "Could not save WebView2 session cookies");
            }
            Ok(())
        }));
        // Empty URI enumerates the entire profile, including HttpOnly cookies.
        if let Err(error) = unsafe { self.manager.GetCookies(windows::core::w!(""), &handler) } {
            tracing::warn!(%error, "Could not enumerate WebView2 session cookies");
        }
    }

    fn save(&self, list: &ICoreWebView2CookieList) -> anyhow::Result<()> {
        let mut count = 0;
        unsafe {
            list.Count(&mut count)?;
        }
        let mut cookies = Vec::new();
        for index in 0..count {
            unsafe {
                let cookie = list.GetValueAtIndex(index)?;
                let mut session = BOOL(0);
                cookie.IsSession(&mut session)?;
                if !session.as_bool() {
                    continue;
                }
                let mut http_only = BOOL(0);
                let mut secure = BOOL(0);
                let mut same_site = COREWEBVIEW2_COOKIE_SAME_SITE_KIND(0);
                cookie.IsHttpOnly(&mut http_only)?;
                cookie.IsSecure(&mut secure)?;
                cookie.SameSite(&mut same_site)?;
                let read = |getter: unsafe fn(
                    &ICoreWebView2Cookie,
                    *mut PWSTR,
                ) -> windows::core::Result<()>| {
                    let mut value = PWSTR::null();
                    getter(&cookie, &mut value)?;
                    Ok::<_, windows::core::Error>(webview2_com::take_pwstr(value))
                };
                cookies.push(SessionCookie {
                    name: read(ICoreWebView2Cookie::Name)?,
                    value: read(ICoreWebView2Cookie::Value)?,
                    domain: read(ICoreWebView2Cookie::Domain)?,
                    path: read(ICoreWebView2Cookie::Path)?,
                    http_only: http_only.as_bool(),
                    secure: secure.as_bool(),
                    same_site: same_site.0,
                });
            }
        }
        let json = Zeroizing::new(serde_json::to_vec(&cookies)?);
        let encrypted = protect(&json, true)?;
        // Replace the whole snapshot, including an empty one after logout.
        let mut file = tempfile::NamedTempFile::new_in(self.path.parent().unwrap())?;
        file.write_all(&encrypted)?;
        file.as_file().sync_all()?;
        file.persist(&self.path).map_err(|error| error.error)?;
        tracing::debug!(count = cookies.len(), "Saved WebView2 session cookies");
        Ok(())
    }
}

fn protect(bytes: &[u8], encrypt: bool) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len().try_into()?,
        pbData: bytes.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    unsafe {
        let ok = if encrypt {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let result = Zeroizing::new(if output.cbData == 0 {
            Vec::new()
        } else {
            let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
            std::ptr::write_bytes(output.pbData, 0, output.cbData as usize);
            result
        });
        LocalFree(output.pbData.cast());
        Ok(result)
    }
}
