use super::*;
use anyhow::{Context as _, ensure};
use gpui::Subscription;
use gpui_component::input::{Input, InputEvent, InputState};
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
        };
        this.open_tab(initial_url, window, cx);
        this
    }

    pub(super) fn ssh_pid(&self) -> u32 {
        self.ssh_pid
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
                        .child(Button::new("browser-go").label("Go").small().on_click(
                            cx.listener(|this, _, window, cx| {
                                let value = this.address.read(cx).value().to_string();
                                this.navigate(&value, window, cx);
                            }),
                        )),
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

#[cfg(test)]
#[path = "../../tests/common/native_server.rs"]
mod native_server;

#[cfg(test)]
#[path = "../../tests/common/vnc_server.rs"]
mod vnc_server;

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use windows::Win32::{
        System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
        UI::WindowsAndMessaging::{
            DispatchMessageW, HWND_MESSAGE, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
        },
    };

    struct Password;
    impl yiterm::native_ssh::auth::Prompt for Password {
        fn ask(&self, _: &str, _: bool) -> anyhow::Result<String> {
            Ok("fixture-password".into())
        }
    }

    fn pump_until<T>(receiver: &async_channel::Receiver<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            match receiver.try_recv() {
                Ok(result) => return result,
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("WebView2 callback disconnected")
                }
                Err(async_channel::TryRecvError::Empty) => {}
            }
            assert!(Instant::now() < deadline, "WebView2 callback timed out");
            let mut message = MSG::default();
            unsafe {
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    struct CompositionView {
        _webview: std::rc::Rc<std::cell::RefCell<Option<WebViewState>>>,
    }

    impl Render for CompositionView {
        fn render(&mut self, _: &mut Window, _: &mut ViewContext<Self>) -> impl IntoElement {
            div().size_full().bg(rgb(0xC02020))
        }
    }

    #[test]
    fn webview_loads_visible_pixels_over_gpui() {
        use std::{cell::RefCell, rc::Rc, sync::Mutex};
        use windows::{
            Win32::{
                Foundation::{LPARAM, POINT, WPARAM},
                Graphics::Gdi::{ClientToScreen, GetDC, GetPixel, ReleaseDC},
                UI::WindowsAndMessaging::{
                    HWND_TOPMOST, PostMessageW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
                    SetWindowPos, WM_CLOSE,
                },
            },
            core::w,
        };
        fn red(color: u32) -> bool {
            let (r, g, b) = (color & 255, (color >> 8) & 255, (color >> 16) & 255);
            r > 100 && r > g * 2 && r > b * 2
        }
        fn green(color: u32) -> bool {
            let (r, g, b) = (color & 255, (color >> 8) & 255, (color >> 16) & 255);
            g > 100 && g > r * 2 && g > b * 2
        }
        let pixels = Arc::new(Mutex::new((0u32, 0u32)));
        let sampled = pixels.clone();
        let profile = tempfile::tempdir().unwrap();
        let profile_path = profile.path().to_owned();
        gpui::Application::new().run(move |cx: &mut gpui::App| {
            let bounds = Bounds::centered(None, size(px(640.), px(480.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                move |window, cx| {
                    let parent = BrowserPage::parent_hwnd(window).unwrap();
                    let live = Rc::new(RefCell::new(None));
                    let retained = live.clone();
                    let (sender, receiver) = async_channel::bounded(1);
                    let (navigation_sender, _navigation_receiver) = async_channel::unbounded();
                    BrowserPage::start_webview(
                        parent,
                        9,
                        "about:blank",
                        &profile_path,
                        sender,
                        navigation_sender,
                    )
                    .unwrap();
                    cx.spawn(async move |_| {
                        let state = receiver.recv().await.unwrap().unwrap();
                        unsafe {
                            state
                                .controller
                                .SetBounds(RECT {
                                    left: 100,
                                    top: 100,
                                    right: 400,
                                    bottom: 300,
                                })
                                .unwrap();
                            state
                                .webview
                                .NavigateToString(w!(
                                    "<html><body style='margin:0;height:100vh;background:#20c040'></body></html>"
                                ))
                                .unwrap();
                            state.controller.SetIsVisible(true).unwrap();
                        }
                        *retained.borrow_mut() = Some(state);
                    })
                    .detach();
                    let parent_raw = parent.0 as usize;
                    std::thread::spawn(move || {
                        let parent = HWND(parent_raw as *mut _);
                        unsafe {
                            SetWindowPos(
                                parent,
                                Some(HWND_TOPMOST),
                                0,
                                0,
                                0,
                                0,
                                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                            )
                            .unwrap()
                        };
                        for _ in 0..200 {
                            std::thread::sleep(Duration::from_millis(100));
                            let mut origin = POINT { x: 0, y: 0 };
                            let colors = unsafe {
                                let _ = ClientToScreen(parent, &mut origin);
                                let screen = GetDC(None);
                                let background = GetPixel(screen, origin.x + 30, origin.y + 50).0;
                                let browser = GetPixel(screen, origin.x + 250, origin.y + 200).0;
                                let _ = ReleaseDC(None, screen);
                                (background, browser)
                            };
                            *sampled.lock().unwrap() = colors;
                            if red(colors.0) && green(colors.1) {
                                break;
                            }
                        }
                        unsafe {
                            let _ = PostMessageW(Some(parent), WM_CLOSE, WPARAM(0), LPARAM(0));
                        }
                    });
                    cx.new(|_| CompositionView { _webview: live })
                },
            )
            .unwrap();
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
        let (background, browser) = *pixels.lock().unwrap();
        assert!(
            red(background),
            "GPUI background did not render: {background:#08x}"
        );
        assert!(
            green(browser),
            "WebView2 is covered by GPUI: background={background:#08x}, browser={browser:#08x}"
        );
    }

    fn script(state: &WebViewState, source: &str) -> String {
        let (sender, receiver) = async_channel::bounded(1);
        let handler =
            webview2_com::ExecuteScriptCompletedHandler::create(Box::new(move |result, value| {
                let _ = sender.try_send(result.map(|()| value));
                Ok(())
            }));
        let source: Vec<u16> = source.encode_utf16().chain(Some(0)).collect();
        unsafe {
            state
                .webview
                .ExecuteScript(PCWSTR(source.as_ptr()), &handler)
                .unwrap();
        }
        pump_until(&receiver).unwrap()
    }

    #[test]
    fn webview_loads_vnc_desktop_through_ssh() {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap() };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (_fixture, proxy_port, worker) = runtime.block_on(async {
            let fixture = native_server::Fixture::new().await;
            let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            *fixture.state.forward_target.lock().unwrap() = Some(target.local_addr().unwrap());
            tokio::spawn(async move {
                while let Ok((socket, _)) = target.accept().await {
                    tokio::spawn(vnc_server::serve(socket));
                }
            });
            let handle = Arc::new(
                yiterm::native_ssh::connect(fixture.config.clone(), Arc::new(Password))
                    .await
                    .unwrap(),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let worker = tokio::spawn(yiterm::native_ssh::socks::serve(listener, handle));
            (fixture, port, worker)
        });
        let bridge = yiterm::vnc::VncBridge::start(proxy_port).unwrap();
        let profile = tempfile::tempdir().unwrap();
        let (sender, receiver) = async_channel::bounded(1);
        let (navigation_sender, navigation_receiver) = async_channel::unbounded();
        BrowserPage::start_webview(
            HWND_MESSAGE,
            0,
            &bridge.url,
            profile.path(),
            sender,
            navigation_sender,
        )
        .unwrap();
        let state = pump_until(&receiver).unwrap();
        assert_eq!(pump_until(&navigation_receiver), None);
        let deadline = Instant::now() + Duration::from_secs(30);
        while script(&state, "typeof document.getElementById('form').onsubmit") != "\"function\"" {
            assert!(Instant::now() < deadline, "noVNC module did not load");
            std::thread::sleep(Duration::from_millis(30));
        }
        script(
            &state,
            "document.getElementById('host').value='remote-only.invalid'; document.getElementById('form').requestSubmit()",
        );
        loop {
            let pixel = script(
                &state,
                "(()=>{const c=document.querySelector('#screen canvas');return c && c.width===2 ? Array.from(c.getContext('2d').getImageData(0,0,1,1).data) : null})()",
            );
            if pixel == "[255,0,0,255]" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "noVNC framebuffer missing: {pixel}; status={}",
                script(&state, "document.getElementById('status').textContent")
            );
            std::thread::sleep(Duration::from_millis(30));
        }
        assert_eq!(
            script(&state, "document.getElementById('status').textContent"),
            "\"Connected\""
        );
        script(&state, "document.getElementById('disconnect').click()");
        drop(state);
        drop(bridge);
        worker.abort();
        unsafe {
            CoUninitialize();
        }
    }

    #[test]
    fn webview_loads_remote_hostname_and_localhost_through_ssh_socks() {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap() };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (fixture, proxy_port, worker) = runtime.block_on(async {
            let fixture = native_server::Fixture::new().await;
            let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            *fixture.state.forward_target.lock().unwrap() = Some(http.local_addr().unwrap());
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = http.accept().await {
                    tokio::spawn(async move {
                        let mut request = Vec::new();
                        while !request.ends_with(b"\r\n\r\n") {
                            match socket.read_u8().await {
                                Ok(byte) => request.push(byte),
                                Err(_) => return,
                            }
                        }
                        let body = b"<!doctype html><html><body>ssh-browser-fixture</body></html>";
                        let header = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                        let _ = socket.write_all(header.as_bytes()).await;
                        let _ = socket.write_all(body).await;
                        let _ = socket.shutdown().await;
                    });
                }
            });
            let handle = Arc::new(yiterm::native_ssh::connect(fixture.config.clone(), Arc::new(Password)).await.unwrap());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let worker = tokio::spawn(yiterm::native_ssh::socks::serve(listener, handle));
            (fixture, port, worker)
        });
        let profile = tempfile::tempdir().unwrap();
        let (sender, receiver) = async_channel::bounded(1);
        let (navigation_sender, navigation_receiver) = async_channel::unbounded();
        BrowserPage::start_webview(
            HWND_MESSAGE,
            proxy_port,
            "about:blank",
            profile.path(),
            sender,
            navigation_sender,
        )
        .unwrap();
        let state = pump_until(&receiver).unwrap();
        assert_eq!(pump_until(&navigation_receiver), None);
        for url in ["http://remote-only.invalid:8080/", "http://localhost:8080/"] {
            BrowserPage::navigate_webview(&state.webview, url).unwrap();
            assert_eq!(
                pump_until(&navigation_receiver),
                None,
                "Navigation must succeed through SOCKS"
            );
            let (sender, receiver) = async_channel::bounded(1);
            let handler = webview2_com::ExecuteScriptCompletedHandler::create(Box::new(
                move |result, value| {
                    let result = result.map(|()| value);
                    let _ = sender.try_send(result);
                    Ok(())
                },
            ));
            let script: Vec<u16> = "document.body.textContent"
                .encode_utf16()
                .chain(Some(0))
                .collect();
            unsafe {
                state
                    .webview
                    .ExecuteScript(PCWSTR(script.as_ptr()), &handler)
                    .unwrap()
            };
            assert_eq!(pump_until(&receiver).unwrap(), "\"ssh-browser-fixture\"");
        }
        // Browser tabs share one proxy/profile but keep independent native views.
        let (second_sender, second_receiver) = async_channel::bounded(1);
        let (second_navigation_sender, second_navigation_receiver) = async_channel::unbounded();
        BrowserPage::start_webview(
            HWND_MESSAGE,
            proxy_port,
            "http://remote-only.invalid:8080/",
            profile.path(),
            second_sender,
            second_navigation_sender,
        )
        .unwrap();
        let second = pump_until(&second_receiver).unwrap();
        assert_eq!(pump_until(&second_navigation_receiver), None);
        let mut source = PWSTR::null();
        unsafe {
            state.webview.Source(&mut source).unwrap();
        }
        assert_eq!(webview2_com::take_pwstr(source), "http://localhost:8080/");
        drop(second);
        BrowserPage::navigate_webview(&state.webview, "http://localhost:8080/after-tab-close")
            .unwrap();
        assert_eq!(
            pump_until(&navigation_receiver),
            None,
            "closing one tab must preserve its sibling"
        );

        let requests = fixture.state.forward_requests.lock().unwrap().clone();
        assert!(
            requests
                .iter()
                .any(|(host, port)| host == "remote-only.invalid" && *port == 8080)
        );
        assert!(
            requests
                .iter()
                .any(|(host, port)| host == "localhost" && *port == 8080)
        );
        drop(state);
        worker.abort();
        drop(runtime);
        unsafe { CoUninitialize() };
    }
}
