use super::*;
#[cfg(target_os = "linux")]
use yiterm::screen_share::linux as platform;
#[cfg(windows)]
use yiterm::screen_share::windows as platform;

pub(super) async fn session(
    stream: ServerStream,
    size: PtySize,
    options: &Options,
    mut stop: Stop<'_, impl std::future::Future<Output = ()>>,
) -> Result<()> {
    let (sizes, size_updates) = watch::channel(size);
    let (input, events) = mpsc::channel(16);
    let (output, mut frames) = mpsc::channel(1);
    let options = options.clone();
    let worker = tokio::task::spawn_blocking(move || {
        platform::stream(&options, size_updates, events, &output)
    });
    let (mut reader, mut writer) = tokio::io::split(stream);
    eprintln!("YiTerm viewer connected.");
    let result = {
        let receive = receive(
            &mut reader,
            async |bytes: &[u8]| {
                let _ = input.send(Zeroizing::new(bytes.to_vec())).await;
                Ok(())
            },
            |size| {
                sizes.send_replace(size);
                Ok(())
            },
        );
        let send = async {
            while let Some(frame) = frames.recv().await {
                timeout(IO_TIMEOUT, async {
                    writer.write_all(&frame).await?;
                    writer.flush().await
                })
                .await??;
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::select! {
            biased;
            _ = stop.as_mut() => Ok(false),
            result = receive => result.map(|()| false),
            result = send => result.map(|()| true),
        }
    };

    drop(frames);
    drop(input);
    let captured = worker.await;
    if result? {
        let captured = captured?;
        let finish = async {
            if let Err(error) = &captured {
                let message: String = format!("{error:#}")
                    .chars()
                    .filter(|c| !c.is_control())
                    .collect();
                writer
                    .write_all(b"\x1b_Ga=d,d=A,q=2;\x1b\\\x1b[?1049l")
                    .await?;
                writer
                    .write_all(format!("\r\n{message}\r\n").as_bytes())
                    .await?;
                writer.flush().await?;
            }
            writer.shutdown().await
        };
        tokio::select! {
            biased;
            _ = stop.as_mut() => return Ok(()),
            _ = timeout(IO_TIMEOUT, finish) => {},
        }
        captured?;
    }
    Ok(())
}
