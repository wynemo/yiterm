//! A native helper owns the Tailcat engine; closing its stdin revokes the code.
mod links;

pub fn current_link(port: Option<u16>) -> Result<String> { links::current(port) }

use super::*;
use std::{
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
    sync::Mutex,
};

#[derive(serde::Deserialize)]
struct Message {
    url: String,
    rows: Vec<String>,
}
pub struct Invitation {
    pub url: String,
    pub pixels: image::RgbaImage,
    #[cfg(not(target_os = "linux"))]
    pub render: Arc<gpui::RenderImage>,
    #[cfg(target_os = "linux")]
    texture: std::sync::OnceLock<eframe::egui::TextureHandle>,
}
impl Invitation {
    fn parse(message: Message) -> Result<Self> {
        let size = message.rows.len();
        ensure!(
            (21..=200).contains(&size)
                && message
                    .rows
                    .iter()
                    .all(|r| r.len() == size && r.bytes().all(|b| b == b'0' || b == b'1')),
            "Invalid sharing code"
        );
        let url = url::Url::parse(&message.url)?;
        ensure!(
            url.scheme() == "tenimon"
                && url.host_str() == Some("yiterm")
                && url.path() == "/connect",
            "Invalid sharing link"
        );
        // Integer scaling keeps every QR module sharp, including its quiet zone.
        let scale = 4;
        let pixels = image::RgbaImage::from_fn(size as u32 * scale, size as u32 * scale, |x, y| {
            let value =
                if message.rows[(y / scale) as usize].as_bytes()[(x / scale) as usize] == b'1' {
                    0
                } else {
                    255
                };
            image::Rgba([value, value, value, 255])
        });
        Ok(Self {
            url: message.url,
            #[cfg(not(target_os = "linux"))]
            render: Arc::new(gpui::RenderImage::new(vec![image::Frame::new(
                pixels.clone(),
            )])),
            pixels,
            #[cfg(target_os = "linux")]
            texture: Default::default(),
        })
    }
    #[cfg(target_os = "linux")]
    pub fn texture(&self, ctx: &eframe::egui::Context) -> eframe::egui::TextureId {
        self.texture
            .get_or_init(|| {
                ctx.load_texture(
                    "yiterm-pairing",
                    eframe::egui::ColorImage::from_rgba_unmultiplied(
                        [self.pixels.width() as usize, self.pixels.height() as usize],
                        self.pixels.as_raw(),
                    ),
                    eframe::egui::TextureOptions::NEAREST,
                )
            })
            .id()
    }
}
#[derive(Clone)]
pub enum Status {
    Starting,
    Ready(Arc<Invitation>),
    Failed,
}
#[derive(Default)]
struct Export { closed: bool, link: Option<links::PublishedLink> }

pub struct Pairing {
    child: Option<Child>,
    state: Arc<Mutex<Status>>,
    export: Arc<Mutex<Export>>,
    copied: bool,
}
impl Pairing {
    pub fn start(port: u16) -> Self {
        let state = Arc::new(Mutex::new(Status::Starting));
        let export = Arc::new(Mutex::new(Export::default()));
        let mut result = Self {
            child: None,
            state: state.clone(),
            export: export.clone(),
            copied: false,
        };
        let start = || -> Result<Child> {
            let executable = std::env::current_exe()?;
            let name = if cfg!(windows) {
                "yiterm-tailcat.exe"
            } else {
                "yiterm-tailcat"
            };
            let helper = executable
                .parent()
                .context("Cannot locate YiTerm")?
                .join(name);
            let mut command = Command::new(helper);
            command
                .args(["--port", &port.to_string()])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000);
            }
            Ok(command.spawn()?)
        };
        match start() {
            Ok(mut child) => {
                let stdout = child.stdout.take().expect("piped stdout");
                result.child = Some(child);
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stdout);
                    let mut line = String::new();
                    let parsed = (&mut reader)
                        .take(65536)
                        .read_line(&mut line)
                        .map_err(anyhow::Error::from)
                        .and_then(|_| serde_json::from_str::<Message>(&line).map_err(Into::into))
                        .and_then(Invitation::parse);
                    *state.lock().unwrap() = match parsed {
                        Ok(invitation) => {
                            let mut published = export.lock().unwrap();
                            if !published.closed {
                                match links::PublishedLink::new(port, &invitation.url) {
                                    Ok(link) => published.link = Some(link),
                                    Err(_) => tracing::warn!("Cannot publish active sharing link for CLI access"),
                                }
                            }
                            Status::Ready(Arc::new(invitation))
                        },
                        Err(_) => Status::Failed,
                    };
                    super::super::wake_ui();
                    // Observe helper exit without holding the process lock.
                    let _ = std::io::copy(&mut reader, &mut std::io::sink());
                    export.lock().unwrap().link = None;
                    *state.lock().unwrap() = Status::Failed;
                    super::super::wake_ui();
                });
            }
            Err(_) => *state.lock().unwrap() = Status::Failed,
        }
        result
    }
    pub fn copied(&self) -> bool { self.copied }
    pub fn mark_copied(&mut self) { self.copied = true; }
    pub fn status(&self) -> Status {
        self.state.lock().unwrap().clone()
    }
}
impl Drop for Pairing {
    fn drop(&mut self) {
        {
            let mut export = self.export.lock().unwrap();
            export.closed = true;
            export.link = None;
        }
        if let Some(mut child) = self.child.take() {
            // EOF triggers graceful engine shutdown. A watchdog also reaps a stuck helper.
            drop(child.stdin.take());
            std::thread::spawn(move || {
                for _ in 0..30 {
                    if child.try_wait().ok().flatten().is_some() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let _ = child.kill();
                let _ = child.wait();
            });
        }
    }
}
