#[path = "common/native_server.rs"]
mod native_server;
use native_server::Fixture;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use yiterm::native_ssh::{self, auth::Prompt, transfer};

struct Password(AtomicUsize);
impl Prompt for Password {
    fn ask(&self, _: &str, secret: bool) -> anyhow::Result<String> {
        assert!(secret, "The fixture host key is already pinned");
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("fixture-password".into())
    }
}
fn image() -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    // Span multiple SFTP writes and channel windows.
    bytes.extend((0..600_000).map(|i| (i % 251) as u8));
    bytes
}
async fn upload(
    handle: Arc<russh::client::Handle<native_ssh::auth::Handler>>,
    bytes: &[u8],
) -> anyhow::Result<String> {
    let (mut client, mut server) = tokio::io::duplex(32768);
    let worker = tokio::spawn(async move { transfer::serve_one(&mut server, &handle).await });
    let result = transfer::request(&mut client, bytes).await;
    let _ = worker.await.unwrap();
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn password_once_shell_and_repeated_uploads_share_one_connection() {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let fixture = Fixture::new().await;
        let prompt = Arc::new(Password(AtomicUsize::new(0)));
        let handle = Arc::new(
            native_ssh::connect(fixture.config.clone(), prompt.clone())
                .await
                .unwrap(),
        );
        let mut shell = native_ssh::shell(&handle, 100, 40).await.unwrap();
        assert!(matches!(
            shell.wait().await,
            Some(russh::ChannelMsg::Data { .. })
        ));
        let bytes = image();
        let first = upload(handle.clone(), &bytes).await.unwrap();
        let second = upload(handle.clone(), &bytes).await.unwrap();
        assert_ne!(first, second);
        assert_eq!(
            fixture.state.images.lock().unwrap()[first.trim_matches('\'')],
            bytes
        );
        assert_eq!(
            fixture.state.images.lock().unwrap()[second.trim_matches('\'')],
            bytes
        );
        assert_eq!(fixture.state.connections.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.state.authentications.load(Ordering::SeqCst), 1);
        assert_eq!(prompt.0.load(Ordering::SeqCst), 1);
        shell.window_change(120, 50, 0, 0).await.unwrap();
        shell.data(&b"\x03terminal-still-works"[..]).await.unwrap();
        match shell.wait().await {
            Some(russh::ChannelMsg::Data { data }) => {
                assert_eq!(&data[..], b"\x03terminal-still-works")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(*fixture.state.size.lock().unwrap(), (120, 50));
        shell.data(&b"exit\r"[..]).await.unwrap();
        assert!(matches!(
            shell.wait().await,
            Some(russh::ChannelMsg::ExitStatus { exit_status: 7 })
        ));
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_final_close_cleans_image_and_does_not_break_session() {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let fixture = Fixture::new().await;
        let handle = Arc::new(
            native_ssh::connect(
                fixture.config.clone(),
                Arc::new(Password(AtomicUsize::new(0))),
            )
            .await
            .unwrap(),
        );
        fixture.state.fail_close.store(true, Ordering::SeqCst);
        assert!(upload(handle.clone(), &image()).await.is_err());
        assert!(fixture.state.images.lock().unwrap().is_empty());
        fixture.state.fail_close.store(false, Ordering::SeqCst);
        assert!(upload(handle, &image()).await.is_ok());
        assert_eq!(fixture.state.authentications.load(Ordering::SeqCst), 1);
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separate_sessions_never_share_upload_destinations() {
    let first = Fixture::new().await;
    let second = Fixture::new().await;
    let a = Arc::new(
        native_ssh::connect(
            first.config.clone(),
            Arc::new(Password(AtomicUsize::new(0))),
        )
        .await
        .unwrap(),
    );
    let b = Arc::new(
        native_ssh::connect(
            second.config.clone(),
            Arc::new(Password(AtomicUsize::new(0))),
        )
        .await
        .unwrap(),
    );
    let bytes = image();
    let (a_path, b_path) = tokio::join!(upload(a, &bytes), upload(b, &bytes));
    assert!(
        !first
            .state
            .images
            .lock()
            .unwrap()
            .contains_key(b_path.unwrap().trim_matches('\''))
    );
    assert!(
        !second
            .state
            .images
            .lock()
            .unwrap()
            .contains_key(a_path.unwrap().trim_matches('\''))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyboard_interactive_login_also_reuses_authentication_for_upload() {
    let mut fixture = Fixture::new().await;
    fixture.state.interactive.store(true, Ordering::SeqCst);
    fixture.config.password = false;
    fixture.config.keyboard_interactive = true;
    fixture.config.auth_order = vec!["keyboard-interactive".into()];
    let prompt = Arc::new(Password(AtomicUsize::new(0)));
    let handle = Arc::new(
        native_ssh::connect(fixture.config.clone(), prompt.clone())
            .await
            .unwrap(),
    );
    upload(handle, &image()).await.unwrap();
    assert_eq!(fixture.state.authentications.load(Ordering::SeqCst), 1);
    assert_eq!(prompt.0.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_private_key_works_without_password_prompts() {
    let mut fixture = Fixture::new().await;
    let key = russh::keys::PrivateKey::random(
        &mut russh::keys::key::safe_rng(),
        russh::keys::Algorithm::Ed25519,
    )
    .unwrap();
    *fixture.state.allowed_key.lock().unwrap() = Some(key.public_key().clone());
    let path = fixture.directory.path().join("id_ed25519");
    key.write_openssh_file(&path, russh::keys::ssh_key::LineEnding::LF)
        .unwrap();
    fixture.config.identities = vec![path];
    fixture.config.public_key = true;
    fixture.config.password = false;
    fixture.config.auth_order = vec!["publickey".into()];
    let prompt = Arc::new(Password(AtomicUsize::new(0)));
    let handle = Arc::new(
        native_ssh::connect(fixture.config.clone(), prompt.clone())
            .await
            .unwrap(),
    );
    upload(handle, &image()).await.unwrap();
    assert_eq!(prompt.0.load(Ordering::SeqCst), 0);
}
