use super::*;
use anyhow::{Context as _, ensure};
use gpui::Subscription;
use gpui_component::{
    Disableable,
    input::{Input, InputEvent, InputState},
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    rc::Rc,
};
use webview2_com::{
    CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, Microsoft::Web::WebView2::Win32::*,
    NavigationCompletedEventHandler, NavigationStartingEventHandler,
    NewWindowRequestedEventHandler,
};
use windows::{
    Win32::Foundation::{E_POINTER, ERROR_INVALID_STATE, HWND, RECT},
    core::{BOOL, PCWSTR, PWSTR},
};

mod cookies;
mod lifecycle;
use cookies::CookiePersistence;
use windows::core::Interface;

pub(super) async fn finish_shutdown() {
    lifecycle::finish_shutdown().await;
}

const TOOLBAR_HEIGHT: f32 = 50.;

// Only VNC profiles are temporary. Web pages always use the same on-disk data.
enum BrowserProfile {
    Persistent(PathBuf),
    Temporary(tempfile::TempDir),
}

impl BrowserProfile {
    fn persistent() -> anyhow::Result<Self> {
        let path = directories::BaseDirs::new()
            .context("Cannot locate local browser data directory")?
            .data_local_dir()
            .join("yiterm/browser");
        std::fs::create_dir_all(&path)
            .with_context(|| format!("Cannot create browser data directory: {}", path.display()))?;
        Ok(Self::Persistent(path))
    }

    fn temporary() -> anyhow::Result<Self> {
        Ok(Self::Temporary(
            tempfile::Builder::new()
                .prefix("yiterm-vnc-")
                .tempdir()
                .context("Cannot create temporary VNC browser data directory")?,
        ))
    }

    fn path(&self) -> &Path {
        match self {
            Self::Persistent(path) => path,
            Self::Temporary(directory) => directory.path(),
        }
    }
}

// Keep the native view's dependencies alive through asynchronous teardown,
// even after the GPUI page and browser panel have already been removed.
#[derive(Clone)]
struct BrowserLifetime {
    executor: gpui::ForegroundExecutor,
    _resources: (
        Rc<yiterm::native_ssh::SocksTunnel>,
        Rc<anyhow::Result<BrowserProfile>>,
        Option<Rc<yiterm::vnc::VncBridge>>,
    ),
}

impl BrowserLifetime {
    fn close_failed_controller(self, controller: ICoreWebView2Controller) {
        let closing = cookies::Closing::begin();
        self.executor
            .clone()
            .spawn(async move {
                let _lifetime = self;
                let _closing = closing;
                if let Err(error) = unsafe { controller.Close() } {
                    tracing::warn!(%error, "Could not close failed WebView2 controller");
                }
            })
            .detach();
    }
}

struct WebViewState {
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
    navigation_token: i64,
    navigation_completed_token: i64,
    new_window_token: Option<i64>,
    cookies: Option<Rc<CookiePersistence>>,
    lifetime: BrowserLifetime,
}

impl Drop for WebViewState {
    fn drop(&mut self) {
        let lifetime = self.lifetime.clone();
        let executor = lifetime.executor.clone();
        let controller = self.controller.clone();
        let webview = self.webview.clone();
        let cookies = self.cookies.clone();
        let navigation_token = self.navigation_token;
        let navigation_completed_token = self.navigation_completed_token;
        let new_window_token = self.new_window_token;
        if let Some(cookies) = &cookies {
            cookies.stop_checkpoints();
        }
        let closing = cookies::Closing::begin();
        // Drop can run during a GPUI update or a WebView2 callback. Return
        // first, then perform all native teardown on the same STA UI thread.
        executor
            .spawn(async move {
                let _lifetime = lifetime;
                let _closing = closing;
                unsafe {
                    let _ = webview.remove_NavigationStarting(navigation_token);
                    let _ = webview.remove_NavigationCompleted(navigation_completed_token);
                    if let Some(token) = new_window_token {
                        let _ = webview.remove_NewWindowRequested(token);
                    }
                }
                if let Some(cookies) = cookies {
                    if let Err(error) = cookies.final_snapshot().await {
                        tracing::warn!(%error, "Could not save final WebView2 session cookies");
                    }
                }
                tracing::info!("Closing WebView2 controller");
                if let Err(error) = unsafe { controller.Close() } {
                    tracing::warn!(%error, "Could not close WebView2 controller");
                }
                drop(webview);
                drop(controller);
                tracing::info!("WebView2 controller closed");
            })
            .detach();
    }
}

