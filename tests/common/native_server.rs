#![allow(dead_code)]
use anyhow::Result;
use russh::{
    Channel, ChannelId, keys,
    server::{self, Auth, Msg, Session},
};
use russh_sftp::protocol::{FileAttributes, Handle, OpenFlags, Status, StatusCode};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use yiterm::native_ssh::config::Config;

#[derive(Default)]
pub struct State {
    pub connections: AtomicUsize,
    pub authentications: AtomicUsize,
    pub images: Mutex<HashMap<String, Vec<u8>>>,
    pub size: Mutex<(u32, u32)>,
    pub fail_close: AtomicBool,
    pub stall_write: AtomicBool,
    pub interactive: AtomicBool,
    pub allowed_key: Mutex<Option<keys::PublicKey>>,
}
pub struct Fixture {
    pub state: Arc<State>,
    pub config: Config,
    pub directory: tempfile::TempDir,
    task: tokio::task::JoinHandle<()>,
}
impl Fixture {
    pub async fn new() -> Self {
        let state = Arc::new(State::default());
        let directory = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let key =
            keys::PrivateKey::random(&mut keys::key::safe_rng(), keys::Algorithm::Ed25519).unwrap();
        let hosts = directory.path().join("known_hosts");
        keys::known_hosts::learn_known_hosts_path("127.0.0.1", port, key.public_key(), &hosts)
            .unwrap();
        let config = Config::parse(&format!("hostname 127.0.0.1\nuser alice\nport {port}\nuserknownhostsfile \"{}\"\nstricthostkeychecking yes\npubkeyauthentication no\nkbdinteractiveauthentication no\npreferredauthentications password\n", hosts.display())).unwrap();
        let options = Arc::new(server::Config {
            keys: vec![key],
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        let shared = state.clone();
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                shared.connections.fetch_add(1, Ordering::SeqCst);
                let handler = Handler {
                    state: shared.clone(),
                    channels: HashMap::new(),
                    shell: None,
                };
                let options = options.clone();
                tokio::spawn(async move {
                    if let Ok(session) = server::run_stream(options, socket, handler).await {
                        let _ = session.await;
                    }
                });
            }
        });
        Self {
            state,
            config,
            directory,
            task,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
struct Handler {
    state: Arc<State>,
    channels: HashMap<ChannelId, Channel<Msg>>,
    shell: Option<ChannelId>,
}
impl server::Handler for Handler {
    type Error = anyhow::Error;
    async fn auth_publickey(&mut self, user: &str, key: &keys::PublicKey) -> Result<Auth> {
        self.state.authentications.fetch_add(1, Ordering::SeqCst);
        Ok(
            if user == "alice"
                && self
                    .state
                    .allowed_key
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|allowed| allowed.key_data() == key.key_data())
            {
                Auth::Accept
            } else {
                Auth::reject()
            },
        )
    }
    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _: &str,
        response: Option<server::Response<'a>>,
    ) -> Result<Auth> {
        if !self.state.interactive.load(Ordering::SeqCst) {
            return Ok(Auth::reject());
        }
        if let Some(mut response) = response {
            self.state.authentications.fetch_add(1, Ordering::SeqCst);
            return Ok(
                if user == "alice"
                    && response
                        .next()
                        .is_some_and(|bytes| &bytes[..] == b"fixture-password")
                {
                    Auth::Accept
                } else {
                    Auth::reject()
                },
            );
        }
        Ok(Auth::Partial {
            name: "Fixture".into(),
            instructions: "".into(),
            prompts: vec![("Password: ".into(), false)].into(),
        })
    }
    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth> {
        self.state.authentications.fetch_add(1, Ordering::SeqCst);
        Ok(if user == "alice" && password == "fixture-password" {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }
    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<()> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }
    async fn pty_request(
        &mut self,
        id: ChannelId,
        _: &str,
        cols: u32,
        rows: u32,
        _: u32,
        _: u32,
        _: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<()> {
        *self.state.size.lock().unwrap() = (cols, rows);
        session.channel_success(id)?;
        Ok(())
    }
    async fn window_change_request(
        &mut self,
        _: ChannelId,
        cols: u32,
        rows: u32,
        _: u32,
        _: u32,
        _: &mut Session,
    ) -> Result<()> {
        *self.state.size.lock().unwrap() = (cols, rows);
        Ok(())
    }
    async fn shell_request(&mut self, id: ChannelId, session: &mut Session) -> Result<()> {
        self.shell = Some(id);
        session.data(id, &b"fixture-shell-ready\r\n"[..])?;
        session.channel_success(id)?;
        Ok(())
    }
    async fn data(&mut self, id: ChannelId, data: &[u8], session: &mut Session) -> Result<()> {
        if self.shell != Some(id) {
            return Ok(());
        }
        if data.starts_with(b"exit") {
            session.exit_status_request(id, 7)?;
            session.eof(id)?;
            session.close(id)?;
        } else {
            session.data(id, data.to_vec())?;
        }
        Ok(())
    }
    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<()> {
        if name != "sftp" {
            session.channel_failure(id)?;
            return Ok(());
        }
        session.channel_success(id)?;
        let channel = self.channels.remove(&id).unwrap();
        let state = self.state.clone();
        tokio::spawn(async move {
            russh_sftp::server::run(channel.into_stream(), Sftp { state }).await;
        });
        Ok(())
    }
}
struct Sftp {
    state: Arc<State>,
}
fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: String::new(),
        language_tag: "en".into(),
    }
}
impl russh_sftp::server::Handler for Sftp {
    type Error = StatusCode;
    fn unimplemented(&self) -> StatusCode {
        StatusCode::OpUnsupported
    }
    async fn open(
        &mut self,
        id: u32,
        filename: String,
        flags: OpenFlags,
        attributes: FileAttributes,
    ) -> Result<Handle, StatusCode> {
        assert!(filename.starts_with("/tmp/yiterm-drop-"));
        assert!(flags.contains(OpenFlags::EXCLUDE | OpenFlags::CREATE | OpenFlags::WRITE));
        assert_eq!(attributes.permissions, Some(0o600));
        let mut images = self.state.images.lock().unwrap();
        if images.contains_key(&filename) {
            return Err(StatusCode::Failure);
        }
        images.insert(filename.clone(), Vec::new());
        Ok(Handle {
            id,
            handle: filename,
        })
    }
    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, StatusCode> {
        while self.state.stall_write.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut images = self.state.images.lock().unwrap();
        let image = images.get_mut(&handle).ok_or(StatusCode::NoSuchFile)?;
        let end = offset as usize + data.len();
        image.resize(image.len().max(end), 0);
        image[offset as usize..end].copy_from_slice(&data);
        Ok(ok(id))
    }
    async fn close(&mut self, id: u32, _: String) -> Result<Status, StatusCode> {
        if self.state.fail_close.load(Ordering::SeqCst) {
            return Err(StatusCode::Failure);
        }
        Ok(ok(id))
    }
    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, StatusCode> {
        self.state.images.lock().unwrap().remove(&filename);
        Ok(ok(id))
    }
}
