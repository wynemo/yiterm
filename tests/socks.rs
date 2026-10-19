#[path = "common/native_server.rs"]
mod native_server;

use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use yiterm::native_ssh::{self, auth::Prompt, socks};

struct Password;
impl Prompt for Password {
    fn ask(&self, _: &str, _: bool) -> anyhow::Result<String> {
        Ok("fixture-password".into())
    }
}

async fn connect(proxy: std::net::SocketAddr, host: &str, port: u16) -> TcpStream {
    let mut client = TcpStream::connect(proxy).await.unwrap();
    client.write_all(&[5, 1, 0]).await.unwrap();
    let mut greeting = [0; 2];
    client.read_exact(&mut greeting).await.unwrap();
    assert_eq!(greeting, [5, 0]);
    let mut request = vec![5, 1, 0, 3, host.len() as u8];
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    client.write_all(&request).await.unwrap();
    client
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socks_relays_http_and_half_close_over_shared_ssh_with_remote_hostnames() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let fixture = native_server::Fixture::new().await;
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        *fixture.state.forward_target.lock().unwrap() = Some(http.local_addr().unwrap());
        let body = vec![b'x'; 600_000];
        let expected = body.clone();
        let http_worker = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = http.accept().await.unwrap();
                let body = body.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        request.push(socket.read_u8().await.unwrap());
                    }
                    assert!(request.starts_with(b"GET / HTTP/1.1\r\n"));
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                    socket.shutdown().await.unwrap();
                });
            }
        });
        let handle = Arc::new(native_ssh::connect(fixture.config.clone(), Arc::new(Password)).await.unwrap());
        let mut shell = native_ssh::shell(&handle, 80, 24).await.unwrap();
        shell.wait().await;
        shell.wait().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        let worker = tokio::spawn(socks::serve(listener, handle));
        for (host, half_close) in [("localhost", false), ("remote-only.invalid", true)] {
            let mut client = connect(proxy, host, 8080).await;
            let mut reply = [0; 10];
            client.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply, [5, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
            client.write_all(b"GET / HTTP/1.1\r\nHost: remote-only.invalid\r\n\r\n").await.unwrap();
            if half_close { client.shutdown().await.unwrap(); }
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
            assert!(response.ends_with(&expected));
        }
        shell.data(&b"still-alive"[..]).await.unwrap();
        match shell.wait().await.unwrap() {
            russh::ChannelMsg::Data { data } => assert_eq!(&data[..], b"still-alive"),
            other => panic!("{other:?}"),
        }
        assert_eq!(fixture.state.connections.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.state.authentications.load(Ordering::SeqCst), 1);
        assert_eq!(*fixture.state.forward_requests.lock().unwrap(), [("localhost".into(), 8080), ("remote-only.invalid".into(), 8080)]);
        worker.abort();
        http_worker.await.unwrap();
    }).await.unwrap();
}

#[tokio::test]
async fn socks_reports_forwarding_denied_without_closing_ssh() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = native_server::Fixture::new().await;
        let handle = Arc::new(
            native_ssh::connect(fixture.config.clone(), Arc::new(Password))
                .await
                .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        let worker = tokio::spawn(socks::serve(listener, handle.clone()));
        let mut client = connect(proxy, "refused.invalid", 443).await;
        let mut reply = [0; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0..3], [5, 1, 0]);
        assert!(native_ssh::shell(&handle, 80, 24).await.is_ok());
        worker.abort();
    })
    .await
    .unwrap();
}
