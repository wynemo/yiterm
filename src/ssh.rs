//! Ordinary SSH sessions are launched through a private PATH shim. On Unix the
//! shim execs OpenSSH, so job control, signals and exit status remain native.
use anyhow::{Context, Result, bail, ensure};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub host: String,
    /// Connection-only options, in scp syntax. Never remote commands/forwarding.
    pub options: Vec<String>,
}

pub struct Integration {
    directory: tempfile::TempDir,
    real_ssh: Option<PathBuf>,
    pub real_scp: PathBuf,
}
impl Integration {
    pub fn new(executable: &Path) -> Result<Self> {
        // Keep Unix socket paths short, and all sockets/scripts user-private.
        #[cfg(unix)]
        let directory = tempfile::Builder::new()
            .prefix("yiterm-")
            .tempdir_in("/tmp")?;
        #[cfg(not(unix))]
        let directory = tempfile::Builder::new().prefix("yiterm-").tempdir()?;
        let real_ssh = std::env::var_os("YITERM_REAL_SSH")
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .or_else(|| find_program("ssh"));
        let real_scp = find_program("scp").unwrap_or_else(|| PathBuf::from("scp"));
        let bin = directory.path().join("bin");
        std::fs::create_dir(&bin)?;
        if real_ssh.is_some() {
            let shim = bin.join(if cfg!(windows) { "ssh.exe" } else { "ssh" });
            #[cfg(unix)]
            std::os::unix::fs::symlink(executable, shim)?;
            #[cfg(windows)]
            {
                // The GUI executable must stay windowless, but its SSH shim
                // must inherit ConPTY and make PowerShell wait for its exit.
                // Write a private copy: changing a hard link would also change
                // the original application's subsystem.
                let mut image = std::fs::read(executable)?;
                console_shim_image(&mut image)?;
                std::fs::write(&shim, image)?;
            }
            #[cfg(all(not(unix), not(windows)))]
            std::fs::copy(executable, &shim)?;
        }
        Ok(Self {
            directory,
            real_ssh,
            real_scp,
        })
    }
    pub fn direct(&self, host: &str) -> Result<portable_pty::CommandBuilder> {
        ensure!(self.real_ssh.is_some(), "OpenSSH is not installed");
        let executable =
            self.directory
                .path()
                .join("bin")
                .join(if cfg!(windows) { "ssh.exe" } else { "ssh" });
        let mut command = portable_pty::CommandBuilder::new(executable);
        command.args(["-tt", "--", host]);
        self.configure(&mut command)?;
        Ok(command)
    }
    pub fn configure(&self, command: &mut portable_pty::CommandBuilder) -> Result<()> {
        let bin = self.directory.path().join("bin");
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command.env("PATH", std::env::join_paths(paths)?);
        command.env("YITERM_SSH_DIRECTORY", self.directory.path());
        #[cfg(windows)]
        for (key, value) in crate::logging::child_environment() {
            command.env(key, value);
        }
        if let Some(real_ssh) = &self.real_ssh {
            command.env("YITERM_REAL_SSH", real_ssh);
        }
        Ok(())
    }

