//! Opt-in aggregate diagnostics; never records terminal or input contents.
use std::{
    collections::BTreeMap,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

static ENABLED: OnceLock<bool> = OnceLock::new();
static PTY_BYTES: AtomicU64 = AtomicU64::new(0);
static PTY_READS: AtomicU64 = AtomicU64::new(0);

pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var("YITERM_PROFILE").is_ok_and(|v| v == "1"))
}

pub fn pty_read(bytes: usize) {
    if enabled() {
        PTY_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
        PTY_READS.fetch_add(1, Ordering::Relaxed);
    }
}

pub struct Profile {
    since: Instant,
    system: System,
    cpu_baseline: Option<(Instant, u64)>,
    frames: u64,
    update_ms: f64,
    poll_ms: f64,
    max_update_ms: f64,
    backend_ms: f64,
    backend_samples: u64,
    causes: BTreeMap<(&'static str, u32), u64>,
}

impl Profile {
    pub fn new() -> Option<Self> {
        if !enabled() {
            return None;
        }
        Some(Self::start())
    }

    fn start() -> Self {
        let mut profile = Self {
            since: Instant::now(),
            system: System::new(),
            cpu_baseline: None,
            frames: 0,
            update_ms: 0.0,
            poll_ms: 0.0,
            max_update_ms: 0.0,
            backend_ms: 0.0,
            backend_samples: 0,
            causes: BTreeMap::new(),
        };
        profile.refresh_cpu();
        tracing::info!(target: "yiterm::profile", version = env!("CARGO_PKG_VERSION"),
            build = option_env!("YITERM_BUILD_REV").unwrap_or("local"),
            os = std::env::consts::OS, arch = std::env::consts::ARCH,
            "Profiling enabled; aggregate interval 5 seconds");
        profile
    }

    fn refresh_cpu(&mut self) -> Option<f64> {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[Pid::from_u32(std::process::id())]),
            true,
            ProcessRefreshKind::nothing().with_cpu().without_tasks(),
        );
        let now = Instant::now();
        let total_ms = self
            .system
            .process(Pid::from_u32(std::process::id()))?
            .accumulated_cpu_time();
        let previous = self.cpu_baseline.replace((now, total_ms));
        previous.and_then(|(at, ms)| cpu_percent(ms, total_ms, now.duration_since(at)))
    }

    pub fn frame(
        &mut self,
        ctx: &eframe::egui::Context,
        update: Duration,
        poll: Duration,
        backend_seconds: Option<f32>,
    ) {
        self.frames += 1;
        let ms = update.as_secs_f64() * 1000.0;
        self.update_ms += ms;
        self.max_update_ms = self.max_update_ms.max(ms);
        self.poll_ms += poll.as_secs_f64() * 1000.0;
        if let Some(seconds) = backend_seconds {
            self.backend_ms += f64::from(seconds) * 1000.0;
            self.backend_samples += 1;
        }
        for cause in ctx.repaint_causes() {
            *self.causes.entry((cause.file, cause.line)).or_default() += 1;
        }
        let elapsed = self.since.elapsed().as_secs_f64();
        if elapsed < 5.0 {
            return;
        }
        let cpu = self.refresh_cpu();
        let logical_cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        tracing::info!(target: "yiterm::profile",
            interval_s = elapsed, frames = self.frames, updates_per_s = self.frames as f64 / elapsed,
            process_cpu_core_percent = ?cpu, logical_cpus,
            update_total_ms = self.update_ms, update_max_ms = self.max_update_ms,
            poll_total_ms = self.poll_ms, backend_total_ms = self.backend_ms,
            backend_samples = self.backend_samples,
            pty_bytes = PTY_BYTES.swap(0, Ordering::Relaxed),
            pty_reads = PTY_READS.swap(0, Ordering::Relaxed),
            focused = ctx.input(|i| i.focused),
            repaint_call_sites = ?self.causes, "Performance sample");
        self.frames = 0;
        self.update_ms = 0.0;
        self.poll_ms = 0.0;
        self.max_update_ms = 0.0;
        self.backend_ms = 0.0;
        self.backend_samples = 0;
        self.causes.clear();
        self.since = Instant::now();
    }
}

fn cpu_percent(previous_ms: u64, current_ms: u64, elapsed: Duration) -> Option<f64> {
    let delta = current_ms.checked_sub(previous_ms)?;
    let seconds = elapsed.as_secs_f64();
    (seconds > 0.0).then(|| delta as f64 / (seconds * 10.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_percentage_uses_interval_deltas_and_supports_multiple_cores() {
        assert_eq!(
            cpu_percent(70000, 75000, Duration::from_secs(5)),
            Some(100.0)
        );
        assert_eq!(
            cpu_percent(70000, 90000, Duration::from_secs(5)),
            Some(400.0)
        );
        assert_eq!(cpu_percent(70000, 70000, Duration::from_secs(5)), Some(0.0));
        assert_eq!(cpu_percent(70000, 1, Duration::from_secs(5)), None);
        assert_eq!(cpu_percent(70000, 75000, Duration::ZERO), None);
    }

    #[test]
    fn profiling_emits_aggregates_only_at_interval_and_resets() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(file.reopen().unwrap())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let mut profile = Profile::start();
            let ctx = eframe::egui::Context::default();
            profile.frame(
                &ctx,
                Duration::from_millis(3),
                Duration::from_millis(1),
                None,
            );
            assert!(
                !std::fs::read_to_string(file.path())
                    .unwrap()
                    .contains("Performance sample")
            );
            profile.since = Instant::now() - Duration::from_secs(6);
            profile.frame(
                &ctx,
                Duration::from_millis(7),
                Duration::from_millis(2),
                Some(0.01),
            );
            let log = std::fs::read_to_string(file.path()).unwrap();
            assert_eq!(log.matches("Performance sample").count(), 1);
            assert!(log.contains("frames=2"));
            assert!(log.contains("update_total_ms=10"));
            assert!(log.contains("poll_total_ms=3"));
            assert!(log.contains("backend_samples=1"));
            assert_eq!(profile.frames, 0);
            assert_eq!(profile.update_ms, 0.0);
            assert!(profile.causes.is_empty());
        });
    }
}
