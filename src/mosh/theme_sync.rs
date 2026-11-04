//! Match mymosh's Herdr-only theme sync: mosh-server consumes OSC queries,
//! so replying in the local terminal parser cannot reach the multiplexer.
use crate::{native_ssh::auth::Handler, theme::ThemeMode};
use anyhow::{Result, ensure};
use russh::{ChannelMsg, client};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle};

pub(super) fn monitor(
    handle: Arc<client::Handle<Handler>>,
    port: u16,
) -> (mpsc::Receiver<Vec<u8>>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel(1);
    let task = tokio::spawn(async move {
        let path = std::env::var_os("YITERM_CONFIG_PATH")
            .map(std::path::PathBuf::from)
            .map(Ok)
            .unwrap_or_else(crate::logging::config_path);
        let Ok(path) = path else { return };
        let mut sent = None;
        let mut foreground = false;
        let mut tick = tokio::time::interval(Duration::from_secs(3));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = sender.closed() => break,
                _ = tick.tick() => {}
            }
            // Monitoring never reconnects SSH or prompts on the Mosh console.
            if handle.is_closed() {
                break;
            }
            let result = tokio::time::timeout(Duration::from_secs(5), probe(&handle, port)).await;
            let Ok(Ok(active)) = result else {
                tracing::debug!("Mosh Herdr theme probe unavailable");
                continue;
            };
            if active != foreground {
                tracing::debug!(foreground = active, "Mosh foreground Herdr changed");
                foreground = active;
            }
            if !active {
                // A later Herdr process needs a fresh initial response too.
                sent = None;
                continue;
            }
            let Ok(theme) = crate::settings::load_theme(&path) else {
                continue;
            };
            if sent != Some(theme) {
                if sender.send(response(theme)).await.is_err() {
                    break;
                }
                sent = Some(theme);
            }
        }
    });
    (receiver, task)
}

async fn probe(handle: &client::Handle<Handler>, port: u16) -> Result<bool> {
    // Associate Herdr with this UDP server, not another login by the same user.
    let command = format!(
        "lsof -nP -a -c mosh-server -iUDP:{port} -Fpn 2>/dev/null; printf '\\n__YITERM_PROCESS_LIST__\\n'; ps -Ao pid= -o ppid= -o pgid= -o tpgid= -o comm= 2>/dev/null"
    );
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, command).await?;
    let mut output = Vec::new();
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => {
                ensure!(
                    output.len() + data.len() <= 512 * 1024,
                    "Process list too large"
                );
                output.extend_from_slice(&data);
            }
            ChannelMsg::Failure => anyhow::bail!("Herdr probe rejected"),
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok(has_foreground_herdr(&String::from_utf8_lossy(&output)))
}

fn has_foreground_herdr(output: &str) -> bool {
    let Some((servers, processes)) = output.split_once("\n__YITERM_PROCESS_LIST__\n") else {
        return false;
    };
    let servers: BTreeSet<u32> = servers
        .lines()
        .filter_map(|line| line.strip_prefix('p')?.parse().ok())
        .collect();
    if servers.is_empty() {
        return false;
    }
    let processes: BTreeMap<u32, (u32, bool)> = processes
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let parent = fields.next()?.parse().ok()?;
            let group: i64 = fields.next()?.parse().ok()?;
            let foreground: i64 = fields.next()?.parse().ok()?;
            let name = fields.next()?.rsplit('/').next()?;
            Some((pid, (parent, group == foreground && name == "herdr")))
        })
        .collect();
    processes.values().any(|&(mut parent, herdr)| {
        if !herdr {
            return false;
        }
        let mut visited = BTreeSet::new();
        while visited.insert(parent) {
            if servers.contains(&parent) {
                return true;
            }
            let Some(&(next, _)) = processes.get(&parent) else {
                break;
            };
            parent = next;
        }
        false
    })
}

fn response(theme: ThemeMode) -> Vec<u8> {
    let osc = |value: crate::backend::Color| {
        #[cfg(target_os = "linux")]
        let [r, g, b] = [value.r(), value.g(), value.b()];
        #[cfg(not(target_os = "linux"))]
        let [r, g, b] = [value.r, value.g, value.b].map(|channel| (channel * 255.).round() as u8);
        format!(
            "rgb:{:04x}/{:04x}/{:04x}",
            u16::from(r) * 257,
            u16::from(g) * 257,
            u16::from(b) * 257
        )
    };
    let mut bytes = format!(
        "\x1b]10;{}\x1b\\\x1b]11;{}\x1b\\",
        osc(theme.foreground()),
        osc(theme.background()),
    );
    for index in 0..16 {
        bytes.push_str(&format!(
            "\x1b]4;{index};{}\x1b\\",
            osc(theme.palette(index))
        ));
    }
    bytes.push_str(if theme == ThemeMode::Light {
        "\x1b[?997;2n"
    } else {
        "\x1b[?997;1n"
    });
    bytes.into_bytes()
}
