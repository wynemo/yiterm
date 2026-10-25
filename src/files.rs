//! Bounded, on-demand local file browsing. No filesystem work runs during rendering.
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
const DIRECTORY_LIMIT: usize = 10_000;
const CACHE_LIMIT: usize = 20_000;
const DIRECTORY_CACHE_LIMIT: usize = 256;
const PREVIEW_LINE_LIMIT: usize = 50_000;
const PREVIEW_LIMIT: u64 = 1024 * 1024;
#[derive(Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub directory: bool,
    pub regular: bool,
}
#[derive(Clone)]
pub struct Row {
    pub entry: Entry,
    pub depth: usize,
}
pub struct Directory {
    pub entries: Vec<Entry>,
    pub notice: Option<String>,
}
pub struct Preview {
    pub path: PathBuf,
    pub lines: Vec<String>,
    pub notice: Option<String>,
}
enum Data {
    Directory(Result<Directory, String>),
    Preview(Result<Preview, String>),
    WorkingDirectory(u32, Option<PathBuf>),
}
struct Reply {
    generation: u64,
    path: PathBuf,
    data: Data,
}
struct Job {
    generation: u64,
    current: Arc<AtomicU64>,
    path: PathBuf,
    preview: bool,
    cwd: Option<u32>,
    hidden: bool,
    tx: mpsc::Sender<Reply>,
}
fn workers() -> &'static mpsc::SyncSender<Job> {
    static WORKERS: OnceLock<mpsc::SyncSender<Job>> = OnceLock::new();
    WORKERS.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<Job>(32);
        let rx = Arc::new(Mutex::new(rx));
        for index in 0..2 {
            let rx = rx.clone();
            std::thread::Builder::new()
                .name(format!("yiterm-files-{index}"))
                .spawn(move || {
                    loop {
                        let Ok(job) = rx.lock().unwrap().recv() else {
                            break;
                        };
                        if job.current.load(Ordering::Acquire) != job.generation {
                            continue;
                        }
                        let data = if let Some(pid) = job.cwd {
                            Data::WorkingDirectory(pid, process_directory(pid))
                        } else if job.preview {
                            Data::Preview(read_preview(&job.path))
                        } else {
                            Data::Directory(read_directory(&job))
                        };
                        if job.current.load(Ordering::Acquire) == job.generation {
                            let _ = job.tx.send(Reply {
                                generation: job.generation,
                                path: job.path,
                                data,
                            });
                            crate::wake_ui();
                        }
                    }
                })
                .expect("starting file worker");
        }
        tx
    })
}
// Query one local process only. This runs in the bounded file worker pool,
// never in the render path, and does not enumerate the process tree.
fn process_directory(pid: u32) -> Option<PathBuf> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing()
            .without_tasks()
            .with_cwd(UpdateKind::Always),
    );
    let path = system.process(pid)?.cwd()?;
    path.is_absolute().then(|| path.to_owned())
}
fn read_directory(job: &Job) -> Result<Directory, String> {
    let mut entries = Vec::new();
    let mut notice = None;
    let iterator = fs::read_dir(&job.path).map_err(|e| e.to_string())?;
    for (index, item) in iterator.enumerate() {
        if job.current.load(Ordering::Acquire) != job.generation {
            return Err("Cancelled".into());
        }
        if index == DIRECTORY_LIMIT {
            notice = Some("Directory limited to 10,000 entries.".into());
            break;
        }
        let item = item.map_err(|e| e.to_string())?;
        let name = item.file_name().to_string_lossy().into_owned();
        if !job.hidden && name.starts_with('.') {
            continue;
        }
        let kind = item.file_type().map_err(|e| e.to_string())?;
        entries.push(Entry {
            path: item.path(),
            name,
            directory: kind.is_dir(),
            regular: kind.is_file(),
        });
    }
    entries.sort_by(|a, b| {
        b.directory
            .cmp(&a.directory)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(Directory { entries, notice })
}
fn read_preview(path: &Path) -> Result<Preview, String> {
    if !fs::symlink_metadata(path)
        .map_err(|e| e.to_string())?
        .is_file()
    {
        return Err("Only regular local files can be previewed.".into());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(PREVIEW_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let truncated = bytes.len() > PREVIEW_LIMIT as usize;
    bytes.truncate(PREVIEW_LIMIT as usize);
    if bytes.contains(&0) {
        return Err("Binary file. Use Open to view with a system application.".into());
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap()
        }
        Err(_) => {
            return Err("Unsupported encoding. UTF-8 preview only; use Open for this file.".into());
        }
    };
    let mut long_line = false;
    let lines = text
        .split('\n')
        .take(PREVIEW_LINE_LIMIT)
        .map(|line| {
            let mut chars = line.trim_end_matches('\r').chars();
            let mut text: String = chars
                .by_ref()
                .take(4096)
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            if chars.next().is_some() {
                long_line = true;
                text.push_str(" …");
            }
            text
        })
        .collect();
    let line_limit = text
        .bytes()
        .filter(|b| *b == b'\n')
        .take(PREVIEW_LINE_LIMIT)
        .count()
        >= PREVIEW_LINE_LIMIT;
    let notice = if line_limit {
        Some("Preview limited to 50,000 lines; long lines limited to 4,096 characters.".into())
    } else {
        match (truncated, long_line) {
            (true, _) => {
                Some("Preview limited to 1 MiB; long lines limited to 4,096 characters.".into())
            }
            (_, true) => Some("Long lines limited to 4,096 characters.".into()),
            _ => None,
        }
    };
    Ok(Preview {
        path: path.to_owned(),
        lines,
        notice,
    })
}
pub struct FileState {
    #[cfg(target_os = "linux")]
    id: u64,
    pub root: PathBuf,
    pub draft: String,
    pub hidden: bool,
    pub rows: Vec<Row>,
    pub expanded: HashSet<PathBuf>,
    pub cache: HashMap<PathBuf, Directory>,
    pub errors: HashMap<PathBuf, String>,
    pub selected: Option<usize>,
    pub preview: Option<Preview>,
    pub preview_path: Option<PathBuf>,
    pub preview_error: Option<String>,
    pub loading: HashSet<PathBuf>,
    active: bool,
    cwd_target: Option<u32>,
    cwd_pending: bool,
    cwd_next: Instant,
    generation: Arc<AtomicU64>,
    tx: mpsc::Sender<Reply>,
    rx: mpsc::Receiver<Reply>,
    #[cfg(not(target_os = "linux"))]
    pub scroll: gpui::UniformListScrollHandle,
    #[cfg(not(target_os = "linux"))]
    pub preview_scroll: gpui::UniformListScrollHandle,
}
impl Default for FileState {
    fn default() -> Self {
        #[cfg(target_os = "linux")]
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let root = std::env::current_dir()
            .ok()
            .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().to_owned()))
            .unwrap_or_else(|| PathBuf::from("/"));
        let (tx, rx) = mpsc::channel();
        Self {
            #[cfg(target_os = "linux")]
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            draft: root.to_string_lossy().into_owned(),
            root,
            hidden: false,
            rows: vec![],
            expanded: HashSet::new(),
            cache: HashMap::new(),
            errors: HashMap::new(),
            selected: None,
            preview: None,
            preview_path: None,
            preview_error: None,
            loading: HashSet::new(),
            active: false,
            cwd_target: None,
            cwd_pending: false,
            cwd_next: Instant::now(),
            generation: Arc::new(AtomicU64::new(0)),
            tx,
            rx,
            #[cfg(not(target_os = "linux"))]
            scroll: Default::default(),
            #[cfg(not(target_os = "linux"))]
            preview_scroll: Default::default(),
        }
    }
}
impl Drop for FileState {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}
impl FileState {
    #[cfg(target_os = "linux")]
    pub fn identity(&self) -> u64 {
        self.id
    }
    pub fn cancel(&mut self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.loading.clear();
        self.active = false;
        self.cwd_pending = false;
    }
    pub fn pause_follow(&mut self) {
        self.cwd_target = None;
        self.cwd_pending = false;
    }
    pub fn follow_directory(&mut self, pid: Option<u32>) {
        if self.cwd_target != pid {
            self.cwd_target = pid;
            self.cwd_pending = false;
            self.cwd_next = Instant::now();
        }
        let Some(pid) = pid else { return };
        if self.cwd_pending || Instant::now() < self.cwd_next {
            return;
        }
        self.cwd_next = Instant::now() + Duration::from_millis(500);
        let job = Job {
            generation: self.generation.load(Ordering::Acquire),
            current: self.generation.clone(),
            path: self.root.clone(),
            preview: false,
            cwd: Some(pid),
            hidden: self.hidden,
            tx: self.tx.clone(),
        };
        // Directory browsing has priority; a full queue simply delays this query.
        self.cwd_pending = workers().try_send(job).is_ok();
    }
    pub fn ensure_loaded(&mut self) {
        if self.active {
            return;
        }
        self.active = true;
        if self.preview_path.is_some() && self.preview.is_none() && self.preview_error.is_none() {
            self.request(self.preview_path.clone().unwrap(), true);
        }
        if !self.cache.contains_key(&self.root) && !self.errors.contains_key(&self.root) {
            self.request(self.root.clone(), false);
        }
        for path in self.expanded.clone() {
            if !self.cache.contains_key(&path) && !self.errors.contains_key(&path) {
                self.request(path, false);
            }
        }
    }
    fn request(&mut self, path: PathBuf, preview: bool) {
        if self.loading.contains(&path) {
            return;
        }
        let job = Job {
            generation: self.generation.load(Ordering::Acquire),
            current: self.generation.clone(),
            path: path.clone(),
            preview,
            cwd: None,
            hidden: self.hidden,
            tx: self.tx.clone(),
        };
        match workers().try_send(job) {
            Ok(()) => {
                self.errors.remove(&path);
                self.loading.insert(path);
            }
            Err(_) => {
                if preview {
                    self.preview_error = Some("File queue busy. Refresh to retry.".into());
                } else {
                    self.errors
                        .insert(path, "File queue busy. Refresh to retry.".into());
                }
            }
        }
    }
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        let mut tree_changed = false;
        while let Ok(reply) = self.rx.try_recv() {
            if reply.generation != self.generation.load(Ordering::Acquire) {
                continue;
            }
            if let Data::WorkingDirectory(pid, path) = reply.data {
                if self.cwd_target == Some(pid) {
                    self.cwd_pending = false;
                    if let Some(path) = path {
                        if path != self.root {
                            self.set_root(path);
                            changed = true;
                        }
                    }
                }
                continue;
            }
            self.loading.remove(&reply.path);
            changed = true;
            match reply.data {
                Data::Directory(Ok(directory)) => {
                    // A collapsed directory may finish its already-running read later.
                    // Drop that result so the collapsed subtree stays out of the cache.
                    if reply.path != self.root && !self.expanded.contains(&reply.path) {
                        continue;
                    }
                    tree_changed = true;
                    let total: usize = self.cache.values().map(|d| d.entries.len()).sum();
                    if total + directory.entries.len() <= CACHE_LIMIT
                        && self.cache.len() < DIRECTORY_CACHE_LIMIT
                    {
                        self.cache.insert(reply.path, directory);
                    } else {
                        self.errors.insert(reply.path, "Directory cache limit reached. Collapse folders, then refresh to retry.".into());
                    }
                }
                Data::Directory(Err(error)) => {
                    if reply.path == self.root || self.expanded.contains(&reply.path) {
                        self.errors.insert(reply.path, error);
                    }
                }
                Data::Preview(result) if self.preview_path.as_ref() == Some(&reply.path) => {
                    match result {
                        Ok(preview) => self.preview = Some(preview),
                        Err(error) => self.preview_error = Some(error),
                    }
                }
                _ => {}
            }
        }
        if tree_changed {
            self.rebuild();
        }
        changed
    }
    pub fn set_root(&mut self, path: PathBuf) {
        self.root = path;
        self.draft = self.root.to_string_lossy().into_owned();
        self.expanded.clear();
        #[cfg(not(target_os = "linux"))]
        {
            self.scroll = Default::default();
        }
        self.refresh();
    }
    pub fn invalidate(&mut self) {
        self.cancel();
        self.cache.clear();
        self.errors.clear();
        self.rows.clear();
        self.selected = None;
        self.back();
    }
    pub fn refresh(&mut self) {
        self.invalidate();
        self.ensure_loaded();
    }
    pub fn back(&mut self) {
        self.preview = None;
        self.preview_path = None;
        self.preview_error = None;
    }
    pub fn activate(&mut self, index: usize) {
        let Some(row) = self.rows.get(index) else {
            return;
        };
        let entry = row.entry.clone();
        self.selected = Some(index);
        if entry.directory {
            if !self.expanded.remove(&entry.path) {
                self.expanded.insert(entry.path.clone());
                if !self.cache.contains_key(&entry.path) {
                    self.request(entry.path, false);
                }
            } else {
                self.expanded.retain(|p| !p.starts_with(&entry.path));
                self.cache.retain(|p, _| !p.starts_with(&entry.path));
                self.errors.retain(|p, _| !p.starts_with(&entry.path));
            }
            self.rebuild();
        } else {
            self.preview = None;
            self.preview_error = None;
            self.preview_path = Some(entry.path.clone());
            if entry.regular {
                self.request(entry.path, true);
            } else {
                self.preview_error =
                    Some("Symbolic link or special file. Use Open to view externally.".into());
            }
            #[cfg(not(target_os = "linux"))]
            {
                self.preview_scroll = Default::default();
            }
        }
    }
    pub fn selected_path(&self) -> &Path {
        self.preview_path
            .as_deref()
            .or_else(|| {
                self.selected
                    .and_then(|i| self.rows.get(i))
                    .map(|r| r.entry.path.as_path())
            })
            .unwrap_or(&self.root)
    }
    fn rebuild(&mut self) {
        let selected = self
            .selected
            .and_then(|i| self.rows.get(i))
            .map(|r| r.entry.path.clone());
        self.rows.clear();
        // Iterative traversal keeps deeply nested trees off the call stack.
        let mut stack = vec![(self.root.clone(), 0, 0)];
        while let Some((path, index, depth)) = stack.pop() {
            let Some(directory) = self.cache.get(&path) else {
                continue;
            };
            let Some(entry) = directory.entries.get(index) else {
                continue;
            };
            stack.push((path, index + 1, depth));
            self.rows.push(Row {
                entry: entry.clone(),
                depth,
            });
            if self.expanded.contains(&entry.path) {
                stack.push((entry.path.clone(), 0, depth + 1));
            }
        }
        self.selected =
            selected.and_then(|path| self.rows.iter().position(|r| r.entry.path == path));
    }
    #[cfg(target_os = "linux")]
    pub fn open(&mut self) {
        let program = if cfg!(target_os = "macos") {
            "open"
        } else if cfg!(windows) {
            "explorer.exe"
        } else {
            "xdg-open"
        };
        if let Err(error) = std::process::Command::new(program)
            .arg(self.selected_path())
            .spawn()
        {
            self.preview_error = Some(error.to_string());
        }
    }
    pub fn notice(&self) -> String {
        if let Some(error) = &self.preview_error {
            return error.clone();
        }
        if let Some(preview) = &self.preview {
            return preview.notice.clone().unwrap_or_default();
        }
        if self.preview_path.is_some() {
            return "Loading preview…".into();
        }
        if !self.loading.is_empty() {
            return "Loading…".into();
        }
        if let Some((path, error)) = self.errors.iter().next() {
            return format!("{}: {error}", path.display());
        }
        if let Some((path, directory)) = self.cache.iter().find(|(_, d)| d.notice.is_some()) {
            return format!("{}: {}", path.display(), directory.notice.as_ref().unwrap());
        }
        if self.rows.is_empty() {
            "Empty directory".into()
        } else {
            String::new()
        }
    }
}
