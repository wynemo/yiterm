use super::*;
use anyhow::{Context as _, ensure};
use gpui::Subscription;
use gpui_component::input::{Input, InputEvent, InputState};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::path::Path;
use webview2_com::{
    CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, Microsoft::Web::WebView2::Win32::*,
    NavigationStartingEventHandler,
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

pub(crate) struct BrowserUi {
    _tunnel: yiterm::native_ssh::SocksTunnel,
    _ssh_pid: u32,
    webview: Option<WebViewState>,
    visible: bool,
    _user_data: Option<tempfile::TempDir>,
    address: gpui::Entity<InputState>,
    status: Option<String>,
    _address_subscription: Option<Subscription>,
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
        let (sender, receiver) = async_channel::bounded::<anyhow::Result<WebViewState>>(1);
        let status = Some("Starting WebView2…".to_owned());
        let parent = Self::parent_hwnd(window);
        match (parent, user_data.as_ref()) {
            (Ok(parent), Some(user_data)) => {
                if let Err(error) = Self::start_webview(
                    parent,
                    tunnel.port(),
                    &initial_url,
                    user_data.path(),
                    sender.clone(),
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
        let mut this = Self {
            _tunnel: tunnel,
            _ssh_pid: ssh_pid,
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
            let result = receiver.recv().await.unwrap_or_else(|_| {
                Err(anyhow::anyhow!("WebView2 initialization was cancelled"))
            });
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(webview) => {
                        this.webview = Some(webview);
                        this.visible = false;
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
    ) -> anyhow::Result<()> {
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
        let callback_sender = sender.clone();
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
                let initial_url = initial_url.to_owned();
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
                        let state = Self::finish_webview(controller, &initial_url);
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
    ) -> anyhow::Result<WebViewState> {
        unsafe {
            controller.SetBounds(RECT::default())?;
            controller.SetIsVisible(false)?;
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

    pub(super) fn set_visible(&mut self, visible: bool) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if let Some(state) = &self.webview
            && let Err(error) = unsafe { state.controller.SetIsVisible(visible) }
        {
            tracing::debug!(%error, "Could not change WebView2 visibility");
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
        self.set_visible(true);
        let status = self.status.clone();
        let controller = self.webview.as_ref().map(|state| state.controller.clone());
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