    #[cfg(windows)]
    pub fn listen_for_osc52(&self) -> Result<crate::native_ssh::ClipboardRequests> {
        crate::native_ssh::ClipboardRequests::listen(crate::native_ssh::osc52_pipe_name(
            self.directory.path(),
        )?)
    }
    pub fn shell(&self, shell: &str) -> Result<portable_pty::CommandBuilder> {
        #[cfg(windows)]
        let resolved = crate::settings::resolve_windows_shell(shell)?;
        #[cfg(windows)]
        let shell = resolved
            .to_str()
            .context("Shell path is not valid Unicode")?;
        let mut command = portable_pty::CommandBuilder::new(shell);
        let bin = self
            .directory
            .path()
            .join("bin")
            .to_string_lossy()
            .into_owned();
        let quoted = crate::paste::quote_posix(&bin);
        let home = std::env::var_os("HOME")
            .or_else(|| {
                if cfg!(windows) {
                    std::env::var_os("USERPROFILE")
                } else {
                    None
                }
            })
            .map(PathBuf::from)
            .unwrap_or_default();
        let name = Path::new(shell)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        match name.as_str() {
            "fish" => {
                // Runs after the user's config, which may otherwise reorder PATH.
                command.args(["-C", &format!("set -gx PATH {quoted} $PATH")]);
            }
            "bash" => {
                let rc = self.directory.path().join("bashrc");
                let bash_bin = if cfg!(windows) {
                    format!("\"$(cygpath -u {quoted})\"")
                } else {
                    quoted.clone()
                };
                std::fs::write(
                    &rc,
                    format!(
                        "[ ! -f {0} ] || . {0}\nexport PATH={bash_bin}:\"$PATH\"\n",
                        crate::paste::quote_posix(&home.join(".bashrc").to_string_lossy())
                    ),
                )?;
                command.arg("--rcfile");
                command.arg(rc);
                command.arg("-i");
            }
            "zsh" => {
                let original = std::env::var_os("ZDOTDIR")
                    .map(PathBuf::from)
                    .unwrap_or(home);
                command.env("YITERM_ORIGINAL_ZDOTDIR", &original);
                let root = crate::paste::quote_posix(&self.directory.path().to_string_lossy());
                std::fs::write(
                    self.directory.path().join(".zshenv"),
                    format!(
                        "ZDOTDIR=$YITERM_ORIGINAL_ZDOTDIR\n[[ ! -f $ZDOTDIR/.zshenv ]] || source $ZDOTDIR/.zshenv\nYITERM_ORIGINAL_ZDOTDIR=${{ZDOTDIR:-$HOME}}\nZDOTDIR={root}\n"
                    ),
                )?;
                std::fs::write(
                    self.directory.path().join(".zshrc"),
                    format!(
                        "ZDOTDIR=$YITERM_ORIGINAL_ZDOTDIR\n[[ ! -f $ZDOTDIR/.zshrc ]] || source $ZDOTDIR/.zshrc\nexport PATH={quoted}:\"$PATH\"\n"
                    ),
                )?;
                command.env("ZDOTDIR", self.directory.path());
                command.arg("-i");
            }
            "powershell" | "pwsh" => {
                let quoted = crate::paste::quote_powershell(&format!("{bin};"));
                command.args([
                    "-NoExit",
                    "-Command",
                    &format!("$env:PATH = {quoted} + $env:PATH"),
                ]);
            }
            _ => {}
        }
        self.configure(&mut command)?;
        Ok(command)
    }
}

// PE32 and PE32+ both store Subsystem at optional-header offset 68.
// https://learn.microsoft.com/en-us/windows/win32/debug/pe-format
#[cfg(any(windows, test))]
fn console_shim_image(image: &mut [u8]) -> Result<()> {
    ensure!(image.get(..2) == Some(b"MZ"), "Invalid SSH shim DOS header");
    let offset = image
        .get(0x3c..0x40)
        .context("Missing SSH shim PE offset")?;
    let pe = u32::from_le_bytes(offset.try_into()?) as usize;
    let header_end = pe.checked_add(24).context("Invalid SSH shim PE offset")?;
    let header = image
        .get(pe..header_end)
        .context("Missing SSH shim PE header")?;
    ensure!(&header[..4] == b"PE\0\0", "Invalid SSH shim PE signature");
    let size = u16::from_le_bytes(header[20..22].try_into()?) as usize;
    let end = header_end
        .checked_add(size)
        .context("Invalid SSH shim header size")?;
    let optional = image
        .get_mut(header_end..end)
        .context("Missing SSH shim optional header")?;
    ensure!(
        optional.len() >= 70,
        "SSH shim optional header is too short"
    );
    let magic = u16::from_le_bytes(optional[..2].try_into()?);
    ensure!(
        matches!(magic, 0x10b | 0x20b),
        "Unsupported SSH shim PE format"
    );
    optional[68..70].copy_from_slice(&3u16.to_le_bytes());
    Ok(())
}

fn find_program(name: &str) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(&name))
        .find(|path| path.is_file())
}

/// Called before GUI startup when invoked as the session's `ssh` shim.
pub fn is_wrapper() -> bool {
    std::env::args_os()
        .next()
        .and_then(|p| PathBuf::from(p).file_stem().map(|s| s.to_os_string()))
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("ssh"))
        && std::env::var_os("YITERM_REAL_SSH").is_some()
}

