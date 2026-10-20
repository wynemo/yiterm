use super::*;
use anyhow::{Context as _, ensure};
use gpui::Subscription;
use gpui_component::{
    Disableable,
    input::{Input, InputEvent, InputState},
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{path::Path, rc::Rc};
use webview2_com::{
    CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, Microsoft::Web::WebView2::Win32::*,
    NavigationCompletedEventHandler, NavigationStartingEventHandler,
    NewWindowRequestedEventHandler,
};
use windows::{
    Win32::Foundation::{E_POINTER, HWND, RECT},
    core::{BOOL, PCWSTR, PWSTR},
};

const TOOLBAR_HEIGHT: f32 = 50.;

struct WebViewState {
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
    navigation_token: i64,
    navigation_completed_token: i64,
    new_window_token: Option<i64>,
}

impl Drop for WebViewState {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .webview
                .remove_NavigationStarting(self.navigation_token);
            let _ = self
                .webview
                .remove_NavigationCompleted(self.navigation_completed_token);
            if let Some(token) = self.new_window_token {
                let _ = self.webview.remove_NewWindowRequested(token);
            }
            let _ = self.controller.Close();
        }
    }
}

pub(super) struct CloseBrowser;
struct OpenBrowserTab(String);
impl gpui::EventEmitter<OpenBrowserTab> for BrowserPage {}

/// A terminal tab owns a side panel. Pages share SSH; VNC uses isolated profiles.
pub(crate) struct BrowserUi {
    tunnel: Rc<yiterm::native_ssh::SocksTunnel>,
    ssh_pid: u32,
    user_data: Rc<Option<tempfile::TempDir>>,
    pages: Vec<(u64, gpui::Entity<BrowserPage>)>,
    active: usize,
    next_id: u64,
    visible: bool,
    maximized: bool,
    pub(super) split_state: gpui::Entity<gpui_component::resizable::ResizableState>,
}

impl gpui::EventEmitter<CloseBrowser> for BrowserUi {}

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
            user_data: Rc::new(
                tempfile::Builder::new()
                    .prefix("yiterm-webview-")
                    .tempdir()
                    .ok(),
            ),
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
        cx.subscribe_in(
            &page,
            window,
            |this, _, event: &OpenBrowserTab, window, cx| {
                let visible = this.visible;
                this.open_tab(event.0.clone(), window, cx);
                this.set_visible(visible, cx);
            },
        )
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
        self.pages[index]
            .1
            .update(cx, |page, _| page.set_visible(false));
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
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_tab("about:blank".into(), window, cx)
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
    _user_data: Rc<Option<tempfile::TempDir>>,
    address: gpui::Entity<InputState>,
    status: Option<String>,
    _address_subscription: Option<Subscription>,
}

impl BrowserPage {
    pub(super) fn new(
        tunnel: Rc<yiterm::native_ssh::SocksTunnel>,
        ssh_pid: u32,
        user_data: Rc<Option<tempfile::TempDir>>,
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
            Rc::new(
                tempfile::Builder::new()
                    .prefix("yiterm-vnc-")
                    .tempdir()
                    .ok(),
            )
        } else {
            user_data
        };
        let address = cx.new(|cx| InputState::new(window, cx).default_value(initial_url.clone()));
        let (sender, receiver) = async_channel::bounded::<anyhow::Result<WebViewState>>(1);
        let (navigation_sender, navigation_receiver) = async_channel::unbounded();
        let (popup_sender, popup_receiver) = async_channel::unbounded();
        let status = Some("Starting WebView2…".to_owned());
        let parent = Self::parent_hwnd(window);
        if let Some(error) = bridge_error {
            let _ = sender.try_send(Err(error));
        } else {
            match (parent, user_data.as_ref().as_ref()) {
                (Ok(parent), Some(user_data)) => {
                    if let Err(error) = Self::start_webview(
                        parent,
                        if is_vnc { 0 } else { tunnel.port() },
                        &initial_url,
                        user_data.path(),
                        sender.clone(),
                        navigation_sender.clone(),
                    ) {
                        let _ = sender.try_send(Err(error));
                    }
                }
                (Err(error), _) => {
                    let _ = sender.try_send(Err(error));
                }
                (_, None) => {
                    let _ = sender.try_send(Err(anyhow::anyhow!(
                        "Cannot create a temporary WebView2 profile directory"
                    )));
                }
            }
        }
        // Retain the proxy/profile until an in-flight native initialization has
        // delivered (and closed) its controller, even if the page is closed early.
        let initializing_resources = (tunnel.clone(), user_data.clone(), vnc.clone());
        let mut this = Self {
            _tunnel: tunnel,
            _ssh_pid: ssh_pid,
            _vnc: vnc,
            webview: None,
            visible: false,
            _user_data: user_data,
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
            let _initializing_resources = initializing_resources;
            let result = receiver.recv().await.unwrap_or_else(|_| {
                Err(anyhow::anyhow!("WebView2 initialization was cancelled"))
            });
            let _ = this.update(cx, |this, cx| {
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
                        this.status = Some(format!(
                            "Cannot initialize WebView2. Install the Microsoft Edge WebView2 Runtime. {error:#}"
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
                    .update(cx, |_, cx| cx.emit(OpenBrowserTab(uri)))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        this
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
    ) -> anyhow::Result<()> {
        let folder: Vec<u16> = user_data_path
            .to_string_lossy()
            .encode_utf16()
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
                let controller_handler = CreateCoreWebView2ControllerCompletedHandler::create(
                    Box::new(move |result, controller| {
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
                            controller,
                            &controller_initial_url,
                            controller_navigation_sender,
                            proxy_port == 0,
                        );
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
    ) -> anyhow::Result<WebViewState> {
        unsafe {
            controller.SetBounds(RECT::default())?;
            controller.SetIsVisible(false)?;
        }
        let webview = unsafe { controller.CoreWebView2()? };
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
        let navigation_completed =
            NavigationCompletedEventHandler::create(Box::new(move |_, args| {
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
            .when(self._vnc.is_some(), |element| {
                element.child(
                    div()
                        .h(px(TOOLBAR_HEIGHT))
                        .flex()
                        .items_center()
                        .px(px(8.))
                        .child("SSH VNC"),
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
