//! Native Mosh protocol-v2 client. The protocol layout and state rebasing follow
//! packages/MoshCore/Sources/MoshCoreSwift in the adjacent mymosh project.
mod proto;
mod wire;

use anyhow::{Context, Result, ensure};
use proto::{Instruction, Operation};
use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{net::UdpSocket, sync::mpsc, task::JoinHandle};

/// Kept in memory only; never serialize or log the MOSH CONNECT key.
pub struct Endpoint {
    address: SocketAddr,
    key: zeroize::Zeroizing<String>,
}

/// Start mosh-server through the already authenticated SSH connection. No PTY
/// or interactive shell is needed, and bootstrap output never reaches logs.
pub async fn bootstrap(
    handle: &russh::client::Handle<crate::native_ssh::auth::Handler>,
    host: &str,
) -> Result<Endpoint> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut channel = handle.channel_open_session().await?;
        channel
            .exec(true, "printf 'YITERM_MOSH_PEER %s\\n' \"$SSH_CONNECTION\"; mosh-server new -s -c 256 -l LANG=C.UTF-8")
            .await?;
        let mut output = zeroize::Zeroizing::new(Vec::new());
        let mut status = None;
        while let Some(message) = channel.wait().await {
            match message {
                russh::ChannelMsg::Data { data } => {
                    ensure!(
                        output.len() + data.len() <= 65536,
                        "Mosh bootstrap output is too large"
                    );
                    output.extend_from_slice(&data);
                }
                russh::ChannelMsg::Failure => anyhow::bail!("Server rejected Mosh startup"),
                russh::ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                russh::ChannelMsg::Close => break,
                _ => {}
            }
        }
        ensure!(
            status == Some(0),
            "Could not start mosh-server; install Mosh on the remote host and enable a UTF-8 locale"
        );
        let text = std::str::from_utf8(&output).context("Invalid Mosh bootstrap response")?;
        let server_ip = text.lines().find_map(|line| {
            line.strip_prefix("YITERM_MOSH_PEER ")?.split_whitespace()
                .nth(2)?.parse::<IpAddr>().ok()
        });
        let mut endpoint = None;
        for line in text.lines() {
            if let Some(connect) = line.trim().strip_prefix("MOSH CONNECT ") {
                ensure!(endpoint.is_none(), "Ambiguous Mosh bootstrap response");
                let mut fields = connect.split_whitespace();
                let port: u16 = fields.next().context("Missing Mosh UDP port")?.parse()?;
                ensure!(port != 0, "Invalid Mosh UDP port");
                let key =
                    zeroize::Zeroizing::new(fields.next().context("Missing Mosh key")?.to_owned());
                ensure!(fields.next().is_none(), "Invalid Mosh bootstrap response");
                wire::Wire::new(&key)?;
                let addresses: Vec<_> = tokio::net::lookup_host((host, port)).await
                    .context("Mosh needs a locally resolvable host and direct UDP access; ProxyJump only carries SSH")?.collect();
                // Match the SSH server interface where possible. With NAT,
                // preserve the configured public host and use the same family.
                let address = server_ip.and_then(|ip| addresses.iter().find(|a| a.ip() == ip).copied())
                    .or_else(|| server_ip.and_then(|ip| addresses.iter().find(|a| a.is_ipv4() == ip.is_ipv4()).copied()))
                    .or_else(|| addresses.first().copied())
                    .context("Mosh host has no IP address")?;
                endpoint = Some(Endpoint { address, key });
            }
        }
        endpoint.context("Server did not return MOSH CONNECT; verify mosh-server is installed")
    })
    .await
    .context("Mosh bootstrap timed out")?
}

