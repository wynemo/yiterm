//! GitHub Release updates. Network/staging work never runs on the UI thread.
use anyhow::{Context, Result, bail, ensure};
use self_update::update::{Release, ReleaseAsset};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

const HELPER_ARG: &str = "--internal-apply-update";
const RELEASES_URL: &str = "https://github.com/wynemo/yiterm/releases";

pub enum State {
    Idle,
    Checking,
    Current,
    Available(Release),
    Downloading,
    Ready(Prepared),
    Starting,
    Restarting,
    Failed(String),
}

impl State {
    pub fn message(&self) -> String {
        match self {
            Self::Idle => String::new(),
            Self::Checking => "Checking GitHub Releases…".into(),
            Self::Current => "You are up to date.".into(),
            Self::Available(release) => format!("Version {} is available.", release.version()),
            Self::Downloading => "Downloading and verifying update…".into(),
            Self::Ready(_) => "Update ready. Restart disconnects ALL terminal sessions.".into(),
            Self::Starting => "Preparing to restart…".into(),
            Self::Restarting => "Restarting…".into(),
            Self::Failed(error) => format!("Update failed: {error}"),
        }
    }

    pub fn action_label(&self) -> Option<&'static str> {
        match self {
            Self::Idle | Self::Current | Self::Failed(_) => Some("Check for Updates"),
            Self::Available(_) => Some("Download Update"),
            Self::Ready(_) => Some("Update and Restart"),
            _ => None,
        }
    }

    pub fn notes_url(&self) -> Option<String> {
        match self {
            Self::Available(release) => Some(format!("{RELEASES_URL}/tag/v{}", release.version())),
            Self::Ready(prepared) => Some(format!("{RELEASES_URL}/tag/v{}", prepared.version)),
            _ => None,
        }
    }
}

pub struct Controller {
    pub state: State,
    result: Option<mpsc::Receiver<Result<State>>>,
}

impl Default for Controller {
    fn default() -> Self {
        let state = std::env::current_exe()
            .ok()
            .and_then(|exe| {
                let path = exe.parent()?.join(".yiterm-update-error.txt");
                let error = fs::read_to_string(&path).ok()?;
                let _ = fs::remove_file(path);
                Some(State::Failed(error))
            })
            .unwrap_or(State::Idle);
        Self {
            state,
            result: None,
        }
    }
}

impl Controller {
    pub fn act(&mut self) {
        if self.state.action_label().is_none() {
            return;
        }
        let previous = std::mem::replace(&mut self.state, State::Checking);
        let job: Box<dyn FnOnce() -> Result<State> + Send> = match previous {
            State::Available(release) => {
                self.state = State::Downloading;
                Box::new(move || prepare(&release).map(State::Ready))
            }
            State::Ready(prepared) => {
                self.state = State::Starting;
                Box::new(move || {
                    prepared.handoff()?;
                    Ok(State::Restarting)
                })
            }
            _ => Box::new(check),
        };
        let (tx, rx) = mpsc::channel();
        self.result = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(job());
        });
    }

    /// Returns true when the UI needs to redraw. Restarting is acknowledged by the UI.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.result else {
            return false;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!("Update worker stopped")),
        };
        self.result = None;
        self.state = result.unwrap_or_else(|error| {
            tracing::warn!(%error, "Update failed");
            State::Failed(format!("{error:#}"))
        });
        true
    }
}

fn check() -> Result<State> {
    platform()?;
    let releases = self_update::backends::github::Update::configure()
        .repo_owner("wynemo")
        .repo_name("yiterm")
        .bin_name(executable())
        .current_version(env!("CARGO_PKG_VERSION"))
        .timeout(Duration::from_secs(20))
        .build()?
        .get_latest_release()?;
    let release = releases
        .latest()
        .context("No stable release is available")?;
    if !newer(release.version(), env!("CARGO_PKG_VERSION"))? {
        return Ok(State::Current);
    }
    release_assets(release)?;
    Ok(State::Available(release.clone()))
}

fn newer(candidate: &str, current: &str) -> Result<bool> {
    let candidate = semver::Version::parse(candidate)?;
    Ok(candidate.pre.is_empty() && candidate > semver::Version::parse(current)?)
}

fn platform() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("macos-arm64"),
        ("windows", "x86_64") => Ok("windows-x64"),
        ("linux", "x86_64") => Ok("linux-x64"),
        _ => bail!("No update package for this platform; download manually from {RELEASES_URL}"),
    }
}

fn executable() -> &'static str {
    if cfg!(windows) {
        "yiterm.exe"
    } else {
        "yiterm"
    }
}