pub(super) struct CloseBrowser;
pub(super) struct OpenWebTab(pub String);
struct OpenBrowserTab(String);
impl gpui::EventEmitter<OpenBrowserTab> for BrowserPage {}

/// A terminal tab owns a side panel. Pages share SSH; VNC uses isolated profiles.
pub(crate) struct BrowserUi {
    tunnel: Rc<yiterm::native_ssh::SocksTunnel>,
    ssh_pid: u32,
    user_data: Rc<anyhow::Result<BrowserProfile>>,
    pages: Vec<(u64, gpui::Entity<BrowserPage>)>,
    active: usize,
    next_id: u64,
    visible: bool,
    maximized: bool,
    pub(super) split_state: gpui::Entity<gpui_component::resizable::ResizableState>,
}

impl gpui::EventEmitter<CloseBrowser> for BrowserUi {}
impl gpui::EventEmitter<OpenWebTab> for BrowserUi {}

impl BrowserUi {
    pub(super) fn new(
        tunnel: yiterm::native_ssh::SocksTunnel,
        ssh_pid: u32,
        initial_url: String,
        window: &mut Window,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        let mut this = Self {
            tunnel: Rc::new(tunnel),
            ssh_pid,
            user_data: Rc::new(BrowserProfile::persistent()),
            pages: Vec::new(),
            active: 0,
            next_id: 0,
            visible: false,
            maximized: false,
            split_state: cx.new(|_| gpui_component::resizable::ResizableState::default()),
        };
        this.open_tab(initial_url, window, cx);
        this
    }

    pub(super) fn ssh_pid(&self) -> u32 {
        self.ssh_pid
    }

    pub(super) fn close(&mut self, cx: &mut ViewContext<Self>) {
        self.set_visible(false, cx);
        for (_, page) in &self.pages {
            page.update(cx, |page, _| page.close());
        }
        self.pages.clear();
    }

    /// Release the shared profile while leaving independent VNC pages running.
    pub(super) fn close_web_tabs(&mut self, cx: &mut ViewContext<Self>) -> bool {
        let active_id = self.pages.get(self.active).map(|(id, _)| *id);
        self.pages.retain(|(_, page)| {
            if page.read(cx)._vnc.is_some() {
                true
            } else {
                page.update(cx, |page, _| page.close());
                false
            }
        });
        self.active = active_id
            .and_then(|id| self.pages.iter().position(|(page_id, _)| *page_id == id))
            .unwrap_or(0);
        self.set_visible(self.visible, cx);
        cx.notify();
        self.pages.is_empty()
    }

    pub(super) fn maximized(&self) -> bool {
        self.maximized
    }

    pub(super) fn open_tab(
        &mut self,
        url: String,
        window: &mut Window,
        cx: &mut ViewContext<Self>,
    ) {
        self.set_visible(false, cx);
        let page = cx.new(|cx| {
            BrowserPage::new(
                self.tunnel.clone(),
                self.ssh_pid,
                self.user_data.clone(),
                url,
                window,
                cx,
            )
        });
        cx.observe(&page, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&page, |_, _, event: &OpenBrowserTab, cx| {
            cx.emit(OpenWebTab(event.0.clone()));
        })
        .detach();
        self.pages.push((self.next_id, page));
        self.next_id += 1;
        self.active = self.pages.len() - 1;
        self.set_visible(true, cx);
        cx.notify();
    }

    pub(super) fn set_visible(&mut self, visible: bool, cx: &mut ViewContext<Self>) {
        self.visible = visible;
        for (index, (_, page)) in self.pages.iter().enumerate() {
            page.update(cx, |page, _| {
                page.set_visible(visible && index == self.active)
            });
        }
    }

