use super::config::Config;
use anyhow::{Context, Result, bail, ensure};
use russh::{
    client,
    keys::{
        self, HashAlg, PublicKey, PublicKeyOrCertificate,
        ssh_key::known_hosts::{HostPatterns, KnownHosts, Marker},
    },
};
use std::sync::Arc;
use zeroize::Zeroizing;

/// Implementations read from the wrapper's console, never from terminal logs.
pub trait Prompt: Send + Sync {
    fn ask(&self, message: &str, secret: bool) -> Result<String>;
}

pub async fn ask(
    prompt: &Arc<dyn Prompt>,
    message: String,
    secret: bool,
) -> Result<Zeroizing<String>> {
    let prompt = prompt.clone();
    tokio::task::spawn_blocking(move || prompt.ask(&message, secret).map(Zeroizing::new)).await?
}

pub struct Handler {
    pub config: Config,
    pub prompt: Arc<dyn Prompt>,
    pub(crate) _proxy_process: Option<tokio::process::Child>,
}
impl client::Handler for Handler {
    type Error = anyhow::Error;
    async fn check_server_key(&mut self, presented: &PublicKeyOrCertificate) -> Result<bool> {
        ensure!(
            presented.certificate().is_none(),
            "Host certificates require ssh --system"
        );
        let key = presented.public_key();
        if verify_known_host(&self.config, &key)? {
            return Ok(true);
        }
        ensure!(
            !self.config.batch && !matches!(self.config.strict_host_key.as_str(), "yes" | "true"),
            "Unknown SSH host key; connect interactively to verify it first"
        );
        if self.config.strict_host_key != "accept-new" {
            let answer = ask(&self.prompt, format!(
                "The authenticity of host '{}:{}' is unknown.\r\n{} key fingerprint: {}\r\nTrust this host (yes/no)? ",
                self.config.host_key_name, self.config.port, key.algorithm(), key.fingerprint(HashAlg::Sha256)
            ), false).await?;
            ensure!(answer.trim() == "yes", "Host key was not accepted");
        }
        keys::known_hosts::learn_known_hosts_path(
            &self.config.host_key_name,
            self.config.port,
            &key,
            &self.config.save_host_key,
        )?;
        Ok(true)
    }
}

pub fn verify_known_host(config: &Config, key: &PublicKey) -> Result<bool> {
    let host = if config.port == 22 {
        config.host_key_name.clone()
    } else {
        format!("[{}]:{}", config.host_key_name, config.port)
    };
    let mut matched = false;
    let mut changed = false;
    for path in &config.known_hosts {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("Cannot read {}", path.display())),
        };
        for entry in KnownHosts::new(&text) {
            let entry = entry
                .with_context(|| format!("Invalid known_hosts entry in {}", path.display()))?;
            if !host_matches(&host, entry.host_patterns()) {
                continue;
            }
            let same = entry.public_key().key_data() == key.key_data();
            match entry.marker() {
                Some(Marker::Revoked) => ensure!(!same, "SSH host key is revoked"),
                Some(Marker::CertAuthority) => {}
                None => {
                    matched |= same;
                    changed |= entry.public_key().algorithm() == key.algorithm() && !same;
                }
            }
        }
    }
    ensure!(
        matched || !changed,
        "SSH HOST KEY CHANGED; verify the server before updating known_hosts"
    );
    Ok(matched)
}

fn host_matches(host: &str, patterns: &HostPatterns) -> bool {
    match patterns {
        HostPatterns::HashedName { salt, hash } => {
            use hmac::{Hmac, KeyInit, Mac};
            Hmac::<sha1::Sha1>::new_from_slice(salt)
                .is_ok_and(|mac| mac.chain_update(host.as_bytes()).verify_slice(hash).is_ok())
        }
        HostPatterns::Patterns(patterns) => {
            let mut matched = false;
            for pattern in patterns {
                if let Some(negative) = pattern.strip_prefix('!') {
                    if glob(negative.as_bytes(), host.as_bytes()) {
                        return false;
                    }
                } else {
                    matched |= glob(pattern.as_bytes(), host.as_bytes());
                }
            }
            matched
        }
    }
}

