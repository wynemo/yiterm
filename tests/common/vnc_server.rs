//! Minimal RFB 3.8 server for exercising a real noVNC client.
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
pub async fn serve(mut tcp: TcpStream) -> anyhow::Result<()> {
    tcp.write_all(b"RFB 003.008\n").await?;
    let mut version = [0; 12];
    tcp.read_exact(&mut version).await?;
    anyhow::ensure!(&version == b"RFB 003.008\n", "RFB version");
    tcp.write_all(&[1, 1]).await?; // one security type: None
    anyhow::ensure!(tcp.read_u8().await? == 1, "RFB security type");
    tcp.write_all(&[0; 4]).await?;
    let _shared = tcp.read_u8().await?;
    let mut init = vec![
        0, 2, 0, 2, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0,
    ];
    init.extend_from_slice(&7u32.to_be_bytes());
    init.extend_from_slice(b"fixture");
    tcp.write_all(&init).await?;
    loop {
        match tcp.read_u8().await? {
            0 => {
                let mut format = [0; 19];
                tcp.read_exact(&mut format).await?;
            }
            2 => {
                let _ = tcp.read_u8().await?;
                let count = tcp.read_u16().await?;
                let mut encodings = vec![0; count as usize * 4];
                tcp.read_exact(&mut encodings).await?;
            }
            3 => {
                let mut request = [0; 9];
                tcp.read_exact(&mut request).await?;
                // FramebufferUpdate: one 2x2 raw rectangle, red in requested RGBX format.
                let mut update = vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 2, 0, 2, 0, 0, 0, 0];
                update.extend_from_slice(&[255, 0, 0, 0].repeat(4));
                tcp.write_all(&update).await?;
            }
            4 => {
                let mut key = [0; 7];
                tcp.read_exact(&mut key).await?;
            }
            5 => {
                let mut pointer = [0; 5];
                tcp.read_exact(&mut pointer).await?;
            }
            other => anyhow::bail!("Unexpected client message {other}"),
        }
    }
}