    fn select(&mut self, index: usize, cx: &mut ViewContext<Self>) {
        self.active = index;
        self.set_visible(self.visible, cx);
        cx.notify();
    }

    fn close_tab(&mut self, index: usize, cx: &mut ViewContext<Self>) {
        self.pages[index].1.update(cx, |page, _| page.close());
        self.pages.remove(index);
        if self.pages.is_empty() {
            cx.emit(CloseBrowser);
        } else {
            if index < self.active {
                self.active -= 1;
            }
            self.active = self.active.min(self.pages.len() - 1);
            self.set_visible(self.visible, cx);
        }
        cx.notify();
    }
}

impl Render for BrowserUi {
    fn render(&mut self, _: &mut Window, cx: &mut ViewContext<Self>) -> impl IntoElement {
        self.set_visible(true, cx);
        let tabs = self
            .pages
            .iter()
            .enumerate()
            .map(|(index, (id, page))| {
                let address = if page.read(cx)._vnc.is_some() {
                    "SSH VNC".to_owned()
                } else {
                    page.read(cx).address.read(cx).value().to_string()
                };
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .min_w_0()
                    .bg(rgb(if index == self.active {
                        0xFFFFFF
                    } else {
                        0xE8EAED
                    }))
                    .child(
                        Button::new(("browser-tab", *id))
                            .small()
                            .label(if address == "about:blank" {
                                "New Tab".to_owned()
                            } else {
                                let mut caption: String = address.chars().take(28).collect();
                                if address.chars().count() > 28 {
                                    caption.push('…');
                                }
                                caption
                            })
                            .on_click(cx.listener(move |this, _, _, cx| this.select(index, cx))),
                    )
                    .child(
                        Button::new(("browser-tab-close", *id))
                            .small()
                            .label("×")
                            .on_click(cx.listener(move |this, _, _, cx| this.close_tab(index, cx))),
                    )
            })
            .collect::<Vec<_>>();
        div()
            .size_full()
            .flex()
            .flex_col()
            .min_w_0()
            .overflow_hidden()
            .bg(rgb(0xF3F4F6))
            .text_color(rgb(0x202124))
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .gap(px(4.))
                    .child(
                        div()
                            .id("browser-tabs")
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .overflow_x_scroll()
                            .children(tabs),
                    )
                    .child(
                        Button::new("browser-new-tab")
                            .small()
                            .label("+")
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(OpenWebTab("about:blank".into()));
                            })),
                    )
                    .child(
                        Button::new("browser-maximize")
                            .small()
                            .label(if self.maximized {
                                "Restore"
                            } else {
                                "Maximize"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.maximized = !this.maximized;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("browser-close")
                            .small()
                            .label("Close")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_visible(false, cx);
                                cx.emit(CloseBrowser);
                            })),
                    ),
            )
            .when_some(self.pages.get(self.active), |this, (_, page)| {
                this.child(div().flex_1().min_h_0().child(page.clone()))
            })
    }
}

struct BrowserPage {
    _tunnel: Rc<yiterm::native_ssh::SocksTunnel>,
    _ssh_pid: u32,
    _vnc: Option<Rc<yiterm::vnc::VncBridge>>,
    webview: Option<WebViewState>,
    visible: bool,
    _user_data: Rc<anyhow::Result<BrowserProfile>>,
    closed: bool,
    address: gpui::Entity<InputState>,
    status: Option<String>,
    _address_subscription: Option<Subscription>,
}

