#[path = "common/native_server.rs"]
mod native_server;
use futures_util::{SinkExt, StreamExt};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use yiterm::{
    native_ssh::{self, auth::Prompt, socks},
    vnc::VncBridge,
};
struct Password;
impl Prompt for Password {
    fn ask(&self, _: &str, _: bool) -> anyhow::Result<String> {
        Ok("fixture-password".into())
    }
}
async fn http(url: &url::Url) -> String {
    let mut tcp = TcpStream::connect(("127.0.0.1", url.port().unwrap()))
        .await
        .unwrap();
    tcp.write_all(
        format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            url.path(),
            url.authority()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut result = String::new();
    tcp.read_to_string(&mut result).await.unwrap();
    result
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vnc_websocket_uses_authenticated_ssh_and_closes_with_panel() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let fixture = native_server::Fixture::new().await;
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        *fixture.state.forward_target.lock().unwrap() = Some(target.local_addr().unwrap());
        let echo = tokio::spawn(async move {
            let (mut tcp, _) = target.accept().await.unwrap();
            tcp.write_all(b"RFB 003.008\n").await.unwrap();
            let mut buf = [0; 12];
            tcp.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"RFB 003.008\n");
            tcp.write_all(b"received").await.unwrap();
            let mut byte = [0];
            assert_eq!(tcp.read(&mut byte).await.unwrap(), 0);
        });
        let handle = Arc::new(
            native_ssh::connect(fixture.config.clone(), Arc::new(Password))
                .await
                .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bridge = VncBridge::start(listener.local_addr().unwrap().port()).unwrap();
        let proxy = tokio::spawn(socks::serve(listener, handle));
        let page = url::Url::parse(&bridge.url).unwrap();
        assert!(http(&page).await.contains("Connect / Reconnect"));
        assert!(
            http(&page.join("core/rfb.js").unwrap())
                .await
                .contains("class RFB")
        );
        assert!(
            http(&page.join("/wrong/index.html").unwrap())
                .await
                .starts_with("HTTP/1.1 404")
        );
        let mut ws = page.join("ws?host=remote-only.invalid&port=5901").unwrap();
        ws.set_scheme("ws").unwrap();
        let mut request = ws.as_str().into_client_request().unwrap();
        request
            .headers_mut()
            .insert("Origin", "http://evil.invalid".parse().unwrap());
        assert!(connect_async(request).await.is_err());
        let mut request = ws.as_str().into_client_request().unwrap();
        request.headers_mut().insert(
            "Origin",
            page.origin().ascii_serialization().parse().unwrap(),
        );
        let (mut socket, _) = connect_async(request).await.unwrap();
        assert_eq!(
            socket.next().await.unwrap().unwrap().into_data(),
            b"RFB 003.008\n".as_slice()
        );
        socket
            .send(Message::Binary(b"RFB 003.008\n".to_vec().into()))
            .await
            .unwrap();
        assert_eq!(
            socket.next().await.unwrap().unwrap().into_data(),
            b"received".as_slice()
        );
        drop(bridge);
        assert!(
            TcpStream::connect(("127.0.0.1", page.port().unwrap()))
                .await
                .is_err()
        );
        echo.await.unwrap();
        proxy.abort();
    })
    .await
    .expect("VNC integration timed out");
}
