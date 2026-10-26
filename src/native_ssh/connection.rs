//! Reuse SSH while alive; reconnect only when an auxiliary operation needs it.
use super::{
    auth::{Handler, Prompt},
    config::Config,
};
use anyhow::{Context, Result, bail};
use russh::{Channel, client};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

type Handle = Arc<client::Handle<Handler>>;

pub struct Connection {
    current: Mutex<(Handle, Option<Instant>)>,
    reconnect: Option<(Config, Arc<dyn Prompt>, PathBuf)>,
}

impl Connection {
    pub fn new(handle: Handle, reconnect: Option<(Config, Arc<dyn Prompt>, PathBuf)>) -> Self {
        Self {
            current: Mutex::new((handle, None)),
            reconnect,
        }
    }

    async fn get(&self, failed: Option<&Handle>) -> Result<Handle> {
        // Serialize reconnects from simultaneous browser requests and uploads.
        let mut state = self.current.lock().await;
        let stale = state.0.is_closed() || failed.is_some_and(|old| Arc::ptr_eq(old, &state.0));
        if !stale {
            return Ok(state.0.clone());
        }
        let (config, prompt, ssh) = self
            .reconnect
            .as_ref()
            .context("SSH connection is closed")?;
        if state
            .1
            .is_some_and(|last| last.elapsed() < Duration::from_secs(2))
        {
            bail!("SSH reconnect failed recently; retry the request");
        }
        state.1 = Some(Instant::now());
        tracing::info!("Reconnecting auxiliary SSH on demand");
        let result = tokio::time::timeout(Duration::from_secs(12), async {
            if config.proxy_jump.is_some() {
                super::connect_proxyjump(config.clone(), prompt.clone(), ssh, true).await
            } else {
                super::connect(config.clone(), prompt.clone()).await
            }
        })
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("Auxiliary SSH reconnect timed out")));
        match result {
            Ok(handle) => {
                state.0 = Arc::new(handle);
                state.1 = None;
                tracing::info!("Auxiliary SSH reconnected");
                Ok(state.0.clone())
            }
            Err(error) => {
                // No timer or retry loop: another operation may try again later.
                state.1 = Some(Instant::now());
                tracing::warn!(error = %format!("{error:#}"), "Auxiliary SSH reconnect failed");
                Err(error)
            }
        }
    }

    pub async fn open_tcp(&self, host: &str, port: u16) -> Result<Channel<client::Msg>> {
        let handle = self.get(None).await?;
        match handle
            .channel_open_direct_tcpip(host, u32::from(port), "127.0.0.1", 0)
            .await
        {
            Ok(channel) => Ok(channel),
            Err(russh::Error::SendError | russh::Error::Disconnect) if self.reconnect.is_some() => {
                self.get(Some(&handle))
                    .await?
                    .channel_open_direct_tcpip(host, u32::from(port), "127.0.0.1", 0)
                    .await
                    .map_err(Into::into)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub async fn open_session(&self) -> Result<Channel<client::Msg>> {
        let handle = self.get(None).await?;
        match handle.channel_open_session().await {
            Ok(channel) => Ok(channel),
            Err(russh::Error::SendError | russh::Error::Disconnect) if self.reconnect.is_some() => {
                self.get(Some(&handle))
                    .await?
                    .channel_open_session()
                    .await
                    .map_err(Into::into)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub async fn close(&self) {
        let state = self.current.lock().await;
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            state
                .0
                .disconnect(russh::Disconnect::ByApplication, "Session closed", "en"),
        )
        .await;
    }
}
