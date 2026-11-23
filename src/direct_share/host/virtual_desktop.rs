//! A dedicated main-queue process owns the virtual display, like the native
//! Cindy helper. The listener retains it across viewer disconnections.
use anyhow::{Context, Result, ensure};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

pub(super) struct VirtualDesktop {
    child: Child,
    input: Option<ChildStdin>,
    replies: Receiver<String>,
    id: u32,
    size: Option<[u16; 3]>,
}
impl VirtualDesktop {
    pub(super) fn create(size: [u16; 3]) -> Result<Self> {
        let mut child = Command::new(std::env::current_exe()?)
            .arg("--internal-virtual-display")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit())
            .spawn().context("Cannot start the virtual display owner")?;
        let input = child.stdin.take().context("Missing virtual display input")?;
        let output = child.stdout.take().context("Missing virtual display output")?;
        let (sender, replies) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() { break }
            }
        });
        let mut owner = Self { child, input: Some(input), replies, id: 0, size: None };
        owner.resize(size)?;
        Ok(owner)
    }
    pub(super) fn id(&self) -> Result<u32> {
        ensure!(self.id != 0, "The iPad virtual display is unavailable");
        Ok(self.id)
    }
    pub(super) fn resize(&mut self, size: [u16; 3]) -> Result<()> {
        ensure!(self.child.try_wait()?.is_none(), "The virtual display owner exited; restart sharing");
        if self.size == Some(size) { return Ok(()); }
        self.size = None; // A failed resize must be retried, not mistaken for the cached mode.
        let [w, h, scale] = size;
        let input = self.input.as_mut().context("Virtual display owner input closed")?;
        writeln!(input, "{{\"width\":{w},\"height\":{h},\"scale\":{scale}}}")?;
        input.flush()?;
        let line = self.replies.recv_timeout(Duration::from_secs(16))
            .context("Virtual display owner did not respond")?;
        let reply: serde_json::Value = serde_json::from_str(&line)
            .context("Invalid virtual display owner reply")?;
        if let Some(error) = reply.get("error").and_then(|value| value.as_str()) {
            anyhow::bail!("Mac virtual display: {error}");
        }
        let id = reply.get("id").and_then(|value| value.as_u64())
            .and_then(|id| u32::try_from(id).ok()).filter(|id| *id != 0)
            .context("Missing registered virtual display ID")?;
        ensure!(self.id == 0 || id == self.id, "Virtual display identity changed; restart sharing");
        self.id = id;
        self.size = Some(size);
        Ok(())
    }
}
impl Drop for VirtualDesktop {
    fn drop(&mut self) {
        self.input.take(); // EOF asks the owner to release its screen on its main queue.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !matches!(self.child.try_wait(), Ok(None)) { return; }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
