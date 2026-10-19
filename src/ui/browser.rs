use super::*;
use anyhow::{Context as _, ensure};
use gpui::Subscription;
use gpui_component::input::{Input, InputEvent, InputState};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{path::Path, sync::mpsc};
use webview2_com::{
    CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, Microsoft::Web::WebView2::Win32::*,
    NavigationStartingEventHandler,
};
use windows::{
    Win32::Foundation::{BOOL, E_POINTER, HWND, RECT},
    core::{PCWSTR, PWSTR},
};

const TOOLBAR_HEIGHT: f32 = 50.;

struct WebViewState {
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
    navigation_token: i64,
}

impl Drop for WebViewState {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .webview
                .remove_NavigationStarting(self.navigation_token);
            let _ = self.controller.Close();
        }
    }
}

pub(super) struct BrowserUi {
    _tunnel: yiterm::native_ssh::SocksTunnel,
    _ssh_pid: u32,
    webview: Option<WebViewState>,
    _user_data: Option<tempfile::TempDir>,
    address: gpui::Entity<InputState>,
    status: Option<String>,
    _address_subscription: Option<Subscription>,
    _bounds_subscription: Subscription,
}

impl BrowserUi {
    pub(super) fn new(
        tunnel: yiterm::native_ssh::SocksTunnel,
        ssh_pid: u32,
        initial_url: String,
        window: &mut Window,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        let address = cx.new(|cx| InputState::new(window, cx).default_value(initial_url.clone()));
        let user_data = tempfile::Builder::new()
            .prefix("yiterm-webview-")
            .tempdir()
            .ok();
        let mut status = None;
        let webview = match user_data.as_ref() {
            Some(user_data) => {
                match Self::create_webview(window, tunnel.port(), &initial_url, user_data.path()) {
                    Ok(webview) => Some(webview),
                    Err(error) => {
                        status = Some(format!(
                            "Cannot initialize WebView2. Install the Microsoft Edge WebView2 Runtime. {error:#}"
                        ));
                        None
                    }
                }
            }
            None => {
                status = Some("Cannot create a temporary WebView2 profile directory.".into());
                None
            }
        };
        let bounds_subscription = cx.observe_window_bounds(window, |this, window, _cx| {
            this.resize_webview(window);
        });
        let mut this = Self {
            _tunnel: tunnel,
            _ssh_pid: ssh_pid,
            webview,
            _user_data: user_data,
            address,
            status,
            _address_subscription: None,
            _bounds_subscription: bounds_subscription,
        };
        this.resize_webview(window);
        let subscription =
            cx.subscribe_in(&this.address, window, |this, input, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let value = input.read(cx).value().to_string();
                    this.navigate(&value, window, cx);
                }
            });
        this._address_subscription = Some(subscription);
        this
    }

    fn create_webview(
        window: &Window,
        proxy_port: u16,
        initial_url: &str,
        user_data_path: &Path,
    ) -> anyhow::Result<WebViewState> {
        let parent = match HasWindowHandle::window_handle(window)
            .context("getting the browser window handle")?
            .as_raw()
        {
            RawWindowHandle::Win32(handle) => HWND(handle.hwnd.get() as *mut _),
            handle => anyhow::bail!("Expected a Win32 browser window, got {handle:?}"),
        };
        let folder: Vec<u16> = user_data_path
            .to_string_lossy()
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let options = CoreWebView2EnvironmentOptions::default();
        let arguments = format!(
            "--proxy-server=socks5://127.0.0.1:{proxy_port} --proxy-bypass-list=<-loopback> --host-resolver-rules=\"MAP * ~NOTFOUND, EXCLUDE localhost, EXCLUDE 127.0.0.1, EXCLUDE ::1\" --disable-quic --force-webrtc-ip-handling-policy=disable_non_proxied_udp"
        );
        unsafe { options.set_additional_browser_arguments(arguments) };
        let options: ICoreWebView2EnvironmentOptions = options.into();

        let environment = {
            let (sender, receiver) = mpsc::channel();
            CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
                Box::new(move |handler| unsafe {
                    CreateCoreWebView2EnvironmentWithOptions(
                        PCWSTR::null(),
                        PCWSTR(folder.as_ptr()),
                        &options,
                        &handler,
                    )
                    .map_err(webview2_com::Error::WindowsError)
                }),
                Box::new(move |result, environment| {
                    result?;
                    sender
                        .send(environment.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                        .expect("WebView2 environment receiver remains alive");
                    Ok(())
                }),
            )?;
            receiver
                .recv()
                .context("waiting for WebView2 environment")??
        };

        let controller = {
            let (sender, receiver) = mpsc::channel();
            CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
                Box::new(move |handler| unsafe {
                    environment
                        .CreateCoreWebView2Controller(parent, &handler)
                        .map_err(webview2_com::Error::WindowsError)
                }),
                Box::new(move |result, controller| {
                    result?;
                    sender
                        .send(controller.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                        .expect("WebView2 controller receiver remains alive");
                    Ok(())
                }),
            )?;
            receiver
                .recv()
                .context("waiting for WebView2 controller")??
        };
        unsafe {
            controller.SetIsVisible(true)?;
        }
        let webview = unsafe { controller.CoreWebView2()? };
        let navigation = NavigationStartingEventHandler::create(Box::new(|_, args| {
            if let Some(args) = args {
                let mut uri = PWSTR::null();
                unsafe { args.Uri(&mut uri)? };
                let uri = webview2_com::take_pwstr(uri);
                let allowed = uri.eq_ignore_ascii_case("about:blank")
                    || uri.starts_with("https://")
                    || uri.starts_with("http://");
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
        if !initial_url.is_empty() {
            Self::navigate_webview(&webview, initial_url)?;
        }
        Ok(WebViewState {
            controller,
            webview,
            navigation_token,
        })
    }

    fn navigate_webview(webview: &ICoreWebView2, value: &str) -> anyhow::Result<()> {
        let value = if value.eq_ignore_ascii_case("about:blank") {
            value.to_owned()
        } else {
            normalize_url(value)?
        };
        let wide: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
        unsafe { webview.Navigate(&PCWSTR(wide.as_ptr()))? };
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
                if let Ok(url) = normalize_url(value) {
                    self.address
                        .update(cx, |input, cx| input.set_value(url, window, cx));
                }
                self.status = None;
            }
            Err(error) => self.status = Some(format!("Cannot open address: {error:#}")),
        }
        cx.notify();
    }

    fn resize_webview(&mut self, window: &Window) {
        let Some(state) = &self.webview else {
            return;
        };
        let bounds = window.bounds();
        let scale = window.scale_factor();
        let width = (f32::from(bounds.size.width) * scale).round() as i32;
        let height = (f32::from(bounds.size.height) * scale).round() as i32;
        let top = (TOOLBAR_HEIGHT * scale).round() as i32;
        let bounds = RECT {
            left: 0,
            top,
            right: width.max(0),
            bottom: height.max(top),
        };
        if let Err(error) = unsafe { state.controller.SetBounds(&bounds) } {
            tracing::warn!(%error, "Could not resize WebView2 content");
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

impl Render for BrowserUi {
    fn render(&mut self, _window: &mut Window, cx: &mut ViewContext<Self>) -> impl IntoElement {
        let status = self.status.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0xF3F4F6))
            .text_color(rgb(0x202124))
            .child(
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
                    ),
            )
            .child(div().flex_1().min_h_0().when_some(status, |this, status| {
                this.flex()
                    .items_center()
                    .justify_center()
                    .p(px(24.))
                    .text_color(rgb(0x9B1C1C))
                    .child(status)
            }))
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
