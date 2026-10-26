//! SOCKS5 TCP forwarding over an existing authenticated SSH transport.
use anyhow::{Context, Result, bail, ensure};
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};

pub async fn serve(
    listener: TcpListener,
    connection: Arc<super::connection::Connection>,
) -> Result<()> {
    const MAX_CLIENTS: usize = 128;
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (client, _) = accepted?;
                if clients.len() >= MAX_CLIENTS {
                    drop(client);
                    continue;
                }
                let connection = connection.clone();
                clients.spawn(async move {
                    if let Err(error) = serve_socks_client(client, connection).await {
                        tracing::debug!(%error, "SOCKS client connection ended");
                    }
                });
            }
            _ = clients.join_next(), if !clients.is_empty() => {}
        }
    }
}

async fn serve_socks_client(
    mut client: TcpStream,
    connection: Arc<super::connection::Connection>,
) -> Result<()> {
    let version = client.read_u8().await?;
    let methods = client.read_u8().await? as usize;
    ensure!(
        version == 5 && (1..=32).contains(&methods),
        "Invalid SOCKS5 greeting"
    );
    let mut offered = vec![0; methods];
    client.read_exact(&mut offered).await?;
    if !offered.contains(&0) {
        client.write_all(&[5, 0xff]).await?;
        bail!("SOCKS5 client requires an unsupported authentication method");
    }
    client.write_all(&[5, 0]).await?;

    let mut request = [0; 4];
    client.read_exact(&mut request).await?;
    ensure!(request[0] == 5 && request[2] == 0, "Invalid SOCKS5 request");
    if request[1] != 1 {
        write_socks_reply(&mut client, 7).await?;
        bail!("Only SOCKS5 TCP CONNECT is supported");
    }
    let host = match request[3] {
        1 => {
            let mut address = [0; 4];
            client.read_exact(&mut address).await?;
            std::net::Ipv4Addr::from(address).to_string()
        }
        3 => {
            let length = client.read_u8().await? as usize;
            ensure!(length > 0, "SOCKS5 domain name is empty");
            let mut name = vec![0; length];
            client.read_exact(&mut name).await?;
            String::from_utf8(name).context("SOCKS5 domain name is not UTF-8")?
        }
        4 => {
            let mut address = [0; 16];
            client.read_exact(&mut address).await?;
            std::net::Ipv6Addr::from(address).to_string()
        }
        _ => {
            write_socks_reply(&mut client, 8).await?;
            bail!("Unsupported SOCKS5 address type");
        }
    };
    let port = client.read_u16().await?;
    tracing::debug!(%host, port, "Opening SSH SOCKS TCP channel");
    let channel = match connection.open_tcp(&host, port).await {
        Ok(channel) => channel,
        Err(error) => {
            write_socks_reply(&mut client, 1).await?;
            tracing::warn!(%host, port, %error, "Could not open SSH SOCKS TCP forwarding");
            return Err(error).context("Could not open SSH TCP forwarding");
        }
    };
    tracing::debug!(%host, port, "SSH SOCKS TCP channel opened");
    write_socks_reply(&mut client, 0).await?;
    let mut remote = channel.into_stream();
    tokio::io::copy_bidirectional(&mut client, &mut remote).await?;
    Ok(())
}

async fn write_socks_reply(client: &mut TcpStream, status: u8) -> Result<()> {
    client
        .write_all(&[5, status, 0, 1, 0, 0, 0, 0, 0, 0])
        .await?;
    Ok(())
}