fn with_multiplexing(args: Vec<OsString>, directory: &Path, unix: bool) -> Vec<OsString> {
    if !unix {
        return args;
    }
    let mut defaults = vec![
        OsString::from("-oControlMaster=auto"),
        OsString::from("-oControlPersist=no"),
        OsString::from(format!("-oControlPath={}/c-%C", directory.display())),
    ];
    // Insert defaults before the destination, after all user flags. OpenSSH's
    // first-value-wins rule keeps explicit user -o/-S options authoritative.
    let strings: Vec<_> = args
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    let index = destination_index(&strings).unwrap_or(args.len());
    let mut output = args[..index].to_vec();
    output.append(&mut defaults);
    output.extend_from_slice(&args[index..]);
    output
}

pub fn run_wrapper() -> Result<()> {
    let real = std::env::var_os("YITERM_REAL_SSH").context("Missing real SSH executable")?;
    let directory = PathBuf::from(
        std::env::var_os("YITERM_SSH_DIRECTORY").context("Missing session directory")?,
    );
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    #[cfg(windows)]
    let args = {
        if !crate::native_ssh::config::system_requested(&args) {
            let code = crate::native_ssh::run(Path::new(&real), &directory, args)?;
            std::process::exit(code);
        }
        let mut args = args;
        if args.first().is_some_and(|arg| arg == "--system") {
            args.remove(0);
        }
        args
    };
    let args = with_multiplexing(args, &directory, cfg!(unix));
    let mut command = Command::new(real);
    command.args(args);
    // Wrapper variables must not propagate to remote sessions via SendEnv.
    command
        .env_remove("YITERM_REAL_SSH")
        .env_remove("YITERM_SSH_DIRECTORY");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec().into())
    }
    #[cfg(not(unix))]
    {
        let status = command.status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

const VALUE_FLAGS: &str = "BbcDEeFIiJLlmOopQRSWw";
fn destination_index(args: &[String]) -> Result<usize> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return Ok(index);
        }
        if !arg.starts_with('-') || arg == "-" {
            return Ok(index);
        }
        let chars: Vec<_> = arg[1..].chars().collect();
        for (offset, flag) in chars.iter().enumerate() {
            if VALUE_FLAGS.contains(*flag) {
                if offset + 1 == chars.len() {
                    index += 1;
                    ensure!(index < args.len(), "SSH option -{flag} needs a value");
                }
                break;
            }
        }
        index += 1;
    }
    bail!("SSH destination is missing")
}

pub fn parse(args: &[String]) -> Result<Remote> {
    let mut options = Vec::new();
    let mut index = 0;
    let mut host = None;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            index += 1;
            host = args.get(index).cloned();
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            host = Some(arg.clone());
            break;
        }
        let chars: Vec<_> = arg[1..].chars().collect();
        for (offset, &flag) in chars.iter().enumerate() {
            ensure!(
                !"NfOGQVWT".contains(flag),
                "Foreground SSH is not an interactive terminal session (-{flag})"
            );
            if VALUE_FLAGS.contains(flag) {
                let value = if offset + 1 < chars.len() {
                    chars[offset + 1..].iter().collect::<String>()
                } else {
                    index += 1;
                    args.get(index)
                        .context("SSH option value is missing")?
                        .clone()
                };
                match flag {
                    'p' => {
                        options.extend(["-P".into(), value]);
                    }
                    'l' => options.extend(["-o".into(), format!("User={value}")]),
                    'S' => options.extend(["-o".into(), format!("ControlPath={value}")]),
                    'B' => options.extend(["-o".into(), format!("BindInterface={value}")]),
                    'b' => options.extend(["-o".into(), format!("BindAddress={value}")]),
                    'F' | 'i' | 'J' | 'c' => options.extend([format!("-{flag}"), value]),
                    'm' => options.extend(["-o".into(), format!("MACs={value}")]),
                    'I' => options.extend(["-o".into(), format!("PKCS11Provider={value}")]),
                    'o' => {
                        let key = value
                            .split(['=', ' ', '\t'])
                            .next()
                            .unwrap_or_default()
                            .to_ascii_lowercase();
                        if ![
                            "controlmaster",
                            "controlpersist",
                            "remotecommand",
                            "requesttty",
                            "sessiontype",
                            "localcommand",
                            "permitlocalcommand",
                            "forkafterauthentication",
                            "forwardagent",
                            "forwardx11",
                            "forwardx11trusted",
                            "localforward",
                            "remoteforward",
                            "dynamicforward",
                            "clearallforwardings",
                            "exitonforwardfailure",
                        ]
                        .contains(&key.as_str())
                        {
                            options.extend(["-o".into(), value]);
                        }
                    }
                    _ => {}
                }
                break;
            }
            if "46C".contains(flag) {
                options.push(format!("-{flag}"));
            }
        }
        index += 1;
    }
    let mut host = host.context("SSH destination is missing")?;
    if host.starts_with("ssh://") {
        let url = url::Url::parse(&host)?;
        let hostname = url.host_str().context("SSH URL has no host")?;
        let hostname = hostname.trim_matches(['[', ']']);
        if let Some(port) = url.port() {
            options.extend(["-P".into(), port.to_string()]);
        }
        host = if url.username().is_empty() {
            hostname.to_string()
        } else {
            format!("{}@{hostname}", url.username())
        };
    }
    ensure!(
        !host.is_empty() && !host.starts_with('-') && !host.chars().any(char::is_whitespace),
        "Invalid SSH destination"
    );
    Ok(Remote { host, options })
}

