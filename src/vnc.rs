//! Loopback-only noVNC bridge. Remote TCP connections always use SSH SOCKS.
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    extract::{DefaultBodyLimit, Path, Query, State, WebSocketUpgrade, ws::Message},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use include_dir::{Dir, include_dir};
use std::{sync::Arc, thread};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::oneshot,
};

static ASSETS: Dir = include_dir!("$CARGO_MANIFEST_DIR/assets/novnc");
struct BridgeState {
    token: String,
    origin: String,
    socks_port: u16,
}

pub struct VncBridge {
    pub url: String,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl VncBridge {
    pub fn start(socks_port: u16) -> Result<Self> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let origin = format!("http://{}", listener.local_addr()?);
        let token = uuid::Uuid::new_v4().to_string();
        let url = format!("{origin}/{token}/index.html");
        let state = Arc::new(BridgeState {
            token,
            origin,
            socks_port,
        });
        let (stop, stopped) = oneshot::channel();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let worker = thread::Builder::new()
            .name("yiterm-vnc".into())
            .spawn(move || {
                runtime.block_on(async move {
                    let app = Router::new()
                        .route("/{token}/ws", get(websocket))
                        .route(
                            "/{token}/diagnostic",
                            post(diagnostic).layer(DefaultBodyLimit::max(1024)),
                        )
                        .route("/{token}/{*path}", get(asset))
                        .with_state(state);
                    let listener = match tokio::net::TcpListener::from_std(listener) {
                        Ok(l) => l,
                        Err(_) => return,
                    };
                    tokio::select! { _ = axum::serve(listener, app) => {}, _ = stopped => {} }
                });
                // Dropping the runtime cancels all outstanding WebSocket/TCP tasks.
            })?;
        Ok(Self {
            url,
            stop: Some(stop),
            worker: Some(worker),
        })
    }
}
impl Drop for VncBridge {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
#[derive(serde::Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
enum DiagnosticEvent {
    Connection,
    Layout,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Diagnostic {
    event: DiagnosticEvent,
    apple: bool,
    authentication: u32,
    display_messages: u32,
    invalid_layouts: u32,
    width: u16,
    height: u16,
    screens: u8,
    selected_screen: Option<u32>,
}

async fn diagnostic(
    State(state): State<Arc<BridgeState>>,
    Path(token): Path<String>,
    axum::Json(info): axum::Json<Diagnostic>,
) -> StatusCode {
    if token != state.token {
        return StatusCode::NOT_FOUND;
    }
    // Only protocol metadata, never credentials, input, or framebuffer data.
    tracing::info!(event = ?info.event, apple = info.apple,
        display_messages = info.display_messages, invalid_layouts = info.invalid_layouts,
        authentication = info.authentication, width = info.width, height = info.height,
        screens = info.screens, selected_screen = ?info.selected_screen,
        "VNC protocol diagnostic");
    StatusCode::NO_CONTENT
}

async fn asset(
    State(state): State<Arc<BridgeState>>,
    Path((token, path)): Path<(String, String)>,
) -> Response {
    if token != state.token {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (body, mime) = if path == "index.html" {
        (
            include_bytes!("../assets/novnc/index.html").as_slice(),
            "text/html; charset=utf-8",
        )
    } else if let Some(file) = ASSETS.get_file(&path) {
        (
            file.contents(),
            if path.ends_with(".js") {
                "text/javascript"
            } else {
                "application/octet-stream"
            },
        )
    } else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let policy = format!(
        "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self' {}; img-src 'self' data: blob:; frame-ancestors 'none'",
        state.origin.replacen("http:", "ws:", 1)
    );
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CONTENT_SECURITY_POLICY, policy.as_str()),
        ],
        body,
    )
        .into_response()
}
#[derive(serde::Deserialize)]
struct Target {
    host: String,
    port: u16,
}
async fn websocket(
    State(state): State<Arc<BridgeState>>,
    Path(token): Path<String>,
    Query(target): Query<Target>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if token != state.token
        || headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) != Some(state.origin.as_str())
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    if target.host.is_empty() || target.host.len() > 255 || target.port == 0 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    ws.max_message_size(8 * 1024 * 1024).max_frame_size(8 * 1024 * 1024).protocols(["binary"]).on_upgrade(move |mut socket| async move {
        let result = async {
            let mut tcp = tokio::time::timeout(std::time::Duration::from_secs(15), connect(state.socks_port, &target.host, target.port)).await??;
            let mut buffer = vec![0; 64 * 1024];
            loop { tokio::select! {
                read = tcp.read(&mut buffer) => { let n = read?; if n == 0 { break; } socket.send(Message::Binary(buffer[..n].to_vec().into())).await?; }
                message = socket.next() => { match message { Some(Ok(Message::Binary(bytes))) => tcp.write_all(&bytes).await?, Some(Ok(Message::Close(_))) | None => break, Some(Err(error)) => return Err(error.into()), _ => {} } }
            } }
            Ok::<_, anyhow::Error>(())
        }.await;
        if let Err(error) = result { tracing::debug!(%error, "VNC bridge disconnected"); }
        let _ = socket.close().await;
    }).into_response()
}
async fn connect(proxy: u16, host: &str, port: u16) -> Result<TcpStream> {
    let mut tcp = TcpStream::connect(("127.0.0.1", proxy))
        .await
        .context("connecting SSH SOCKS")?;
    tcp.write_all(&[5, 1, 0]).await?;
    let mut greeting = [0; 2];
    tcp.read_exact(&mut greeting).await?;
    ensure!(greeting == [5, 0], "SSH SOCKS authentication rejected");
    let mut request = vec![5, 1, 0, 3, u8::try_from(host.len())?];
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    tcp.write_all(&request).await?;
    let mut reply = [0; 4];
    tcp.read_exact(&mut reply).await?;
    ensure!(reply[0] == 5 && reply[1] == 0, "SSH refused VNC forwarding");
    let length = match reply[3] {
        1 => 4,
        4 => 16,
        3 => tcp.read_u8().await? as usize,
        _ => anyhow::bail!("Invalid SOCKS reply"),
    };
    let mut address = vec![0; length + 2];
    tcp.read_exact(&mut address).await?;
    Ok(tcp)
}