impl BrowserPage {
    pub(super) fn new(
        tunnel: Rc<yiterm::native_ssh::SocksTunnel>,
        ssh_pid: u32,
        user_data: Rc<anyhow::Result<BrowserProfile>>,
        initial_url: String,
        window: &mut Window,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        let is_vnc = initial_url == "yiterm:vnc";
        let bridge_result = is_vnc.then(|| yiterm::vnc::VncBridge::start(tunnel.port()));
        let (vnc, bridge_error) = match bridge_result {
            Some(Ok(bridge)) => (Some(Rc::new(bridge)), None),
            Some(Err(error)) => (None, Some(error)),
            None => (None, None),
        };
        let initial_url = vnc
            .as_ref()
            .map(|bridge| bridge.url.clone())
            .unwrap_or(initial_url);
        // Local VNC pages must never share the SSH browser's proxy/profile.
        let user_data = if is_vnc {
            Rc::new(BrowserProfile::temporary())
        } else {
            user_data
        };
        let address = cx.new(|cx| InputState::new(window, cx).default_value(initial_url.clone()));
        let (sender, receiver) = async_channel::bounded::<anyhow::Result<WebViewState>>(1);
        let (navigation_sender, navigation_receiver) = async_channel::unbounded();
        let (popup_sender, popup_receiver) = async_channel::unbounded();
        let status = Some("Starting WebView2…".to_owned());
        let parent = Self::parent_hwnd(window);
        // Keep the tunnel and temporary VNC profile alive through native init,
        // including when the page is closed before its callbacks return.
        let initializing_resources = (tunnel.clone(), user_data.clone(), vnc.clone());
        let executor = cx.foreground_executor().clone();
        let mut this = Self {
            _tunnel: tunnel,
            _ssh_pid: ssh_pid,
            _vnc: vnc,
            webview: None,
            visible: false,
            _user_data: user_data,
            closed: false,
            address,
            status,
            _address_subscription: None,
        };
        let subscription =
            cx.subscribe_in(&this.address, window, |this, input, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let value = input.read(cx).value().to_string();
                    this.navigate(&value, window, cx);
                }
            });
        this._address_subscription = Some(subscription);
        cx.spawn_in(window, async move |this, cx| {
            let (tunnel, user_data, _vnc) = initializing_resources;
            let started = std::time::Instant::now();
            let mut bridge_error = bridge_error;
            let result = loop {
                if !this.update(cx, |this, _| !this.closed).unwrap_or(false) {
                    return;
                }
                // A previous last tab may still be saving its session cookies.
                // Do not restore an older snapshot while that close is pending.
                if !is_vnc && cookies::closing() {
                    if started.elapsed() >= Duration::from_secs(10) {
                        break Err(anyhow::anyhow!(
                            "Timed out waiting for the previous browser to save session cookies"
                        ));
                    }
                    gpui::Timer::after(Duration::from_millis(25)).await;
                    continue;
                }
                let start = if let Some(error) = bridge_error.take() {
                    Err(error)
                } else {
                    match (&parent, user_data.as_ref()) {
                        (Ok(parent), Ok(profile)) => Self::start_webview(
                            *parent,
                            if is_vnc { 0 } else { tunnel.port() },
                            &initial_url,
                            profile.path(),
                            sender.clone(),
                            navigation_sender.clone(),
                            BrowserLifetime {
                                executor: executor.clone(),
                                _resources: (tunnel.clone(), user_data.clone(), _vnc.clone()),
                            },
                        ),
                        (Err(error), _) | (_, Err(error)) => Err(anyhow::anyhow!("{error:#}")),
                    }
                };
                let result = match start {
                    Ok(()) => receiver.recv().await.unwrap_or_else(|_| {
                        Err(anyhow::anyhow!("WebView2 initialization was cancelled"))
                    }),
                    Err(error) => Err(error),
                };
                // Closing the last controller does not immediately stop its
                // browser process. Retry options conflicts while it releases
                // the shared directory; never fall back to a temporary profile.
                let options_conflict = result.as_ref().err().is_some_and(|error| {
                    error.downcast_ref::<windows::core::Error>().is_some_and(|error| {
                        error.code() == windows::core::HRESULT::from_win32(ERROR_INVALID_STATE.0)
                    })
                });
                if !is_vnc && options_conflict && started.elapsed() < Duration::from_secs(10) {
                    gpui::Timer::after(Duration::from_millis(100)).await;
                    continue;
                }
                break result;
            };
            let _ = this.update(cx, |this, cx| {
                if this.closed {
                    return;
                }
                match result {
                    Ok(mut webview) => {
                        let handler = NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
                            if let Some(args) = args {
                                unsafe { args.SetHandled(true)?; }
                                let mut uri = PWSTR::null();
                                unsafe { args.Uri(&mut uri)?; }
                                let uri = webview2_com::take_pwstr(uri);
                                if !is_vnc && (uri == "about:blank" || normalize_url(&uri).is_ok()) {
                                    let _ = popup_sender.try_send(uri);
                                }
                            }
                            Ok(())
                        }));
                        let mut token = 0;
                        match unsafe { webview.webview.add_NewWindowRequested(&handler, &mut token) } {
                            Ok(()) => webview.new_window_token = Some(token),
                            Err(error) => tracing::warn!(%error, "Could not handle browser new-window requests"),
                        }
                        this.webview = Some(webview);
                        if let Some(state) = &this.webview {
                            let _ = unsafe { state.controller.SetIsVisible(this.visible) };
                        }
                        this.status = None;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Could not initialize WebView2");
                        this.status = Some(format!(
                            "Cannot initialize WebView2: {error:#}. Check the WebView2 Runtime and close any other YiTerm browser using this data directory."
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(status) = navigation_receiver.recv().await {
                if this
                    .update_in(cx, |this, window, cx| {
                        if this.closed {
                            return;
                        }
                        if status.is_none()
                            && let Some(state) = &this.webview
                        {
                            let mut uri = PWSTR::null();
                            if unsafe { state.webview.Source(&mut uri) }.is_ok() {
                                let uri = webview2_com::take_pwstr(uri);
                                this.address
                                    .update(cx, |input, cx| input.set_value(uri, window, cx));
                            }
                        }
                        this.status = status;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(uri) = popup_receiver.recv().await {
                if this
                    .update(cx, |this, cx| {
                        if !this.closed {
                            cx.emit(OpenBrowserTab(uri));
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(Duration::from_secs(5)).await;
                let running = this.update(cx, |this, _| {
                    if this.closed {
                        return false;
                    }
                    if let Some(state) = &this.webview
                        && let Some(cookies) = &state.cookies
                    {
                        cookies.checkpoint();
                    }
                    true
                });
                if !running.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
        this
    }

    fn close(&mut self) {
        self.closed = true;
        self.set_visible(false);
        self.webview.take();
    }

    fn parent_hwnd(window: &Window) -> anyhow::Result<HWND> {
        match HasWindowHandle::window_handle(window)
            .context("getting the browser window handle")?
            .as_raw()
        {
            RawWindowHandle::Win32(handle) => Ok(HWND(handle.hwnd.get() as *mut _)),
            handle => anyhow::bail!("Expected a Win32 browser window, got {handle:?}"),
        }
    }

    fn start_webview(
        parent: HWND,
        proxy_port: u16,
        initial_url: &str,
        user_data_path: &Path,
        sender: async_channel::Sender<anyhow::Result<WebViewState>>,
        navigation_sender: async_channel::Sender<Option<String>>,
        lifetime: BrowserLifetime,
    ) -> anyhow::Result<()> {
        let folder: Vec<u16> = user_data_path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let options = CoreWebView2EnvironmentOptions::default();
        // Chromium applies host mapping to the proxy endpoint too, even when
        // it is an IP literal. Keep the local proxy reachable while target
        // hostnames are resolved remotely by SOCKS5 / SSH.
        let arguments = format!(
            "--proxy-server=socks5://127.0.0.1:{proxy_port} --proxy-bypass-list=<-loopback> --host-resolver-rules=\"MAP * ~NOTFOUND, EXCLUDE 127.0.0.1\" --disable-dns-prefetch --disable-quic --force-webrtc-ip-handling-policy=disable_non_proxied_udp"
        );
        let arguments = if proxy_port == 0 {
            "--no-proxy-server".to_owned()
        } else {
            arguments
        };
        unsafe { options.set_additional_browser_arguments(arguments) };
        let options: ICoreWebView2EnvironmentOptions = options.into();
        let callback_sender = sender.clone();
        let callback_navigation_sender = navigation_sender.clone();
        let initial_url = initial_url.to_owned();
        let user_data_path = user_data_path.to_owned();
        let initializing = lifecycle::Initialization::begin();
        let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
            move |result, environment| {
                let environment = match result
                    .and_then(|()| environment.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                {
                    Ok(environment) => environment,
                    Err(error) => {
                        let _ = callback_sender.try_send(Err(error.into()));
                        return Ok(());
                    }
                };
                let controller_sender = callback_sender.clone();
                let controller_navigation_sender = callback_navigation_sender.clone();
                let controller_initial_url = initial_url.clone();
                let actual_data_path = (|| -> anyhow::Result<PathBuf> {
                    let mut folder = PWSTR::null();
                    unsafe {
                        environment
                            .cast::<ICoreWebView2Environment7>()?
                            .UserDataFolder(&mut folder)?;
                    }
                    Ok(PathBuf::from(webview2_com::take_pwstr(folder)))
                })();
                let controller_data_path = match actual_data_path {
                    Ok(path) => path,
                    Err(error) => {
                        let _ = callback_sender.try_send(Err(error));
                        return Ok(());
                    }
                };
                tracing::info!(requested = %user_data_path.display(), actual = %controller_data_path.display(), "WebView2 user data folder");
                if std::fs::canonicalize(&controller_data_path).ok()
                    != std::fs::canonicalize(&user_data_path).ok()
                {
                    tracing::warn!(
                        "WebView2 user data folder was overridden by runtime configuration or policy"
                    );
                }
                let controller_handler = CreateCoreWebView2ControllerCompletedHandler::create(
                    Box::new(move |result, controller| {
                        let _initializing = initializing;
                        let controller = match result.and_then(|()| {
                            controller.ok_or_else(|| windows::core::Error::from(E_POINTER))
                        }) {
                            Ok(controller) => controller,
                            Err(error) => {
                                let _ = controller_sender.try_send(Err(error.into()));
                                return Ok(());
                            }
                        };
                        let state = Self::finish_webview(
                            controller.clone(),
                            &controller_initial_url,
                            controller_navigation_sender,
                            proxy_port == 0,
                            &controller_data_path,
                            lifetime.clone(),
                        );
                        if state.is_err() {
                            lifetime.close_failed_controller(controller);
                        }
                        let _ = controller_sender.try_send(state);
                        Ok(())
                    }),
                );
                if let Err(error) =
                    unsafe { environment.CreateCoreWebView2Controller(parent, &controller_handler) }
                {
                    let _ = callback_sender.try_send(Err(error.into()));
                }
                Ok(())
            },
        ));
        unsafe {
            CreateCoreWebView2EnvironmentWithOptions(
                PCWSTR::null(),
                PCWSTR(folder.as_ptr()),
                &options,
                &handler,
            )?;
        }
        Ok(())
    }

    fn finish_webview(
        controller: ICoreWebView2Controller,
        initial_url: &str,
        navigation_sender: async_channel::Sender<Option<String>>,
        local_only: bool,
        user_data_path: &Path,
        lifetime: BrowserLifetime,
    ) -> anyhow::Result<WebViewState> {
        unsafe {
            controller.SetBounds(RECT::default())?;
            controller.SetIsVisible(false)?;
        }
        let webview = unsafe { controller.CoreWebView2()? };
        lifecycle::track(&webview);
        if !local_only {
            // Inspect the effective profile, not just the requested directory.
            let profile = unsafe { webview.cast::<ICoreWebView2_13>()?.Profile()? };
            let mut private = BOOL(0);
            let mut path = PWSTR::null();
            unsafe {
                profile.IsInPrivateModeEnabled(&mut private)?;
                profile.ProfilePath(&mut path)?;
            }
            let path = webview2_com::take_pwstr(path);
            ensure!(
                !private.as_bool(),
                "WebView2 opened an InPrivate profile; browser data cannot be persisted"
            );
            tracing::info!(profile = %path, in_private = private.as_bool(), "WebView2 storage profile");
        }
        let cookies = if local_only {
            None
        } else {
            Some(CookiePersistence::new(
                &webview,
                user_data_path,
                lifetime.executor.clone(),
            )?)
        };
        let local_vnc = local_only.then(|| url::Url::parse(initial_url).expect("local VNC URL"));
        let navigation = NavigationStartingEventHandler::create(Box::new(move |_, args| {
            if let Some(args) = args {
                let mut uri = PWSTR::null();
                unsafe { args.Uri(&mut uri)? };
                let uri = webview2_com::take_pwstr(uri);
                let allowed = uri.eq_ignore_ascii_case("about:blank")
                    || uri.starts_with("https://")
                    || uri.starts_with("http://");
                let allowed = allowed
                    && local_vnc.as_ref().is_none_or(|local| {
                        url::Url::parse(&uri).is_ok_and(|url| {
                            url.origin() == local.origin() && url.path() == local.path()
                        })
                    });
                if !allowed {
                    let mut cancel = BOOL(1);
                    unsafe { args.Cancel(&mut cancel)? };
                }
            }
            Ok(())
        }));
        let mut navigation_token = 0;
        unsafe {
            webview.add_NavigationStarting(&navigation, &mut navigation_token)?;
        }
        let navigation_cookies = cookies.clone();
        let navigation_completed =
            NavigationCompletedEventHandler::create(Box::new(move |_, args| {
                if let Some(cookies) = &navigation_cookies {
                    cookies.checkpoint();
                }
                if let Some(args) = args {
                    let mut is_success = BOOL(0);
                    unsafe { args.IsSuccess(&mut is_success)? };
                    if is_success.0 != 0 {
                        tracing::info!("WebView2 page navigation completed");
                        let _ = navigation_sender.try_send(None);
                    } else {
                        let mut error_status = COREWEBVIEW2_WEB_ERROR_STATUS(0);
                        unsafe { args.WebErrorStatus(&mut error_status)? };
                        tracing::warn!(error_status = error_status.0, "WebView2 navigation failed");
                        let _ = navigation_sender.try_send(Some(format!(
                            "Page failed to load (WebView2 error {}).",
                            error_status.0
                        )));
                    }
                }
                Ok(())
            }));
        let mut navigation_completed_token = 0;
        unsafe {
            webview
                .add_NavigationCompleted(&navigation_completed, &mut navigation_completed_token)?;
        }
        if !initial_url.is_empty() {
            Self::navigate_webview(&webview, initial_url)?;
        }
        Ok(WebViewState {
            controller,
            webview,
            navigation_token,
            navigation_completed_token,
            new_window_token: None,
            cookies,
            lifetime,
        })
    }

    fn navigate_webview(webview: &ICoreWebView2, value: &str) -> anyhow::Result<()> {
        let value = if value.eq_ignore_ascii_case("about:blank") {
            value.to_owned()
        } else {
            normalize_url(value)?
        };
        let wide: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
        unsafe { webview.Navigate(PCWSTR(wide.as_ptr()))? };
        Ok(())
    }

    fn navigate(&mut self, value: &str, window: &mut Window, cx: &mut ViewContext<Self>) {
        let result = self
            .webview
            .as_ref()
            .context("WebView2 is unavailable")
            .and_then(|state| Self::navigate_webview(&state.webview, value));
        match result {
            Ok(()) => {
                if let Ok(url) = if value.eq_ignore_ascii_case("about:blank") {
                    Ok("about:blank".to_owned())
                } else {
                    normalize_url(value)
                } {
                    self.address
                        .update(cx, |input, cx| input.set_value(url, window, cx));
                }
                self.status = None;
            }
            Err(error) => self.status = Some(format!("Cannot open address: {error:#}")),
        }
        cx.notify();
    }

    pub(super) fn set_visible(&mut self, visible: bool) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if let Some(state) = &self.webview
            && let Err(error) = unsafe { state.controller.SetIsVisible(visible) }
        {
            tracing::warn!(%error, "Could not change WebView2 visibility");
        }
    }

    fn current_url(&self) -> Option<String> {
        let state = self.webview.as_ref()?;
        let mut uri = PWSTR::null();
        unsafe { state.webview.Source(&mut uri) }.ok()?;
        crate::browser_url(&webview2_com::take_pwstr(uri))
    }

    fn go_back(&self) {
        if let Some(state) = &self.webview
            && let Err(error) = unsafe { state.webview.GoBack() }
        {
            tracing::debug!(%error, "WebView2 cannot navigate back");
        }
    }

    fn go_forward(&self) {
        if let Some(state) = &self.webview
            && let Err(error) = unsafe { state.webview.GoForward() }
        {
            tracing::debug!(%error, "WebView2 cannot navigate forward");
        }
    }

    fn reload(&self) {
        if let Some(state) = &self.webview
            && let Err(error) = unsafe { state.webview.Reload() }
        {
            tracing::debug!(%error, "Could not reload WebView2 page");
        }
    }
}

impl Render for BrowserPage {
    fn render(&mut self, _window: &mut Window, cx: &mut ViewContext<Self>) -> impl IntoElement {
        let status = self.status.clone();
        let controller = self.webview.as_ref().map(|state| state.controller.clone());
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0xF3F4F6))
            .text_color(rgb(0x202124))
            .when(self._vnc.is_none(), |element| {
                element.child(
                    div()
                        .h(px(TOOLBAR_HEIGHT))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .px(px(8.))
                        .bg(rgb(0xE8EAED))
                        .child(
                            Button::new("browser-back")
                                .label("Back")
                                .small()
                                .on_click(cx.listener(|this, _, _, _| this.go_back())),
                        )
                        .child(
                            Button::new("browser-forward")
                                .label("Forward")
                                .small()
                                .on_click(cx.listener(|this, _, _, _| this.go_forward())),
                        )
                        .child(
                            Button::new("browser-reload")
                                .label("Reload")
                                .small()
                                .on_click(cx.listener(|this, _, _, _| this.reload())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&self.address).small()),
                        )
                        .child(
                            Button::new("browser-go")
                                .label("Go")
                                .small()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    let value = this.address.read(cx).value().to_string();
                                    this.navigate(&value, window, cx);
                                })),
                        )
                        .child(
                            Button::new("browser-open-system")
                                .label("Open in System Browser")
                                .small()
                                .disabled(self.current_url().is_none())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(url) = this.current_url() {
                                        cx.open_url(&url);
                                    }
                                })),
                        ),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .when_some(status, |this, status| {
                        this.child(
                            div()
                                .flex_shrink_0()
                                .px(px(8.))
                                .py(px(4.))
                                .text_color(rgb(0x9B1C1C))
                                .child(status),
                        )
                    })
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, _| {
                                let Some(controller) = &controller else {
                                    return;
                                };
                                let scale = window.scale_factor();
                                let left = (f32::from(bounds.origin.x) * scale).round() as i32;
                                let top = (f32::from(bounds.origin.y) * scale).round() as i32;
                                let right = (f32::from(bounds.origin.x + bounds.size.width) * scale)
                                    .round() as i32;
                                let bottom = (f32::from(bounds.origin.y + bounds.size.height)
                                    * scale)
                                    .round() as i32;
                                let bounds = RECT {
                                    left,
                                    top,
                                    right: right.max(left),
                                    bottom: bottom.max(top),
                                };
                                if let Err(error) = unsafe { controller.SetBounds(bounds) } {
                                    tracing::warn!(%error, "Could not resize WebView2 content");
                                }
                            },
                        )
                        .flex_1()
                        .min_h_0(),
                    ),
            )
    }
}

fn normalize_url(value: &str) -> anyhow::Result<String> {
    let value = value.trim();
    ensure!(!value.is_empty(), "address is empty");
    let value = if value.contains("://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    let parsed = url::Url::parse(&value).context("address is not a valid URL")?;
    ensure!(
        matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some(),
        "only HTTP and HTTPS addresses are supported"
    );
    Ok(parsed.to_string())
}
