//! Let OpenSSH resolve Host/Include/Match and aliases; never guess their meaning.
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub user: String,
    pub port: u16,
    pub proxy_jump: Option<String>,
    pub host_key_name: String,
    pub known_hosts: Vec<PathBuf>,
    pub save_host_key: PathBuf,
    pub strict_host_key: String,
    pub identities: Vec<PathBuf>,
    pub password: bool,
    pub public_key: bool,
    pub keyboard_interactive: bool,
    pub batch: bool,
    pub attempts: usize,
    pub timeout: Duration,
    pub keepalive: Option<Duration>,
    pub keepalive_max: usize,
    pub auth_order: Vec<String>,
    pub identities_only: bool,
    pub agent: Option<String>,
}

pub fn system_requested(args: &[OsString]) -> bool {
    args.first().is_some_and(|a| a == "--system")
        || args
            .iter()
            .any(|a| matches!(a.to_str(), Some("-V" | "-G" | "-Q")))
}

const HELP: &str = "Use ssh --system <arguments> for system OpenSSH (password-only image upload is unavailable there)";

// Only options that this backend actually implements are accepted. In particular,
// silently ignoring a proxy or host-key policy could connect to the wrong host.
pub fn validate_args(args: &[OsString]) -> Result<()> {
    let mut i = 0;
    let mut destination = false;
    while i < args.len() {
        let arg = args[i].to_str().context("SSH arguments must be UTF-8")?;
        if arg == "--" {
            i += 1;
            ensure!(i + 1 == args.len(), "Expected one SSH destination. {HELP}");
            destination = true;
            break;
        }
        if !arg.starts_with('-') {
            ensure!(
                i + 1 == args.len(),
                "Remote commands are not supported by built-in SSH yet. {HELP}"
            );
            destination = true;
            break;
        }
        ensure!(arg.len() > 1, "Missing SSH destination");
        let flags = &arg[1..];
        for (offset, flag) in flags.char_indices() {
            if "pliFoJ".contains(flag) {
                let value = if offset + 1 < flags.len() {
                    &flags[offset + 1..]
                } else {
                    i += 1;
                    args.get(i)
                        .and_then(|s| s.to_str())
                        .context("Missing SSH option value")?
                };
                if flag == 'o' {
                    let key = value
                        .split(['=', ' ', '\t'])
                        .next()
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    ensure!(
                        [
                            "hostname",
                            "user",
                            "port",
                            "identityfile",
                            "userknownhostsfile",
                            "globalknownhostsfile",
                            "stricthostkeychecking",
                            "hostkeyalias",
                            "batchmode",
                            "passwordauthentication",
                            "pubkeyauthentication",
                            "kbdinteractiveauthentication",
                            "challengeresponseauthentication",
                            "preferredauthentications",
                            "numberofpasswordprompts",
                            "connecttimeout",
                            "serveraliveinterval",
                            "serveralivecountmax",
                            "identitiesonly",
                            "identityagent",
                            "proxyjump",
                        ]
                        .contains(&key.as_str()),
                        "Unsupported SSH option {key}. {HELP}"
                    );
                }
                break;
            }
            ensure!(flag == 't', "Unsupported SSH flag -{flag}. {HELP}");
        }
        i += 1;
    }
    ensure!(destination, "Missing SSH destination");
    Ok(())
}