fn package_root(release: &Release) -> Result<String> {
    // Parse before using the remote version in any path.
    let version = semver::Version::parse(release.version())?;
    Ok(format!("yiterm-{version}-{}", platform()?))
}

fn release_assets(release: &Release) -> Result<(ReleaseAsset, ReleaseAsset)> {
    let root = package_root(release)?;
    let name = format!("{root}.{}", if cfg!(windows) { "zip" } else { "tar.gz" });
    let find = |name: &str| {
        release
            .assets()
            .iter()
            .find(|a| a.name() == name)
            .cloned()
            .with_context(|| {
                format!("Release is missing {name}; try again after publication completes")
            })
    };
    Ok((find(&name)?, find(&format!("{name}.sha256"))?))
}

fn download(asset: &ReleaseAsset, output: impl Write) -> Result<()> {
    let url = url::Url::parse(asset.download_url())?;
    ensure!(
        url.scheme() == "https" && url.host_str() == Some("api.github.com"),
        "Unexpected release asset host"
    );
    self_update::Download::from_url(asset.download_url())
        .timeout(Duration::from_secs(300))
        .request_header(
            self_update::http::header::ACCEPT,
            "application/octet-stream",
        )
        .download_to(output)?;
    Ok(())
}

fn verify_checksum(archive: &Path, checksum: &str, filename: &str) -> Result<()> {
    let fields: Vec<_> = checksum.split_whitespace().collect();
    ensure!(
        fields.len() == 2
            && fields[1] == filename
            && fields[0].len() == 64
            && fields[0].bytes().all(|c| c.is_ascii_hexdigit()),
        "Invalid SHA-256 file"
    );
    let mut file = File::open(archive)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    ensure!(
        format!("{:x}", hash.finalize()).eq_ignore_ascii_case(fields[0]),
        "SHA-256 mismatch; update was not installed"
    );
    Ok(())
}

pub struct Prepared {
    staging: tempfile::TempDir,
    version: String,
    target: PathBuf,
    files: Vec<PathBuf>,
}

fn prepare(release: &Release) -> Result<Prepared> {
    let target_exe = std::env::current_exe()?.canonicalize()?;
    let target = target_exe
        .parent()
        .context("Missing installation directory")?
        .to_owned();
    ensure!(
        target_exe.file_name() == Some(executable().as_ref()),
        "Run the release executable with its original filename before updating"
    );
    ensure!(
        !target
            .components()
            .any(|part| part.as_os_str().to_string_lossy().ends_with(".app")),
        "Bundle updates are not supported; download the new application manually"
    );
    let staging = tempfile::Builder::new()
        .prefix(".yiterm-update-")
        .tempdir_in(&target)
        .context("Installation directory is not writable; move YiTerm to a writable folder")?;
    let (asset, checksum_asset) = release_assets(release)?;
    let archive = staging.path().join(asset.name());
    download(
        &asset,
        LimitedWriter {
            inner: File::create(&archive)?,
            remaining: 512 * 1024 * 1024,
        },
    )?;
    let mut checksum = Vec::new();
    download(
        &checksum_asset,
        LimitedWriter {
            inner: &mut checksum,
            remaining: 4096,
        },
    )?;
    verify_checksum(&archive, std::str::from_utf8(&checksum)?, asset.name())?;
    let extracted = staging.path().join("payload");
    fs::create_dir(&extracted)?;
    self_update::Extract::from_source(&archive).extract_into(&extracted)?;
    let root = extracted.join(package_root(release)?);
    let files = payload_files(&root)?;
    validate_payload(&root, &files)?;
    Ok(Prepared {
        staging,
        version: release.version().to_owned(),
        target,
        files,
    })
}