// OpenSSH host patterns use only '*' and '?'; brackets in IPv6/ports are literal.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t, mut star, mut retry) = (0, 0, None, 0);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p].eq_ignore_ascii_case(&text[t])) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = t;
        } else if let Some(s) = star {
            retry += 1;
            t = retry;
            p = s + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

pub async fn authenticate(
    handle: &mut client::Handle<Handler>,
    config: &Config,
    prompt: &Arc<dyn Prompt>,
) -> Result<()> {
    let mut result = handle.authenticate_none(&config.user).await?;
    if result.success() {
        return Ok(());
    }
    for method in &config.auth_order {
        let available = |result: &russh::client::AuthResult, name: &str| match result {
            russh::client::AuthResult::Success => true,
            russh::client::AuthResult::Failure {
                remaining_methods, ..
            } => remaining_methods.iter().any(|m| <&str>::from(m) == name),
        };
        if !available(&result, method) {
            continue;
        }
        match method.as_str() {
            "publickey" if config.public_key => {
                // Try explicit/default key files before an agent's unrelated identities.
                for path in &config.identities {
                    if !path.is_file() {
                        continue;
                    }
                    let encoded = Zeroizing::new(
                        std::fs::read_to_string(path)
                            .with_context(|| format!("Cannot read key {}", path.display()))?,
                    );
                    let key = keys::PrivateKey::from_openssh(encoded.as_bytes())
                        .with_context(|| format!("Cannot parse key {}", path.display()))?;
                    let key = if key.is_encrypted() {
                        if config.batch {
                            continue;
                        }
                        let passphrase = ask(
                            prompt,
                            format!("Passphrase for key '{}': ", path.display()),
                            true,
                        )
                        .await?;
                        key.decrypt(passphrase.as_bytes())
                            .context("Invalid key passphrase")?
                    } else {
                        key
                    };
                    let hash = handle.best_supported_rsa_hash().await?.flatten();
                    result = handle
                        .authenticate_publickey(
                            &config.user,
                            keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                        )
                        .await?;
                    if result.success() {
                        return Ok(());
                    }
                    if !available(&result, "publickey") {
                        break;
                    }
                }
                if available(&result, "publickey")
                    && !config.identities_only
                    && config.agent.is_some()
                    && let Some(mut agent) = connect_agent(config).await
                    && let Ok(identities) = agent.request_identities().await
                {
                    for identity in identities.into_iter().take(16) {
                        if let keys::agent::AgentIdentity::PublicKey { key, .. } = identity {
                            let hash = handle.best_supported_rsa_hash().await?.flatten();
                            result = handle
                                .authenticate_publickey_with(&config.user, key, hash, &mut agent)
                                .await?;
                            if result.success() {
                                return Ok(());
                            }
                            if !available(&result, "publickey") {
                                break;
                            }
                        }
                    }
                }
            }
            "password" if config.password && !config.batch => {
                for _ in 0..config.attempts {
                    let password = ask(
                        prompt,
                        format!("{}@{} password: ", config.user, config.host),
                        true,
                    )
                    .await?;
                    result = handle
                        .authenticate_password(&config.user, password.as_str())
                        .await?;
                    if result.success() {
                        return Ok(());
                    }
                    if !available(&result, "password") {
                        break;
                    }
                }
            }
            "keyboard-interactive" if config.keyboard_interactive && !config.batch => {
                for _ in 0..config.attempts {
                    let mut response = handle
                        .authenticate_keyboard_interactive_start(&config.user, None::<String>)
                        .await?;
                    // Bound malicious/broken servers asking indefinitely.
                    for _ in 0..32 {
                        match response {
                            client::KeyboardInteractiveAuthResponse::Success => return Ok(()),
                            client::KeyboardInteractiveAuthResponse::Failure {
                                remaining_methods,
                                partial_success,
                            } => {
                                result = client::AuthResult::Failure {
                                    remaining_methods,
                                    partial_success,
                                };
                                break;
                            }
                            client::KeyboardInteractiveAuthResponse::InfoRequest {
                                name,
                                instructions,
                                prompts,
                            } => {
                                ensure!(prompts.len() <= 16, "Too many SSH authentication prompts");
                                let mut responses = Vec::new();
                                for (index, p) in prompts.into_iter().enumerate() {
                                    let message = if index == 0 {
                                        format!("{name}\r\n{instructions}\r\n{}", p.prompt)
                                    } else {
                                        p.prompt
                                    };
                                    responses.push(ask(prompt, message, !p.echo).await?);
                                }
                                response = handle
                                    .authenticate_keyboard_interactive_respond(
                                        responses.iter().map(|s| s.to_string()).collect(),
                                    )
                                    .await?;
                            }
                        }
                    }
                    if !available(&result, "keyboard-interactive") {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    bail!("SSH authentication failed")
}

type Agent =
    keys::agent::client::AgentClient<Box<dyn keys::agent::client::AgentStream + Send + Unpin>>;
async fn connect_agent(config: &Config) -> Option<Agent> {
    let requested = config.agent.as_deref()?;
    let path = if requested == "SSH_AUTH_SOCK" {
        std::env::var("SSH_AUTH_SOCK")
            .ok()
            .or_else(|| cfg!(windows).then(|| r"\\.\pipe\openssh-ssh-agent".into()))?
    } else {
        requested.to_owned()
    };
    #[cfg(windows)]
    let connect = keys::agent::client::AgentClient::connect_named_pipe(path);
    #[cfg(unix)]
    let connect = keys::agent::client::AgentClient::connect_uds(path);
    tokio::time::timeout(std::time::Duration::from_secs(2), connect)
        .await
        .ok()?
        .ok()
        .map(|agent| agent.dynamic())
}