pub fn resolve(ssh: &Path, args: &[OsString]) -> Result<Config> {
    validate_args(args)?;
    let mut command = Command::new(ssh);
    command
        .arg("-G")
        .args(args)
        .env_remove("YITERM_REAL_SSH")
        .env_remove("YITERM_SSH_DIRECTORY");
    // Config resolution can run Match exec; retain OpenSSH's own semantics.
    let output = command
        .output()
        .context("Could not resolve SSH config using ssh -G")?;
    ensure!(
        output.status.success(),
        "SSH config resolution failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Config::parse(&String::from_utf8(output.stdout)?)
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let mut values: HashMap<String, Vec<String>> = HashMap::new();
        for line in text.lines() {
            if let Some((key, value)) = line.split_once(char::is_whitespace) {
                values
                    .entry(key.to_ascii_lowercase())
                    .or_default()
                    .push(value.trim().to_owned());
            }
        }
        let get = |key: &str, default: &str| {
            values
                .get(key)
                .and_then(|v| v.first())
                .cloned()
                .unwrap_or_else(|| default.into())
        };
        for key in [
            "proxycommand",
            "localforward",
            "remoteforward",
            "dynamicforward",
            "remotecommand",
            "certificatefile",
            "revokedhostkeys",
            "knownhostscommand",
        ] {
            ensure!(
                !values
                    .get(key)
                    .is_some_and(|v| v.iter().any(|s| s != "none" && !s.is_empty())),
                "SSH config uses unsupported {key}. {HELP}"
            );
        }
        for key in [
            "forwardagent",
            "forwardx11",
            "permitlocalcommand",
            "gssapiauthentication",
            "hostbasedauthentication",
            "verifyhostkeydns",
        ] {
            ensure!(
                matches!(get(key, "no").as_str(), "no" | "false"),
                "SSH config enables unsupported {key}. {HELP}"
            );
        }
        ensure!(
            get("sessiontype", "default") == "default",
            "Unsupported SSH SessionType. {HELP}"
        );
        let home = std::env::home_dir().context("Cannot locate home directory")?;
        let paths = |key: &str| -> Result<Vec<PathBuf>> {
            let mut result = Vec::new();
            if let Some(entries) = values.get(key) {
                for entry in entries {
                    // IdentityFile occupies a whole line. OpenSSH loses quotes
                    // when dumping known-host lists, so reject ambiguous paths
                    // rather than trusting/writing a different known_hosts file.
                    let entries = if key == "identityfile" {
                        vec![entry.clone()]
                    } else {
                        path_words(entry)?
                    };
                    for entry in entries {
                        if entry == "none" {
                            continue;
                        }
                        let path = entry.trim_matches('"');
                        ensure!(
                            !path.contains('%') && !path.contains("${"),
                            "Unexpanded path tokens in {key}. {HELP}"
                        );
                        result.push(
                            if let Some(tail) =
                                path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"))
                            {
                                home.join(tail)
                            } else {
                                PathBuf::from(path)
                            },
                        );
                    }
                }
            }
            Ok(result)
        };
        let mut known_hosts = paths("userknownhostsfile")?;
        let save_host_key = known_hosts
            .first()
            .cloned()
            .context("Built-in SSH requires a UserKnownHostsFile")?;
        known_hosts.extend(paths("globalknownhostsfile")?);
        let host = get("hostname", "");
        let user = get("user", "");
        ensure!(
            !host.is_empty() && !user.is_empty(),
            "SSH config is missing hostname or user"
        );
        let host_key_name = get("hostkeyalias", &host);
        let strict_host_key = get("stricthostkeychecking", "ask");
        ensure!(
            ["ask", "true", "yes", "accept-new"].contains(&strict_host_key.as_str()),
            "Built-in SSH requires StrictHostKeyChecking=ask, yes or accept-new. {HELP}"
        );
        let timeout = match get("connecttimeout", "none").as_str() {
            "none" | "0" => 15,
            n => n.parse::<u64>().context("Invalid ConnectTimeout")?,
        };
        let interval: u64 = get("serveraliveinterval", "0").parse()?;
        let agent = get("identityagent", "SSH_AUTH_SOCK");
        let proxy_jump = get("proxyjump", "none");
        ensure!(
            proxy_jump == "none"
                || proxy_jump.is_empty()
                || proxy_jump.split(',').all(|hop| !hop.trim().is_empty()),
            "Invalid ProxyJump configuration"
        );
        Ok(Self {
            host,
            user,
            proxy_jump: (proxy_jump != "none" && !proxy_jump.is_empty()).then_some(proxy_jump),
            host_key_name,
            known_hosts,
            save_host_key,
            strict_host_key,
            port: get("port", "22").parse()?,
            identities: paths("identityfile")?,
            password: get("passwordauthentication", "yes") == "yes",
            public_key: get("pubkeyauthentication", "yes") != "no",
            keyboard_interactive: get("kbdinteractiveauthentication", "yes") == "yes",
            batch: get("batchmode", "no") == "yes",
            attempts: get("numberofpasswordprompts", "3")
                .parse::<usize>()?
                .min(10),
            timeout: Duration::from_secs(timeout),
            keepalive: (interval > 0).then(|| Duration::from_secs(interval)),
            keepalive_max: get("serveralivecountmax", "3").parse()?,
            auth_order: get(
                "preferredauthentications",
                "publickey,keyboard-interactive,password",
            )
            .split(',')
            .map(str::to_owned)
            .collect(),
            identities_only: get("identitiesonly", "no") == "yes",
            agent: (agent != "none").then_some(agent),
        })
    }
}