struct Local {
    event: Vec<u8>,
}
struct Session {
    wire: wire::Wire,
    started: Instant,
    local: BTreeMap<u64, Local>,
    next_local: u64,
    ack_local: u64,
    remote: BTreeMap<u64, vt100::Screen>,
    displayed: u64,
    last_send: Instant,
    send_due: Instant,
    last_contact: Instant,
    last_roundtrip: Instant,
    rto: Duration,
    closing: Option<Instant>,
    close_acked: bool,
    remote_closed: bool,
}
impl Session {
    fn new(key: &str, cols: u16, rows: u16) -> Result<Self> {
        let now = Instant::now();
        let mut session = Self {
            wire: wire::Wire::new(key)?,
            started: now,
            local: BTreeMap::from([(0, Local { event: Vec::new() })]),
            next_local: 1,
            ack_local: 0,
            remote: BTreeMap::from([(0, vt100::Parser::default().screen().clone())]),
            displayed: 0,
            last_send: now,
            send_due: now,
            last_contact: now,
            last_roundtrip: now,
            rto: Duration::from_millis(250),
            closing: None,
            close_acked: false,
            remote_closed: false,
        };
        session.event(proto::resize(cols, rows))?;
        Ok(session)
    }
    fn clock(&self) -> u16 {
        let value = self.started.elapsed().as_millis() as u16;
        if value == u16::MAX { 0 } else { value }
    }
    fn event(&mut self, event: Vec<u8>) -> Result<()> {
        ensure!(
            self.local.len() < 65536
                && self.local.values().map(|s| s.event.len()).sum::<usize>() + event.len()
                    <= 1024 * 1024,
            "Mosh unacknowledged input exceeded 1 MiB; reconnect before sending more input"
        );
        ensure!(self.next_local < u64::MAX, "Mosh state number exhausted");
        self.local.insert(self.next_local, Local { event });
        self.next_local += 1;
        self.send_due = self
            .send_due
            .min(Instant::now() + Duration::from_millis(20));
        Ok(())
    }
    fn close(&mut self) {
        self.closing.get_or_insert_with(Instant::now);
        self.send_due = Instant::now();
    }
    fn packets(&mut self) -> Result<Vec<Vec<u8>>> {
        if Instant::now() < self.send_due {
            return Ok(Vec::new());
        }
        let latest = *self.local.last_key_value().unwrap().0;
        // Empty numbered states make keepalives acknowledgeable. Lost input is
        // always rebased on the last *acknowledged* state, never a guessed one.
        if latest == self.ack_local && self.closing.is_none() {
            self.event(Vec::new())?;
        }
        let latest = *self.local.last_key_value().unwrap().0;
        let diff = if self.closing.is_some() {
            Vec::new()
        } else {
            self.local
                .range((
                    std::ops::Bound::Excluded(self.ack_local),
                    std::ops::Bound::Unbounded,
                ))
                .flat_map(|(_, s)| s.event.iter().copied())
                .collect()
        };
        let instruction = Instruction {
            version: 2,
            old: self.ack_local,
            new: if self.closing.is_some() {
                u64::MAX
            } else {
                latest
            },
            ack: self.displayed,
            throwaway: self.ack_local,
            diff,
        };
        let packets = self.wire.encode(&instruction.encode(), self.clock())?;
        self.last_send = Instant::now();
        self.send_due = self.last_send
            + if latest > self.ack_local || self.closing.is_some() {
                self.rto + Duration::from_millis(100)
            } else {
                Duration::from_secs(3)
            };
        Ok(packets)
    }
    fn receive(&mut self, data: &[u8]) -> Result<Option<Vec<u8>>> {
        let received = self.wire.receive(data, self.clock())?;
        if let Some(rtt) = received.rtt.filter(|rtt| *rtt < Duration::from_secs(5)) {
            self.rto = (rtt * 2).clamp(Duration::from_millis(50), Duration::from_secs(1));
        }
        let Some(message) = received.message else {
            return Ok(None);
        };
        let instruction = Instruction::decode(&message)?;
        self.last_contact = Instant::now();
        if instruction.ack == u64::MAX && self.closing.is_some() {
            self.close_acked = true;
        }
        if instruction.ack > self.ack_local && instruction.ack < self.next_local {
            self.ack_local = instruction.ack;
            self.local.retain(|n, _| *n >= self.ack_local);
            // Already incorporated into server state; retain only the number.
            if let Some(base) = self.local.get_mut(&self.ack_local) {
                base.event.clear();
            }
            self.last_roundtrip = Instant::now();
            if self.ack_local + 1 == self.next_local {
                self.send_due = self.last_send + Duration::from_secs(3);
            }
        }
        // Never apply HostBytes on a different branch's framebuffer. If the
        // parent is absent, leave our ACK unchanged so the sender will rebase.
        if self.remote.contains_key(&instruction.new) {
            self.send_due = self
                .send_due
                .min(Instant::now() + Duration::from_millis(100));
            return Ok(None);
        }
        let Some(parent) = self.remote.get(&instruction.old) else {
            return Ok(None);
        };
        // mosh-server generates complete VT paint sequences per state diff.
        // Preserve the full screen/cursor/mode snapshot, with a fresh decoder
        // at this paint boundary. Remote terminal responses are never input.
        let mut parser = vt100::Parser::default();
        *parser.screen_mut() = parent.clone();
        let mut paint = Vec::new();
        let mut resized = false;
        for operation in proto::host(&instruction.diff)? {
            match operation {
                Operation::Host(bytes) => {
                    parser.process(bytes);
                    paint.extend_from_slice(bytes);
                }
                Operation::Resize(cols, rows) => {
                    parser.set_size(rows.try_into()?, cols.try_into()?);
                    resized = true;
                }
            }
        }
        let screen = parser.screen().clone();
        let rendered = if instruction.new > self.displayed {
            let render = if instruction.old == self.displayed && !resized {
                paint
            } else {
                screen.state_formatted().into_bytes()
            };
            self.displayed = instruction.new;
            if instruction.new == u64::MAX {
                self.remote_closed = true;
            }
            if self.remote_closed {
                self.send_due = Instant::now();
            } else if !instruction.diff.is_empty() {
                self.send_due = self
                    .send_due
                    .min(Instant::now() + Duration::from_millis(100));
            }
            Some(render)
        } else {
            None
        };
        self.remote.insert(instruction.new, screen);
        self.remote
            .retain(|n, _| *n >= instruction.throwaway || *n == self.displayed);
        // Keep bounded snapshots during a stalled or misbehaving peer. An
        // evicted parent causes a rebase to our latest acknowledged state.
        while self.remote.len() > 128
            || self
                .remote
                .values()
                .map(|screen| {
                    let (rows, cols) = screen.size();
                    usize::from(rows.get()) * usize::from(cols.get())
                })
                .sum::<usize>()
                > 1_000_000
        {
            let Some(n) = self.remote.keys().copied().find(|n| *n != self.displayed) else {
                break;
            };
            self.remote.remove(&n);
        }
        Ok(rendered)
    }
    fn frame(&self) -> Vec<u8> {
        self.remote[&self.displayed].state_formatted().into_bytes()
    }
}