impl Remote {
    pub fn scp_destination(&self) -> String {
        let (user, host) = self
            .host
            .rsplit_once('@')
            .map_or((None, self.host.as_str()), |(u, h)| (Some(u), h));
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.into()
        };
        user.map_or(host.clone(), |user| format!("{user}@{host}"))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TargetIdentity {
    pub pid: u32,
    pub group: Option<i32>,
    pub start_time: u64,
}
#[derive(Clone)]
pub struct Detection {
    pub remote: Remote,
    pub identity: TargetIdentity,
    /// Only a built-in Windows SSH shim owns this upload pipe.
    pub upload_pipe: Option<String>,
}

fn is_descendant(system: &System, mut pid: Pid, root: Pid) -> bool {
    for _ in 0..128 {
        if pid == root {
            return true;
        }
        let Some(parent) = system.process(pid).and_then(|p| p.parent()) else {
            return false;
        };
        if parent == pid {
            return false;
        }
        pid = parent;
    }
    false
}

pub fn detect(root: u32, group: Option<i32>) -> Result<Option<Detection>> {
    // exec/exit can briefly expose an SSH process with incomplete argv. Retry
    // before reporting an error; never fall back to a local path on an error.
    for _ in 0..3 {
        match detect_once(root, group) {
            Ok(found) => return Ok(found),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
    detect_once(root, group)
}

fn detect_once(root: u32, group: Option<i32>) -> Result<Option<Detection>> {
    #[cfg(unix)]
    ensure!(
        group.is_some(),
        "Cannot identify the terminal foreground job; image was not pasted"
    );
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    ensure!(
        system.process(Pid::from_u32(root)).is_some(),
        "Terminal shell has exited"
    );
    let candidates: Vec<_> = system
        .processes()
        .iter()
        .filter_map(|(&pid, process)| {
            let name = process.name().to_string_lossy();
            if !(name == "ssh" || name.eq_ignore_ascii_case("ssh.exe")) {
                return None;
            }
            #[cfg(unix)]
            {
                let current_group = unsafe { libc::getpgid(pid.as_u32() as i32) };
                if group != Some(current_group) {
                    return None;
                }
            }
            #[cfg(not(unix))]
            if !is_descendant(&system, pid, Pid::from_u32(root)) {
                return None;
            }
            Some(pid)
        })
        .collect();
    if candidates.is_empty() {
        return Ok(None);
    }
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&candidates),
        true,
        ProcessRefreshKind::nothing()
            .without_tasks()
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always),
    );
    let mut real_candidates = Vec::new();
    for pid in candidates {
        let Some(process) = system.process(pid) else {
            continue;
        };
        // Explicit system-SSH invocations still have a waiting shim parent.
        // Native shims own the connection themselves and must be selected.
        if shim_directory(process.exe()).is_some()
            && (!cfg!(windows)
                || crate::native_ssh::config::system_requested(
                    &process.cmd().iter().skip(1).cloned().collect::<Vec<_>>(),
                ))
        {
            continue;
        }
        real_candidates.push(pid);
    }
    // ProxyJump/ProxyCommand SSH helpers inherit the foreground process group.
    // Select the outer SSH session, not its transport children (e.g. ssh -W).
    // Exclude our Windows shim above before checking ancestry.
    let mut found = Vec::new();
    for &pid in &real_candidates {
        if real_candidates
            .iter()
            .any(|&parent| parent != pid && is_descendant(&system, pid, parent))
        {
            tracing::debug!(pid = pid.as_u32(), "Ignoring SSH transport child");
            continue;
        }
        let Some(process) = system.process(pid) else {
            continue;
        };
        let cmd = process.cmd();
        ensure!(
            !cmd.is_empty(),
            "Cannot read foreground SSH arguments; image was not pasted"
        );
        let args: Vec<_> = cmd
            .iter()
            .skip(1)
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        let remote = parse(&args)?;
        found.push(Detection {
            remote,
            upload_pipe: if cfg!(windows) {
                shim_directory(process.exe())
                    .map(|directory| crate::native_ssh::pipe_name(directory, pid.as_u32()))
                    .transpose()?
            } else {
                None
            },
            identity: TargetIdentity {
                pid: pid.as_u32(),
                group,
                start_time: process.start_time(),
            },
        });
    }
    ensure!(
        found.len() <= 1,
        "Multiple SSH sessions are active; cannot safely choose the image destination"
    );
    Ok(found.pop())
}