fn path_words(text: &str) -> Result<Vec<String>> {
    if !text.contains('"') {
        let words: Vec<_> = text.split_whitespace().collect();
        let rooted = |path: &str| {
            path.starts_with('/')
                || path.starts_with('~')
                || path.starts_with("\\\\")
                || (path.as_bytes().get(1) == Some(&b':')
                    && path
                        .as_bytes()
                        .get(2)
                        .is_some_and(|b| matches!(b, b'/' | b'\\')))
        };
        ensure!(
            words.len() <= 1 || words.iter().all(|word| rooted(word)),
            "Ambiguous known_hosts paths in ssh -G output; use paths without spaces. {HELP}"
        );
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            _ => word.push(c),
        }
    }
    if quoted {
        bail!("Unclosed quote in SSH config path");
    }
    if !word.is_empty() {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    const BASE: &str = "hostname 127.0.0.1\nuser alice\nport 2222\nuserknownhostsfile \"C:\\Users\\A B\\.ssh\\known_hosts\"\nidentityfile C:\\Users\\A B\\.ssh\\id_ed25519\n";
    #[test]
    fn resolved_paths_and_connection_are_preserved() {
        let config = Config::parse(BASE).unwrap();
        assert_eq!(config.port, 2222);
        assert_eq!(
            config.save_host_key,
            PathBuf::from("C:\\Users\\A B\\.ssh\\known_hosts")
        );
        assert_eq!(
            config.identities[0],
            PathBuf::from("C:\\Users\\A B\\.ssh\\id_ed25519")
        );
        assert_eq!(config.host_key_name, "127.0.0.1");
    }
    #[test]
    fn unsupported_routes_and_security_settings_fail_closed() {
        for setting in [
            "proxycommand ssh -W %h:%p gateway",
            "forwardagent yes",
            "stricthostkeychecking no",
            "remotecommand tmux",
            "revokedhostkeys revoked",
        ] {
            assert!(
                Config::parse(&format!("{BASE}{setting}\n")).is_err(),
                "{setting}"
            );
        }
        for args in [
            vec!["-L8080:host:80", "host"],
            vec!["host", "ls"],
            vec!["-oStrictHostKeyChecking=no", "host", "ls"],
        ] {
            assert!(
                validate_args(&args.into_iter().map(OsString::from).collect::<Vec<_>>()).is_err()
            );
        }
        validate_args(&["-tt", "-p2222", "-i", "C:\\key path", "host"].map(OsString::from))
            .unwrap();
        validate_args(&["-Jbastion", "host"].map(OsString::from)).unwrap();
        validate_args(&["-oProxyJump=bastion", "host"].map(OsString::from)).unwrap();
    }

    #[test]
    fn proxy_jump_is_resolved_and_preserved() {
        let config = Config::parse(&format!("{BASE}proxyjump bastion,jump2\n")).unwrap();
        assert_eq!(config.proxy_jump.as_deref(), Some("bastion,jump2"));
        let config = Config::parse(&format!("{BASE}proxyjump none\n")).unwrap();
        assert_eq!(config.proxy_jump, None);
    }
}