struct Socket {
    socket: Arc<UdpSocket>,
    reader: JoinHandle<()>,
    created: Instant,
}
impl Drop for Socket {
    fn drop(&mut self) {
        self.reader.abort();
    }
}
async fn socket(address: SocketAddr, sender: mpsc::Sender<Vec<u8>>) -> Result<Socket> {
    let bind = if address.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = Arc::new(UdpSocket::bind(bind).await?);
    socket.connect(address).await?;
    let read_socket = socket.clone();
    let reader = tokio::spawn(async move {
        let mut buffer = vec![0; 65536];
        loop {
            match read_socket.recv(&mut buffer).await {
                Ok(n) => {
                    if sender.send(buffer[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    });
    Ok(Socket {
        socket,
        reader,
        created: Instant::now(),
    })
}

/// Console I/O is supplied by the existing Windows SSH adapter, keeping the
/// protocol portable and compiled by the ordinary macOS Release build as well.
pub async fn run(
    endpoint: Endpoint,
    input: &mut mpsc::Receiver<Result<Vec<u8>>>,
    size: impl Fn() -> Result<(u32, u32)>,
    mut output: impl FnMut(&[u8]) -> Result<()>,
) -> Result<i32> {
    let dimensions = || -> Result<(u16, u16)> {
        let (cols, rows) = size()?;
        let cols = cols.clamp(1, 1000);
        let rows = rows.clamp(1, (100_000 / cols).min(1000));
        Ok((cols as u16, rows as u16))
    };
    let mut current_size = dimensions()?;
    let mut session = Session::new(&endpoint.key, current_size.0, current_size.1)?;
    let (sender, mut packets) = mpsc::channel(64);
    let mut sockets = vec![socket(endpoint.address, sender.clone()).await?];
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_hop = Instant::now();
    let mut escape = false;
    let mut notice = None;
    output(b"\x1b[?1049h\x1b[?1h")?;
    tracing::info!(port = endpoint.address.port(), "Mosh UDP session started");
    let result = async {
        loop {
            tokio::select! {
                bytes = input.recv(), if session.closing.is_none() => {
                    match bytes {
                        Some(Ok(bytes)) if !bytes.is_empty() => {
                            let mut forward = Vec::new();
                            for byte in bytes {
                                if escape {
                                    escape = false;
                                    match byte {
                                        b'.' => { session.close(); break; }
                                        30 | b'^' => forward.push(30),
                                        _ => { forward.push(30); forward.push(byte); }
                                    }
                                } else if byte == 30 { escape = true; }
                                else { forward.push(byte); }
                            }
                            if !forward.is_empty() && session.closing.is_none() {
                                session.event(proto::input(&forward))?;
                            }
                        }
                        Some(Err(error)) => return Err(error),
                        _ => session.close(),
                    }
                }
                Some(packet) = packets.recv() => {
                    match session.receive(&packet) {
                        Ok(paint) => {
                            if notice.take().is_some() { output(&session.frame())?; }
                            else if let Some(paint) = paint { output(&paint)?; }
                        }
                        // Unauthenticated/invalid datagrams cannot end a session
                        // or leak peer-provided contents into the terminal/log.
                        Err(_) => tracing::debug!("Discarded invalid Mosh datagram"),
                    }
                }
                _ = tick.tick() => {
                    if session.close_acked || session.closing.is_some_and(|at| at.elapsed() >= Duration::from_secs(1)) {
                        return Ok(0);
                    }
                    let next_size = dimensions()?;
                    if next_size != current_size && session.closing.is_none() {
                        current_size = next_size;
                        session.event(proto::resize(current_size.0, current_size.1))?;
                        output(&session.frame())?;
                    }
                    if session.last_roundtrip.elapsed() >= Duration::from_secs(10) && last_hop.elapsed() >= Duration::from_secs(10) {
                        last_hop = Instant::now();
                        match socket(endpoint.address, sender.clone()).await {
                            Ok(next) => { sockets.push(next); tracing::info!("Mosh selected a new local UDP port"); }
                            Err(error) => tracing::debug!(%error, "Mosh UDP port change deferred"),
                        }
                    }
                    while sockets.len() > 1 && sockets[0].created.elapsed() > Duration::from_secs(60) { sockets.remove(0); }
                    for packet in session.packets()? {
                        if let Err(error) = sockets.last().unwrap().socket.send(&packet).await {
                            tracing::debug!(%error, "Mosh UDP send deferred; state remains pending");
                        }
                    }
                    if session.remote_closed { return Ok(0); } // final server ACK was sent above
                    let seconds = session.last_contact.elapsed().as_secs();
                    if seconds >= 5 && notice != Some(seconds) {
                        let text = format!("Mosh: no UDP reply for {seconds}s; Ctrl-^ . to quit");
                        let text: String = text.chars().take(current_size.0 as usize).collect();
                        output(format!("\x1b7\x1b[{};1H\x1b[0;7m{text}\x1b[K\x1b[0m\x1b8", current_size.1).as_bytes())?;
                        notice = Some(seconds);
                    }
                }
            }
        }
    }.await;
    let reset = output(b"\x1b[0m\x1b[?25h\x1b[?1l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1049l");
    tracing::info!("Mosh UDP session ended");
    result.and_then(|code| reset.map(|_| code))
}