fn shim_directory(executable: Option<&Path>) -> Option<&Path> {
    let bin = executable?.parent()?;
    let directory = bin.parent()?;
    (bin.file_name()? == "bin"
        && directory
            .file_name()?
            .to_string_lossy()
            .starts_with("yiterm-"))
    .then_some(directory)
}

pub fn target_is_alive(target: TargetIdentity, group: Option<i32>) -> bool {
    if target.group != group {
        return false;
    }
    let pid = Pid::from_u32(target.pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    system
        .process(pid)
        .is_some_and(|process| process.start_time() == target.start_time)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_shim_uses_console_subsystem_for_both_pe_formats() {
        for magic in [0x10bu16, 0x20b] {
            let mut image = vec![0u8; 512];
            image[..2].copy_from_slice(b"MZ");
            image[0x3c..0x40].copy_from_slice(&128u32.to_le_bytes());
            image[128..132].copy_from_slice(b"PE\0\0");
            image[148..150].copy_from_slice(&240u16.to_le_bytes());
            image[152..154].copy_from_slice(&magic.to_le_bytes());
            image[220..222].copy_from_slice(&2u16.to_le_bytes());
            let original = image.clone();
            console_shim_image(&mut image).unwrap();
            assert_eq!(&image[220..222], &3u16.to_le_bytes());
            image[220..222].copy_from_slice(&2u16.to_le_bytes());
            assert_eq!(image, original);
            for length in [0, 63, 131, 151, 221, 391] {
                assert!(console_shim_image(&mut original[..length].to_vec()).is_err());
            }
        }
        let mut invalid = vec![0u8; 64];
        invalid[..2].copy_from_slice(b"MZ");
        invalid[0x3c..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(console_shim_image(&mut invalid).is_err());
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).into()).collect()
    }
    #[test]
    fn preserves_connection_options_and_removes_commands_and_forwarding() {
        let remote = parse(&args(&[
            "-vvAp2222",
            "-i",
            "/a key",
            "-Jjump",
            "-L",
            "8080:localhost:80",
            "-S",
            "/socket",
            "-oRemoteCommand=bad",
            "user@host",
            "codex",
        ]))
        .unwrap();
        assert_eq!(remote.host, "user@host");
        assert_eq!(
            remote.options,
            args(&[
                "-P",
                "2222",
                "-i",
                "/a key",
                "-J",
                "jump",
                "-o",
                "ControlPath=/socket"
            ])
        );
    }
    #[test]
    fn defaults_are_before_destination_but_after_user_options() {
        let original = args(&[
            "-p",
            "2222",
            "-oControlMaster=no",
            "--",
            "host",
            "echo",
            "ok",
        ]);
        let wrapped = with_multiplexing(
            original.into_iter().map(Into::into).collect(),
            Path::new("/tmp/private"),
            true,
        );
        assert_eq!(wrapped[2], "-oControlMaster=no");
        assert_eq!(wrapped[3], "-oControlMaster=auto");
        assert_eq!(wrapped[6], "--");
        assert_eq!(wrapped[7], "host");
    }
    #[test]
    fn supports_uri_and_ipv6_and_rejects_noninteractive() {
        let remote = parse(&args(&["ssh://alice@[::1]:2222"])).unwrap();
        assert_eq!(remote.scp_destination(), "alice@[::1]");
        assert_eq!(remote.options, args(&["-P", "2222"]));
        assert!(parse(&args(&["-N", "host"])).is_err());
        assert!(parse(&args(&["-p"])).is_err());
    }
}