struct LimitedWriter<W> {
    inner: W,
    remaining: usize,
}
impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(std::io::Error::other("Download exceeds size limit"));
        }
        let count = self.inner.write(bytes)?;
        self.remaining -= count;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn payload_files(root: &Path) -> Result<Vec<PathBuf>> {
    fn visit(root: &Path, relative: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(root.join(relative))? {
            let entry = entry?;
            let path = relative.join(entry.file_name());
            let kind = entry.file_type()?;
            ensure!(!kind.is_symlink(), "Update contains a symbolic link");
            if kind.is_dir() {
                visit(root, &path, files)?;
            } else {
                ensure!(kind.is_file(), "Update contains a special file");
                files.push(path);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    visit(root, Path::new(""), &mut files)?;
    files.sort();
    Ok(files)
}

fn validate_payload(root: &Path, files: &[PathBuf]) -> Result<()> {
    let required: &[&str] = if cfg!(windows) {
        &["yiterm.exe", "bin/ssh.exe", "conpty.dll", "OpenConsole.exe"]
    } else {
        &["yiterm"]
    };
    for name in required {
        ensure!(
            files.contains(&PathBuf::from(name)) && fs::metadata(root.join(name))?.len() > 0,
            "Update is missing required file {name}"
        );
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Plan {
    parent: u32,
    target: PathBuf,
    payload: PathBuf,
    files: Vec<PathBuf>,
    args: Vec<OsString>,
    cwd: PathBuf,
}

impl Prepared {
    fn handoff(self) -> Result<()> {
        let root = self.staging.path();
        let helper = root.join(if cfg!(windows) {
            "updater.exe"
        } else {
            "updater"
        });
        fs::copy(std::env::current_exe()?, &helper)?;
        let plan = Plan {
            parent: std::process::id(),
            target: self.target,
            payload: root
                .join("payload")
                .join(format!("yiterm-{}-{}", self.version, platform()?)),
            files: self.files,
            args: std::env::args_os().skip(1).collect(),
            cwd: std::env::current_dir()?,
        };
        let plan_path = root.join("plan.json");
        fs::write(&plan_path, serde_json::to_vec(&plan)?)?;
        let mut command = Command::new(&helper);
        command
            .arg(HELPER_ARG)
            .arg(&plan_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command.spawn().context("Could not start update helper")?;
        let start = Instant::now();
        while !root.join("ready").is_file() {
            if let Some(status) = child.try_wait()? {
                bail!("Update helper exited before installation: {status}");
            }
            if start.elapsed() > Duration::from_secs(15) {
                let _ = child.kill();
                let _ = child.wait();
                bail!("Update helper did not become ready; application remains open");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // The helper owns staging after the ready handshake, including recovery files.
        let _ = self.staging.keep();
        Ok(())
    }
}

/// Runs before CLI parsing, logging, SSH wrappers or GUI initialization.
pub fn run_helper_if_requested() -> Option<Result<()>> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(HELPER_ARG)) {
        return None;
    }
    Some((|| {
        let path = PathBuf::from(args.next().context("Missing update plan")?);
        ensure!(args.next().is_none(), "Unexpected helper arguments");
        let root = path.parent().context("Missing staging directory")?;
        let result = apply_plan(&path);
        if let Err(error) = &result {
            let _ = fs::write(root.join("error.txt"), format!("{error:#}"));
        }
        result
    })())
}

fn apply_plan(path: &Path) -> Result<()> {
    let plan: Plan = serde_json::from_slice(&fs::read(path)?)?;
    let root = path.parent().context("Missing staging directory")?;
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(plan.target.join(".yiterm-update.lock"))?;
    lock.try_lock()
        .context("Another update is already installing")?;
    validate_payload(&plan.payload, &plan.files)?;
    // Acquire a real process handle on Windows before acknowledging readiness,
    // preventing a PID reuse race and waiting for loaded DLLs to be released.
    let parent = Parent::open(plan.parent)?;
    fs::write(root.join("ready"), b"ready")?;
    parent.wait()?;
    let stash = root.join("backup");
    fs::create_dir(&stash)?;
    let start = Instant::now();
    loop {
        match install(&plan.payload, &plan.target, &plan.files, &stash) {
            Ok(()) => break,
            Err(error)
                if fs::read_dir(&stash)?.next().is_none()
                    && start.elapsed() < Duration::from_secs(30) =>
            {
                fs::write(root.join("error.txt"), format!("Retrying: {error:#}"))?;
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(error) => {
                // MoveAll rolls back ordinary failures. Keep recovery files if rollback fails.
                let message = format!(
                    "Installation failed: {error:#}. Recovery files: {}",
                    root.display()
                );
                fs::write(plan.target.join(".yiterm-update-error.txt"), &message)?;
                if fs::read_dir(&stash)?.next().is_none() {
                    let _ = restart(&plan);
                }
                return Err(error.context("Installation failed; recovery files retained"));
            }
        }
    }
    if let Err(error) = restart(&plan) {
        let message =
            format!("Update installed but restart failed: {error:#}. Launch YiTerm manually.");
        fs::write(plan.target.join(".yiterm-update-error.txt"), &message)?;
        bail!(message);
    }
    // The framework relocates/deletes the running helper on Windows so staging
    // can be removed without a batch file or a second application launch.
    if self_replace::self_delete_outside_path(root).is_ok() {
        let _ = fs::remove_dir_all(root);
    }
    Ok(())
}

fn install(payload: &Path, target: &Path, files: &[PathBuf], stash: &Path) -> Result<()> {
    // MoveAll consumes sources even when it rolls back. Copy each attempt so a
    // Windows sharing violation can be retried without losing the staged payload.
    let attempt = tempfile::Builder::new()
        .prefix("attempt-")
        .tempdir_in(payload.parent().context("Missing payload parent")?)?;
    let mut moves = self_update::MoveAll::from_temp(stash);
    for relative in files {
        ensure!(
            !relative.as_os_str().is_empty()
                && relative
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_))),
            "Invalid update path"
        );
        // Never follow existing symlinks in an installation directory.
        let mut dest = target.to_path_buf();
        for part in relative.components() {
            dest.push(part);
            if let Ok(meta) = fs::symlink_metadata(&dest) {
                ensure!(
                    !meta.file_type().is_symlink(),
                    "Installation contains a symbolic link"
                );
            }
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let source = attempt.path().join(relative);
        fs::create_dir_all(source.parent().context("Missing source parent")?)?;
        fs::copy(payload.join(relative), &source)?;
        moves.add(&source, &dest);
    }
    moves.commit()?;
    Ok(())
}

fn restart(plan: &Plan) -> Result<Child> {
    Ok(Command::new(plan.target.join(executable()))
        .args(&plan.args)
        .current_dir(&plan.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?)
}

#[cfg(unix)]
struct Parent(u32);
#[cfg(unix)]
impl Parent {
    fn open(pid: u32) -> Result<Self> {
        ensure!(pid > 1 && pid != std::process::id(), "Invalid parent PID");
        Ok(Self(pid))
    }
    fn wait(self) -> Result<()> {
        let start = Instant::now();
        loop {
            if unsafe { libc::kill(self.0 as i32, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return Ok(());
            }
            ensure!(
                start.elapsed() < Duration::from_secs(60),
                "Application did not exit; update cancelled"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(windows)]
struct Parent(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl Parent {
    fn open(pid: u32) -> Result<Self> {
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        ensure!(
            !handle.is_null(),
            "Could not open parent process: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(handle))
    }
    fn wait(self) -> Result<()> {
        let status =
            unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(self.0, 60000) };
        ensure!(
            status == windows_sys::Win32::Foundation::WAIT_OBJECT_0,
            "Application did not exit; update cancelled"
        );
        Ok(())
    }
}
#[cfg(windows)]
impl Drop for Parent {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_payload_requires_the_dedicated_ssh_helper() {
        let root = tempfile::tempdir().unwrap();
        for name in ["yiterm.exe", "conpty.dll", "OpenConsole.exe"] {
            fs::write(root.path().join(name), b"image").unwrap();
        }
        assert!(validate_payload(root.path(), &payload_files(root.path()).unwrap()).is_err());
        fs::create_dir(root.path().join("bin")).unwrap();
        fs::write(root.path().join("bin/ssh.exe"), b"console image").unwrap();
        validate_payload(root.path(), &payload_files(root.path()).unwrap()).unwrap();
    }

    #[test]
    fn versions_only_upgrade_to_newer_stable_releases() {
        assert!(newer("0.0.12", "0.0.11").unwrap());
        assert!(newer("1.0.0", "0.0.11").unwrap());
        assert!(!newer("0.0.11", "0.0.11").unwrap());
        assert!(!newer("0.0.10", "0.0.11").unwrap());
        assert!(!newer("0.0.12-rc.1", "0.0.11").unwrap());
        assert!(newer("../../bad", "0.0.11").is_err());
    }

    #[test]
    fn release_requires_exact_platform_archive_and_checksum() {
        let root = format!("yiterm-0.0.12-{}", platform().unwrap());
        let archive = format!("{root}.{}", if cfg!(windows) { "zip" } else { "tar.gz" });
        let asset = |name: String| {
            ReleaseAsset::new(
                name,
                "https://api.github.com/repos/wynemo/yiterm/releases/assets/1",
            )
        };
        let mut builder = Release::builder();
        builder.version("0.0.12").name("v0.0.12").date("");
        builder.assets([
            asset(format!("{archive}.sha256")),
            asset(format!("wrong-{archive}")),
        ]);
        assert!(release_assets(&builder.build().unwrap()).is_err());
        builder.assets([asset(archive.clone()), asset(format!("{archive}.sha256"))]);
        let (selected, _) = release_assets(&builder.build().unwrap()).unwrap();
        assert_eq!(selected.name(), archive);
    }

    #[test]
    fn checksum_rejects_corruption_and_wrong_filename() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("release.zip");
        fs::write(&path, b"release").unwrap();
        let digest = format!("{:x}", Sha256::digest(b"release"));
        verify_checksum(&path, &format!("{digest}  release.zip\n"), "release.zip").unwrap();
        assert!(verify_checksum(&path, &format!("{digest}  other.zip"), "release.zip").is_err());
        fs::write(&path, b"corrupted").unwrap();
        assert!(verify_checksum(&path, &format!("{digest}  release.zip"), "release.zip").is_err());
    }

    #[test]
    fn failed_install_restores_old_files_and_keeps_payload_for_retry() {
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        let target = temp.path().join("target");
        let stash = temp.path().join("stash");
        for path in [&payload, &target, &stash] {
            fs::create_dir(path).unwrap();
        }
        fs::write(payload.join("first"), "new first").unwrap();
        fs::write(payload.join("second"), "new second").unwrap();
        fs::write(target.join("first"), "old first").unwrap();
        fs::write(target.join("second"), "old second").unwrap();
        // A missing source fails after the first move, exercising real rollback.
        let mut moves = self_update::MoveAll::from_temp(&stash);
        moves.add(payload.join("first"), target.join("first"));
        moves.add(payload.join("missing"), target.join("second"));
        assert!(moves.commit().is_err());
        assert_eq!(
            fs::read_to_string(target.join("first")).unwrap(),
            "old first"
        );
        assert_eq!(
            fs::read_to_string(target.join("second")).unwrap(),
            "old second"
        );
        fs::write(payload.join("first"), "new first").unwrap();
        let files = vec![PathBuf::from("first"), PathBuf::from("second")];
        install(&payload, &target, &files, &stash).unwrap();
        assert_eq!(
            fs::read_to_string(target.join("first")).unwrap(),
            "new first"
        );
        assert_eq!(
            fs::read_to_string(target.join("second")).unwrap(),
            "new second"
        );
        assert_eq!(
            fs::read_to_string(payload.join("first")).unwrap(),
            "new first"
        );
        assert_eq!(
            fs::read_to_string(stash.join("self_update-stash-0")).unwrap(),
            "old first"
        );
    }

    #[test]
    fn install_rejects_parent_paths_before_touching_files() {
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        let target = temp.path().join("target");
        let stash = temp.path().join("stash");
        for path in [&payload, &target, &stash] {
            fs::create_dir(path).unwrap();
        }
        fs::write(temp.path().join("outside"), "keep").unwrap();
        assert!(install(&payload, &target, &[PathBuf::from("../outside")], &stash).is_err());
        assert_eq!(
            fs::read_to_string(temp.path().join("outside")).unwrap(),
            "keep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_rejects_existing_symlink_directories() {
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        let target = temp.path().join("target");
        let stash = temp.path().join("stash");
        let outside = temp.path().join("outside");
        for path in [&payload, &target, &stash, &outside] {
            fs::create_dir(path).unwrap();
        }
        std::os::unix::fs::symlink(&outside, target.join("licenses")).unwrap();
        assert!(install(&payload, &target, &[PathBuf::from("licenses/file")], &stash).is_err());
        assert!(!outside.join("file").exists());
    }

    #[cfg(windows)]
    #[test]
    fn locked_windows_sidecar_rolls_back_then_retries_whole_package() {
        use std::os::windows::fs::OpenOptionsExt;
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        let target = temp.path().join("target");
        let stash = temp.path().join("stash");
        for path in [&payload, &target, &stash] {
            fs::create_dir(path).unwrap();
        }
        let files = vec![PathBuf::from("yiterm.exe"), PathBuf::from("conpty.dll")];
        for name in &files {
            fs::write(target.join(name), "old").unwrap();
            fs::write(payload.join(name), "new").unwrap();
        }
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(target.join("conpty.dll"))
            .unwrap();
        assert!(install(&payload, &target, &files, &stash).is_err());
        assert_eq!(
            fs::read_to_string(target.join("yiterm.exe")).unwrap(),
            "old"
        );
        assert_eq!(
            fs::read_to_string(payload.join("yiterm.exe")).unwrap(),
            "new"
        );
        assert!(fs::read_dir(&stash).unwrap().next().is_none());
        drop(locked);
        install(&payload, &target, &files, &stash).unwrap();
        for name in &files {
            assert_eq!(fs::read_to_string(target.join(name)).unwrap(), "new");
        }
    }
}
