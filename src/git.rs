//! Shared Git snapshots and serialized actions. Commands never run on the UI thread.
#[cfg(unix)]
mod executable;
#[cfg(unix)]
pub use executable::Session;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::{OsStr, OsString},
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
const STATUS_BYTES: usize = 4 * 1024 * 1024;
const DIFF_BYTES: usize = 2 * 1024 * 1024;
const RECENT_COMMITS: usize = 50;
const HISTORY_BYTES: usize = 1024 * 1024;
const MAX_CHANGES: usize = 20_000;
const MAX_LINES: usize = 50_000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Conflicted,
    Staged,
    Unstaged,
    Untracked,
}
impl Section {
    pub const ALL: [Self; 4] = [
        Self::Conflicted,
        Self::Staged,
        Self::Unstaged,
        Self::Untracked,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Conflicted => "Conflicted",
            Self::Staged => "Staged",
            Self::Unstaged => "Unstaged",
            Self::Untracked => "Untracked",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}
impl Mark {
    pub fn label(self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Untracked => "U",
            Self::Conflicted => "!",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub path: PathBuf,
    pub from: Option<PathBuf>,
    pub section: Section,
    pub mark: Mark,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListRow {
    Header(Section, usize),
    Change(usize),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub subject: String,
    pub author: String,
    pub when: String,
    pub refs: Vec<String>,
}
impl Commit {
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(7)]
    }
    pub fn details(&self) -> String {
        format!(
            "{}\n{} · {}\n{}\n{}",
            self.subject,
            self.author,
            self.when,
            self.id,
            self.refs.join(", ")
        )
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub root: PathBuf,
    pub git_dir: PathBuf,
    pub head: Option<String>,
    pub index_fingerprint: [u8; 32],
    pub branch: String,
    pub upstream: Option<String>,
    pub rows: Vec<ListRow>,
    pub changes: Vec<Change>,
    pub commits: Vec<Commit>,
    pub history_error: Option<String>,
    pub marks: HashMap<PathBuf, Mark>,
    pub dirty: HashSet<PathBuf>,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RepoKey {
    pub root: PathBuf,
    pub git_dir: PathBuf,
}
impl Snapshot {
    pub fn key(&self) -> RepoKey {
        RepoKey {
            root: self.root.clone(),
            git_dir: self.git_dir.clone(),
        }
    }
    pub fn count(&self, section: Section) -> usize {
        self.changes.iter().filter(|c| c.section == section).count()
    }
    pub fn badge(&self, path: &Path) -> Option<&'static str> {
        if let Some(mark) = self.marks.get(path) {
            return Some(mark.label());
        }
        if path
            .ancestors()
            .skip(1)
            .take_while(|p| p.starts_with(&self.root))
            .any(|p| self.marks.get(p) == Some(&Mark::Untracked))
        {
            return Some("U");
        }
        self.dirty.contains(path).then_some("•")
    }
}
#[derive(Clone, Copy)]
pub enum LineKind {
    Context,
    Addition,
    Deletion,
    Header,
    Hunk,
}
#[derive(Clone)]
pub struct DiffLine {
    pub text: String,
    pub kind: LineKind,
}
pub struct Diff {
    pub id: uuid::Uuid,
    pub lines: Vec<DiffLine>,
    pub notice: Option<String>,
}
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(PathBuf::from(OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(bytes)
            .map(PathBuf::from)
            .map_err(|_| "Git returned a path outside UTF-8.".into())
    }
}
fn relative_path(bytes: &[u8]) -> Result<PathBuf, String> {
    let path = path_from_bytes(bytes)?;
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err("Git returned an invalid relative path.".into());
    }
    Ok(path)
}
pub fn shown(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
fn code_mark(code: u8) -> Result<Mark, String> {
    match code {
        b'M' | b'T' => Ok(Mark::Modified),
        b'A' | b'C' => Ok(Mark::Added),
        b'D' => Ok(Mark::Deleted),
        b'R' => Ok(Mark::Renamed),
        _ => Err("Unknown Git status code.".into()),
    }
}
fn snapshot(root: PathBuf, git_dir: PathBuf, bytes: &[u8]) -> Result<Snapshot, String> {
    let mut result = Snapshot {
        root,
        git_dir,
        head: None,
        index_fingerprint: [0; 32],
        branch: "HEAD".into(),
        upstream: None,
        changes: vec![],
        commits: vec![],
        history_error: None,
        rows: vec![],
        marks: HashMap::new(),
        dirty: HashSet::new(),
    };
    let mut staged_hash = Sha256::new();
    let mut records = bytes.split(|b| *b == 0).filter(|r| !r.is_empty());
    while let Some(record) = records.next() {
        if let Some(value) = record.strip_prefix(b"# branch.oid ") {
            result.head =
                (value != b"(initial)").then(|| String::from_utf8_lossy(value).into_owned());
            continue;
        }
        if let Some(value) = record.strip_prefix(b"# branch.head ") {
            result.branch = if value == b"(detached)" {
                "Detached HEAD".into()
            } else {
                String::from_utf8_lossy(value).into_owned()
            };
            continue;
        }
        if let Some(value) = record.strip_prefix(b"# branch.upstream ") {
            result.upstream = Some(String::from_utf8_lossy(value).into_owned());
            continue;
        }
        if record.starts_with(b"# ") {
            continue;
        }
        let kind = *record.first().ok_or("Empty Git status record.")?;
        let (path, from, x, y) = if kind == b'?' {
            (
                relative_path(record.get(2..).ok_or("Invalid untracked record.")?)?,
                None,
                b'?',
                b'?',
            )
        } else {
            let fields = match kind {
                b'1' => 9,
                b'2' => 10,
                b'u' => 11,
                _ => return Err("Unknown Git status record.".into()),
            };
            let parts: Vec<_> = record.splitn(fields, |b| *b == b' ').collect();
            if parts.len() != fields || parts[1].len() != 2 {
                return Err("Malformed Git status record.".into());
            }
            let path = relative_path(parts[fields - 1])?;
            let from = if kind == b'2' {
                Some(relative_path(
                    records.next().ok_or("Missing rename source.")?,
                )?)
            } else {
                None
            };
            (path, from, parts[1][0], parts[1][1])
        };
        if kind != b'?' && kind != b'u' && x != b'.' {
            staged_hash.update(record);
            staged_hash.update([0]);
            if let Some(from) = &from {
                staged_hash.update(from.as_os_str().as_encoded_bytes());
                staged_hash.update([0]);
            }
        }
        let mut push = |section, mark, from| -> Result<(), String> {
            if result.changes.len() >= MAX_CHANGES {
                return Err("Git status exceeds 20,000 changes. Narrow the repository or use Git in the terminal.".into());
            }
            result.changes.push(Change {
                path: path.clone(),
                from,
                section,
                mark,
            });
            Ok(())
        };
        let mark = if kind == b'u' {
            push(Section::Conflicted, Mark::Conflicted, None)?;
            Mark::Conflicted
        } else if kind == b'?' {
            push(Section::Untracked, Mark::Untracked, None)?;
            Mark::Untracked
        } else {
            if x != b'.' {
                push(Section::Staged, code_mark(x)?, from.clone())?;
            }
            if y != b'.' {
                push(Section::Unstaged, code_mark(y)?, None)?;
            }
            if y != b'.' {
                code_mark(y)?
            } else {
                code_mark(x)?
            }
        };
        let absolute = result.root.join(&path);
        result.marks.insert(absolute.clone(), mark);
        if let Some(from) = from {
            result.marks.insert(result.root.join(from), mark);
        }
        for parent in absolute
            .ancestors()
            .skip(1)
            .take_while(|p| p.starts_with(&result.root))
        {
            if result.dirty.len() >= 80_000 {
                return Err("Git directory index exceeds the display limit.".into());
            }
            result.dirty.insert(parent.to_owned());
        }
    }
    result.index_fingerprint = staged_hash.finalize().into();
    result.changes.sort_by(|a, b| a.path.cmp(&b.path));
    for section in Section::ALL {
        let rows: Vec<_> = result
            .changes
            .iter()
            .enumerate()
            .filter(|(_, c)| c.section == section)
            .map(|(index, _)| ListRow::Change(index))
            .collect();
        if !rows.is_empty() {
            result.rows.push(ListRow::Header(section, rows.len()));
            result.rows.extend(rows);
        }
    }
    Ok(result)
}
fn run(
    #[cfg(unix)] git: &Path,
    folder: &Path,
    args: &[OsString],
    limit: usize,
    generation: &AtomicU64,
    expected: u64,
) -> Result<(bool, Vec<u8>, String), String> {
    run_with_timeout(
        #[cfg(unix)]
        git,
        folder,
        args,
        limit,
        generation,
        expected,
        Duration::from_secs(5),
    )
}
fn run_with_timeout(
    #[cfg(unix)] git: &Path,
    folder: &Path,
    args: &[OsString],
    limit: usize,
    generation: &AtomicU64,
    expected: u64,
    timeout: Duration,
) -> Result<(bool, Vec<u8>, String), String> {
    if generation.load(Ordering::Acquire) != expected {
        return Err("Cancelled".into());
    }
    #[cfg(unix)]
    let mut command = Command::new(git);
    #[cfg(windows)]
    let mut command = Command::new("git");
    command
        .args([
            "--no-pager",
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-C",
        ])
        .arg(folder)
        .args(args)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0");
    capture(command, limit, generation, expected, timeout)
}
fn capture(
    mut command: Command,
    limit: usize,
    generation: &AtomicU64,
    expected: u64,
    timeout: Duration,
) -> Result<(bool, Vec<u8>, String), String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "Git is not installed or is missing from PATH.".into()
        } else {
            format!("Cannot start Git: {e}")
        }
    })?;
    let (tx, rx) = mpsc::channel();
    for (index, pipe) in [
        (
            0,
            Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
        ),
        (
            1,
            Box::new(child.stderr.take().unwrap()) as Box<dyn Read + Send>,
        ),
    ] {
        let tx = tx.clone();
        let cap = if index == 0 { limit } else { 64 * 1024 };
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = pipe
                .take((cap + 1) as u64)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            let _ = tx.send((index, result));
        });
    }
    drop(tx);
    let started = Instant::now();
    let mut output = [None, None];
    let status = loop {
        while let Ok((index, result)) = rx.try_recv() {
            match result {
                Ok(bytes) => output[index] = Some(bytes),
                Err(e) => {
                    terminate(&mut child);
                    return Err(format!("Cannot read Git output: {e}"));
                }
            }
        }
        let overflow = output[0].as_ref().is_some_and(|b| b.len() > limit)
            || output[1].as_ref().is_some_and(|b| b.len() > 64 * 1024);
        let cancelled = generation.load(Ordering::Acquire) != expected;
        if overflow || cancelled || started.elapsed() > timeout {
            terminate(&mut child);
            return Err(if overflow {
                "Git output exceeds the display limit."
            } else if cancelled {
                "Cancelled"
            } else {
                "Git exceeded its time limit. Refresh the repository before retrying."
            }
            .into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                terminate(&mut child);
                return Err(format!("Cannot wait for Git: {e}"));
            }
        }
    };
    while output.iter().any(Option::is_none) {
        let (index, result) = rx
            .recv_timeout(Duration::from_millis(250))
            .map_err(|_| "Git output pipe did not close.".to_string())?;
        output[index] = Some(result.map_err(|e| e.to_string())?);
    }
    let stdout = output[0].take().unwrap();
    let stderr = output[1].take().unwrap();
    if stdout.len() > limit || stderr.len() > 64 * 1024 {
        return Err("Git output exceeds the display limit.".into());
    }
    Ok((
        status.success(),
        stdout,
        String::from_utf8_lossy(&stderr)
            .trim()
            .chars()
            .take(4096)
            .collect(),
    ))
}
fn terminate(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}
fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}
fn read_status(
    #[cfg(unix)] git: &Path,
    folder: &Path,
    generation: &AtomicU64,
    expected: u64,
) -> Result<Option<Snapshot>, String> {
    let (ok, root, error) = run(
        #[cfg(unix)]
        git,
        folder,
        &args(&["rev-parse", "--show-toplevel"]),
        64 * 1024,
        generation,
        expected,
    )?;
    if !ok {
        return if error.contains("not a git repository")
            || error.contains("must be run in a work tree")
        {
            Ok(None)
        } else {
            Err(error)
        };
    }
    let root = path_from_bytes(root.strip_suffix(b"\n").unwrap_or(&root))?;
    let (ok, git_dir, error) = run(
        #[cfg(unix)]
        git,
        &root,
        &args(&["rev-parse", "--absolute-git-dir"]),
        64 * 1024,
        generation,
        expected,
    )?;
    if !ok {
        return Err(error);
    }
    let git_dir = path_from_bytes(git_dir.strip_suffix(b"\n").unwrap_or(&git_dir))?;
    let (ok, bytes, error) = run(
        #[cfg(unix)]
        git,
        &root,
        &args(&[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=normal",
        ]),
        STATUS_BYTES,
        generation,
        expected,
    )?;
    if !ok {
        return Err(error);
    }
    snapshot(root, git_dir, &bytes).map(Some)
}
// Keep history failures separate from status so staging remains available.
fn read_snapshot(
    #[cfg(unix)] git: &Path,
    folder: &Path,
    generation: &AtomicU64,
    expected: u64,
) -> Result<Option<Snapshot>, String> {
    let Some(mut snapshot) = read_status(
        #[cfg(unix)]
        git,
        folder,
        generation,
        expected,
    )?
    else {
        return Ok(None);
    };
    if let Some(head) = &snapshot.head {
        match read_history(
            #[cfg(unix)]
            git,
            &snapshot.root,
            head,
            generation,
            expected,
        ) {
            Ok(commits) => snapshot.commits = commits,
            Err(error) => snapshot.history_error = Some(error),
        }
    }
    Ok(Some(snapshot))
}
fn read_history(
    #[cfg(unix)] git: &Path,
    root: &Path,
    head: &str,
    generation: &AtomicU64,
    expected: u64,
) -> Result<Vec<Commit>, String> {
    let (ok, bytes, error) = run(
        #[cfg(unix)]
        git,
        root,
        &args(&[
            "log",
            "-z",
            "--no-color",
            "--no-show-signature",
            "--decorate=short",
            "--encoding=UTF-8",
            &format!("--max-count={RECENT_COMMITS}"),
            "--format=%H%x00%s%x00%an%x00%ar%x00%D",
            head,
            "--",
        ]),
        HISTORY_BYTES,
        generation,
        expected,
    )?;
    if !ok {
        return Err(error);
    }
    let mut fields: Vec<_> = bytes.split(|b| *b == 0).collect();
    if fields.last() == Some(&&b""[..]) {
        fields.pop();
    }
    if fields.len() % 5 != 0 || fields.len() / 5 > RECENT_COMMITS {
        return Err("Malformed Git history.".into());
    }
    fields
        .chunks_exact(5)
        .map(|fields| {
            if !matches!(fields[0].len(), 40 | 64) || !fields[0].iter().all(u8::is_ascii_hexdigit) {
                return Err("Invalid Git commit ID.".into());
            }
            let text = |bytes: &[u8]| {
                String::from_utf8_lossy(bytes)
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .take(4096)
                    .collect::<String>()
            };
            Ok(Commit {
                id: text(fields[0]),
                subject: text(fields[1]),
                author: text(fields[2]),
                when: text(fields[3]),
                refs: text(fields[4])
                    .split(", ")
                    .filter(|r| !r.is_empty())
                    .map(str::to_owned)
                    .collect(),
            })
        })
        .collect()
}
fn read_diff(
    #[cfg(unix)] git: &Path,
    root: &Path,
    change: &Change,
    generation: &AtomicU64,
    expected: u64,
) -> Result<Diff, String> {
    let bytes = if change.section == Section::Untracked {
        let path = root.join(&change.path);
        if !fs::symlink_metadata(&path)
            .map_err(|e| e.to_string())?
            .is_file()
        {
            return Err("Untracked directory or symbolic link. Browse it in Files.".into());
        }
        let mut bytes = Vec::new();
        fs::File::open(path)
            .map_err(|e| e.to_string())?
            .take((DIFF_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > DIFF_BYTES {
            return Err("File exceeds the 2 MiB preview limit.".into());
        }
        bytes
    } else {
        let mut arguments = args(&["diff", "--no-color", "--no-ext-diff", "--no-textconv"]);
        if change.section == Section::Staged {
            arguments.push("--cached".into());
        }
        arguments.push("--".into());
        arguments.push(change.path.as_os_str().into());
        if let Some(from) = &change.from {
            arguments.push(from.as_os_str().into());
        }
        let (ok, bytes, error) = run(
            #[cfg(unix)]
            git,
            root,
            &arguments,
            DIFF_BYTES,
            generation,
            expected,
        )?;
        if !ok {
            return Err(error);
        }
        bytes
    };
    if bytes.contains(&0) {
        return Err("Binary file; no text diff available.".into());
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| "Text diff is not UTF-8. Use Git in the terminal.".to_string())?;
    let mut shortened = false;
    let mut source = text.lines();
    let mut in_hunk = false;
    let lines = source
        .by_ref()
        .take(MAX_LINES)
        .map(|line| {
            let kind = if change.section == Section::Untracked {
                LineKind::Addition
            } else if line.starts_with("diff ") {
                in_hunk = false;
                LineKind::Header
            } else if line.starts_with("@@") {
                in_hunk = true;
                LineKind::Hunk
            } else if !in_hunk
                && (line.starts_with("+++")
                    || line.starts_with("---")
                    || line.starts_with("index "))
            {
                LineKind::Header
            } else if line.starts_with('+') {
                LineKind::Addition
            } else if line.starts_with('-') {
                LineKind::Deletion
            } else {
                LineKind::Context
            };
            let mut chars = line.chars();
            let mut text: String = chars
                .by_ref()
                .take(4096)
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            if chars.next().is_some() {
                text.push_str(" …");
                shortened = true;
            }
            DiffLine { text, kind }
        })
        .collect::<Vec<_>>();
    let capped = source.next().is_some();
    let notice = if capped || shortened {
        Some("Display limited to 50,000 lines and 4,096 characters per line.".into())
    } else if lines.is_empty() {
        Some("No text diff. The change may contain file mode or submodule metadata only.".into())
    } else {
        None
    };
    Ok(Diff {
        id: uuid::Uuid::new_v4(),
        lines,
        notice,
    })
}
#[derive(Clone)]
enum Action {
    Stage(Vec<Change>),
    Unstage(Vec<Change>),
    Discard(Change),
    Commit(String),
}
impl Action {
    fn label(&self) -> &'static str {
        match self {
            Self::Stage(_) => "Staging",
            Self::Unstage(_) => "Unstaging",
            Self::Discard(_) => "Discarding",
            Self::Commit(_) => "Committing",
        }
    }
}
struct ActionRequest {
    #[cfg(unix)]
    session: Arc<Session>,
    id: uuid::Uuid,
    snapshot: Arc<Snapshot>,
    action: Action,
}
fn apply_action(#[cfg(unix)] git: &Path, request: &ActionRequest) -> Result<(), String> {
    // A view disappearing never cancels a write. This private token stays valid
    // for validation and the command, whose target was captured at click time.
    let token = AtomicU64::new(0);
    let expected = &request.snapshot;
    let fresh = read_status(
        #[cfg(unix)]
        git,
        &expected.root,
        &token,
        0,
    )?
    .ok_or("The repository no longer exists. Refresh before retrying.")?;
    if fresh.key() != expected.key()
        || fresh.head != expected.head
        || fresh.branch != expected.branch
    {
        return Err("The repository or HEAD changed. Refresh before retrying.".into());
    }
    let mut file =
        tempfile::NamedTempFile::new().map_err(|e| format!("Cannot prepare Git input: {e}"))?;
    let mut arguments = match &request.action {
        Action::Stage(changes) | Action::Unstage(changes) => {
            if changes.is_empty() {
                return Err("No files selected.".into());
            }
            let staging = matches!(request.action, Action::Stage(_));
            for change in changes {
                if !fresh.changes.contains(change) {
                    return Err(format!(
                        "{} changed since it was displayed. Refresh before retrying.",
                        shown(&change.path)
                    ));
                }
                if staging && !matches!(change.section, Section::Unstaged | Section::Untracked)
                    || !staging && change.section != Section::Staged
                {
                    return Err("This section does not support that action.".into());
                }
                for path in std::iter::once(&change.path).chain(change.from.iter()) {
                    file.write_all(path.as_os_str().as_encoded_bytes())
                        .and_then(|_| file.write_all(&[0]))
                        .map_err(|e| e.to_string())?;
                }
            }
            let mut arguments = if staging {
                args(&["add", "-A"])
            } else if fresh.head.is_none() {
                args(&["rm", "--cached", "-f", "-q", "--ignore-unmatch"])
            } else {
                args(&["reset", "-q"])
            };
            let mut pathspec = OsString::from("--pathspec-from-file=");
            pathspec.push(file.path());
            arguments.push(pathspec);
            arguments.push("--pathspec-file-nul".into());
            arguments
        }
        Action::Discard(change) => {
            if !matches!(change.section, Section::Unstaged | Section::Untracked)
                || !fresh.changes.contains(change)
                || fresh.index_fingerprint != expected.index_fingerprint
            {
                return Err(
                    "The change or index changed. Review it before discarding again.".into(),
                );
            }
            if change.section == Section::Untracked {
                // One literal path only. Respect ignored files and nested repositories.
                let mut arguments = args(&["clean", "-f", "-d", "--"]);
                arguments.push(change.path.as_os_str().to_owned());
                arguments
            } else {
                for path in std::iter::once(&change.path).chain(change.from.iter()) {
                    file.write_all(path.as_os_str().as_encoded_bytes())
                        .and_then(|_| file.write_all(&[0]))
                        .map_err(|e| e.to_string())?;
                }
                let mut arguments = if matches!(change.mark, Mark::Added | Mark::Renamed) {
                    args(&["rm", "-f", "-q"])
                } else {
                    args(&["restore", "--worktree"])
                };
                let mut pathspec = OsString::from("--pathspec-from-file=");
                pathspec.push(file.path());
                arguments.push(pathspec);
                arguments.push("--pathspec-file-nul".into());
                arguments
            }
        }
        Action::Commit(message) => {
            if message.trim().is_empty() || message.len() > 64 * 1024 || message.contains('\0') {
                return Err(
                    "Use a non-empty commit message up to 64 KiB, without NUL characters.".into(),
                );
            }
            if fresh.count(Section::Conflicted) > 0 || fresh.count(Section::Staged) == 0 {
                return Err("Commit requires staged changes and no unresolved conflicts.".into());
            }
            if fresh.index_fingerprint != expected.index_fingerprint {
                return Err("The staged snapshot changed. Review it and commit again.".into());
            }
            file.write_all(message.as_bytes())
                .map_err(|e| e.to_string())?;
            let mut arguments = args(&["commit", "-F"]);
            arguments.push(file.path().as_os_str().to_owned());
            arguments
        }
    };
    file.flush().map_err(|e| e.to_string())?;
    // No shell interpolation; every write is limited to the captured repository.
    let (ok, output, error) = run_with_timeout(
        #[cfg(unix)]
        git,
        &expected.root,
        &std::mem::take(&mut arguments),
        STATUS_BYTES,
        &token,
        0,
        Duration::from_secs(120),
    )?;
    if ok {
        if matches!(&request.action, Action::Discard(change) if change.section == Section::Untracked)
            && output.is_empty()
        {
            return Err("Git did not remove this untracked item; it may contain ignored files or a nested repository.".into());
        }
        Ok(())
    } else {
        Err(if error.is_empty() {
            "Git action failed. Refresh before retrying.".into()
        } else {
            error
        })
    }
}
enum Work {
    Action(Arc<ActionRequest>),
    Status(PathBuf),
    Diff(PathBuf, Change),
}
struct Job {
    #[cfg(unix)]
    session: Arc<Session>,
    work: Work,
    generation: Arc<AtomicU64>,
    expected: u64,
}
enum Reply {
    Action(uuid::Uuid, Result<(), String>),
    Status(u64, Result<Option<Snapshot>, String>),
    Diff(u64, Result<Diff, String>),
}
fn process_job(#[cfg(unix)] git: &Path, job: Job) -> Reply {
    match job.work {
        Work::Action(request) => Reply::Action(
            request.id,
            apply_action(
                #[cfg(unix)]
                git,
                &request,
            ),
        ),
        Work::Status(folder) => Reply::Status(
            job.expected,
            read_snapshot(
                #[cfg(unix)]
                git,
                &folder,
                &job.generation,
                job.expected,
            ),
        ),
        Work::Diff(root, change) => Reply::Diff(
            job.expected,
            read_diff(
                #[cfg(unix)]
                git,
                &root,
                &change,
                &job.generation,
                job.expected,
            ),
        ),
    }
}
pub struct Controller {
    #[cfg(unix)]
    session: Option<Arc<Session>>,
    pub snapshot: Option<Arc<Snapshot>>,
    pub error: Option<String>,
    pub outside: bool,
    pub cursor: Option<usize>,
    pub selected: Option<Change>,
    pub diff: Option<Result<Diff, String>>,
    pub loading: bool,
    pub commit_message: String,
    action: Option<Arc<ActionRequest>>,
    action_queued: bool,
    action_error: Option<(RepoKey, String)>,
    draft_key: Option<RepoKey>,
    drafts: VecDeque<(RepoKey, String)>,
    read_folder: Option<PathBuf>,
    acted: Vec<PathBuf>,
    folder: Option<PathBuf>,
    status_generation: Arc<AtomicU64>,
    diff_generation: Arc<AtomicU64>,
    tx: Option<mpsc::SyncSender<Job>>,
    rx: mpsc::Receiver<Reply>,
    replies: Option<mpsc::Sender<Reply>>,
    dirty: bool,
    due: Instant,
    activity: Option<(uuid::Uuid, u64)>,
    diff_pending: bool,
    diff_queued: bool,
    #[cfg(not(target_os = "linux"))]
    pub scroll: gpui::UniformListScrollHandle,
    #[cfg(not(target_os = "linux"))]
    pub diff_scroll: gpui::UniformListScrollHandle,
    #[cfg(not(target_os = "linux"))]
    pub history_scroll: gpui::UniformListScrollHandle,
}
impl Default for Controller {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            #[cfg(unix)]
            session: None,
            snapshot: None,
            error: None,
            outside: false,
            cursor: None,
            selected: None,
            diff: None,
            loading: false,
            commit_message: String::new(),
            action: None,
            action_queued: false,
            action_error: None,
            draft_key: None,
            drafts: VecDeque::new(),
            read_folder: None,
            acted: vec![],
            folder: None,
            status_generation: Arc::new(AtomicU64::new(0)),
            diff_generation: Arc::new(AtomicU64::new(0)),
            tx: None,
            rx,
            replies: Some(tx),
            dirty: false,
            due: Instant::now(),
            activity: None,
            diff_pending: false,
            diff_queued: false,
            #[cfg(not(target_os = "linux"))]
            scroll: Default::default(),
            #[cfg(not(target_os = "linux"))]
            diff_scroll: Default::default(),
            #[cfg(not(target_os = "linux"))]
            history_scroll: Default::default(),
        }
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.status_generation.fetch_add(1, Ordering::AcqRel);
        self.diff_generation.fetch_add(1, Ordering::AcqRel);
    }
}
impl Controller {
    fn worker(&mut self) -> &mpsc::SyncSender<Job> {
        if self.tx.is_none() {
            let (tx, rx) = mpsc::sync_channel::<Job>(1);
            let replies = self.replies.take().unwrap();
            thread::Builder::new()
                .name("yiterm-git".into())
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        if !matches!(job.work, Work::Action(_))
                            && job.generation.load(Ordering::Acquire) != job.expected
                        {
                            continue;
                        }
                        #[cfg(unix)]
                        let reply = {
                            let folder = match &job.work {
                                Work::Action(request) => &request.snapshot.root,
                                Work::Status(folder) | Work::Diff(folder, _) => folder,
                            };
                            match job.session.resolve(folder, &job.generation, job.expected) {
                                Ok(git) => process_job(&git, job),
                                Err(error) => match job.work {
                                    Work::Action(request) => Reply::Action(request.id, Err(error)),
                                    Work::Status(_) => Reply::Status(job.expected, Err(error)),
                                    Work::Diff(_, _) => Reply::Diff(job.expected, Err(error)),
                                },
                            }
                        };
                        #[cfg(windows)]
                        let reply = process_job(job);
                        if replies.send(reply).is_err() {
                            break;
                        }
                        crate::wake_ui();
                    }
                })
                .expect("starting Git worker");
            self.tx = Some(tx);
        }
        self.tx.as_ref().unwrap()
    }
    pub fn sync(
        &mut self,
        folder: Option<&Path>,
        activity: Option<(uuid::Uuid, u64)>,
        #[cfg(unix)] session: Option<&Arc<Session>>,
    ) -> bool {
        let mut changed = false;
        #[cfg(unix)]
        let session_changed = match (&self.session, session) {
            (Some(old), Some(new)) => !Arc::ptr_eq(old, new),
            (None, None) => false,
            _ => true,
        };
        #[cfg(windows)]
        let session_changed = false;
        if self.folder.as_deref() != folder || session_changed {
            #[cfg(unix)]
            {
                self.session = session.cloned();
            }
            self.status_generation.fetch_add(1, Ordering::AcqRel);
            self.loading = false;
            self.back();
            self.folder = folder.map(Path::to_owned);
            self.read_folder = None;
            self.error = None;
            self.outside = false;
            if session_changed
                || !self
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| folder.is_some_and(|p| p.starts_with(&s.root)))
            {
                self.snapshot = None;
            }
            self.dirty = folder.is_some();
            self.due = Instant::now();
            self.activity = activity;
            changed = true;
            #[cfg(not(target_os = "linux"))]
            {
                self.scroll = Default::default();
                self.history_scroll = Default::default();
            }
        } else if self.activity != activity {
            self.activity = activity;
            if !self.dirty {
                self.dirty = folder.is_some();
            }
            self.due = Instant::now() + Duration::from_millis(500);
        }
        changed | self.poll()
    }
    #[cfg(unix)]
    pub fn redetect(&mut self) {
        if self.busy() {
            return;
        }
        if let Some(session) = &self.session {
            session.invalidate();
        }
        self.status_generation.fetch_add(1, Ordering::AcqRel);
        self.loading = false;
        self.read_folder = None;
        self.snapshot = None;
        self.error = None;
        self.outside = false;
        self.back();
        self.refresh();
    }
    pub fn refresh(&mut self) {
        if self.selected.is_some() {
            self.diff_generation.fetch_add(1, Ordering::AcqRel);
            self.diff = None;
            self.diff_pending = true;
            self.diff_queued = false;
        }
        if self.folder.is_some() {
            self.dirty = true;
            self.due = Instant::now();
        }
    }
    pub fn back(&mut self) {
        self.diff_generation.fetch_add(1, Ordering::AcqRel);
        self.selected = None;
        self.diff = None;
        self.diff_pending = false;
        self.diff_queued = false;
    }
    pub fn open(&mut self, change: Change) {
        self.back();
        self.selected = Some(change);
        self.diff_pending = true;
        #[cfg(not(target_os = "linux"))]
        {
            self.diff_scroll = Default::default();
        }
    }
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(reply) = self.rx.try_recv() {
            match reply {
                Reply::Action(id, result) if self.action.as_ref().is_some_and(|a| a.id == id) => {
                    let request = self.action.take().unwrap();
                    self.action_queued = false;
                    let key = request.snapshot.key();
                    self.acted.push(key.root.clone());
                    match result {
                        Ok(()) => {
                            self.action_error = None;
                            if let Action::Commit(message) = &request.action {
                                if self.draft_key.as_ref() == Some(&key)
                                    && self.commit_message == *message
                                {
                                    self.commit_message.clear();
                                }
                                self.drafts.retain(|(draft_key, draft)| {
                                    !(draft_key == &key && draft == message)
                                });
                            }
                        }
                        Err(error) => self.action_error = Some((key, error)),
                    }
                    self.status_generation.fetch_add(1, Ordering::AcqRel);
                    self.loading = false;
                    self.read_folder = None;
                    self.back();
                    self.refresh();
                    changed = true;
                }
                Reply::Status(epoch, result)
                    if epoch == self.status_generation.load(Ordering::Acquire) =>
                {
                    self.loading = false;
                    changed = true;
                    match result {
                        Ok(snapshot) => {
                            self.outside = snapshot.is_none();
                            self.error = None;
                            let next = snapshot.map(Arc::new);
                            self.read_folder = self.folder.clone();
                            self.switch_draft(next.as_ref().map(|s| s.key()));
                            if self.snapshot != next {
                                self.back();
                                self.snapshot = next;
                                self.cursor = None;
                            }
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
                Reply::Diff(epoch, result)
                    if epoch == self.diff_generation.load(Ordering::Acquire) =>
                {
                    self.diff = Some(result);
                    self.diff_pending = false;
                    self.diff_queued = false;
                    changed = true;
                }
                _ => {}
            }
        }
        if let Some(request) = &self.action {
            if !self.action_queued {
                let job = Job {
                    #[cfg(unix)]
                    session: request.session.clone(),
                    work: Work::Action(request.clone()),
                    generation: Arc::new(AtomicU64::new(0)),
                    expected: 0,
                };
                if self.worker().try_send(job).is_ok() {
                    self.action_queued = true;
                    changed = true;
                }
            }
            return changed;
        }
        if self.folder.is_none() {
            return changed;
        }
        #[cfg(unix)]
        if self.session.is_none() {
            return changed;
        }
        if self.diff_pending && !self.diff_queued {
            if let (Some(snapshot), Some(change)) = (&self.snapshot, &self.selected) {
                let job = Job {
                    #[cfg(unix)]
                    session: self.session.as_ref().unwrap().clone(),
                    work: Work::Diff(snapshot.root.clone(), change.clone()),
                    generation: self.diff_generation.clone(),
                    expected: self.diff_generation.load(Ordering::Acquire),
                };
                if self.worker().try_send(job).is_ok() {
                    self.diff_queued = true;
                    changed = true;
                }
            }
        }
        if self.dirty && !self.loading && Instant::now() >= self.due {
            let job = Job {
                #[cfg(unix)]
                session: self.session.as_ref().unwrap().clone(),
                work: Work::Status(self.folder.clone().unwrap()),
                generation: self.status_generation.clone(),
                expected: self.status_generation.load(Ordering::Acquire),
            };
            if self.worker().try_send(job).is_ok() {
                self.dirty = false;
                self.loading = true;
                changed = true;
            }
        }
        changed
    }
    fn switch_draft(&mut self, key: Option<RepoKey>) {
        if self.draft_key == key {
            return;
        }
        if let Some(old) = self.draft_key.take() {
            self.drafts.retain(|(k, _)| k != &old);
            if !self.commit_message.is_empty() && self.commit_message.len() <= 64 * 1024 {
                self.drafts
                    .push_back((old, std::mem::take(&mut self.commit_message)));
            }
        }
        self.commit_message.clear();
        if let Some(key) = &key {
            if let Some(index) = self.drafts.iter().position(|(k, _)| k == key) {
                self.commit_message = self.drafts.remove(index).unwrap().1;
            }
        }
        while self.drafts.len() > 8 {
            self.drafts.pop_front();
        }
        self.draft_key = key;
    }
    pub fn draft_key(&self) -> Option<&RepoKey> {
        self.draft_key.as_ref()
    }
    pub fn busy(&self) -> bool {
        self.action.is_some()
    }
    pub fn can_act(&self) -> bool {
        self.snapshot.is_some()
            && self.folder.is_some()
            && self.read_folder == self.folder
            && self.error.is_none()
            // A refresh does not invalidate a resolved folder. The write worker
            // validates HEAD and the selected/index contents again before acting.
            && !self.busy()
    }
    pub fn can_commit(&self) -> bool {
        self.can_act()
            && self
                .snapshot
                .as_ref()
                .is_some_and(|s| s.count(Section::Staged) > 0 && s.count(Section::Conflicted) == 0)
            && !self.commit_message.trim().is_empty()
            && self.commit_message.len() <= 64 * 1024
            && !self.commit_message.contains('\0')
    }
    pub fn action_label(&self) -> Option<String> {
        self.action
            .as_ref()
            .map(|a| format!("{} {}", a.action.label(), shown(&a.snapshot.root)))
    }
    pub fn action_error(&self) -> Option<&str> {
        self.action_error
            .as_ref()
            .filter(|(key, _)| self.snapshot.as_ref().is_some_and(|s| s.key() == *key))
            .map(|(_, message)| message.as_str())
    }
    pub fn take_acted(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.acted)
    }
    fn enqueue(&mut self, action: Action) {
        if !self.can_act() {
            return;
        }
        let snapshot = self.snapshot.as_ref().unwrap().clone();
        self.action_error = None;
        self.status_generation.fetch_add(1, Ordering::AcqRel);
        self.loading = false;
        self.back();
        self.action = Some(Arc::new(ActionRequest {
            id: uuid::Uuid::new_v4(),
            #[cfg(unix)]
            session: self.session.as_ref().unwrap().clone(),
            snapshot,
            action,
        }));
        self.action_queued = false;
        self.poll();
    }
    pub fn act_on(&mut self, change: Change) {
        let action = match change.section {
            Section::Staged => Action::Unstage(vec![change]),
            Section::Unstaged | Section::Untracked => Action::Stage(vec![change]),
            Section::Conflicted => return,
        };
        self.enqueue(action);
    }
    #[cfg(not(target_os = "linux"))]
    pub fn discard(&mut self, expected: &Snapshot, change: Change) {
        if !self.can_act() {
            return;
        }
        let current = self.snapshot.as_ref().unwrap();
        if current.key() != expected.key()
            || current.head != expected.head
            || current.index_fingerprint != expected.index_fingerprint
            || !current.changes.contains(&change)
        {
            self.action_error = Some((
                expected.key(),
                "Repository changed while confirming. Review before discarding again.".into(),
            ));
            return;
        }
        if matches!(change.section, Section::Unstaged | Section::Untracked) {
            self.enqueue(Action::Discard(change));
        }
    }
    pub fn act_section(&mut self, section: Section) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let changes: Vec<_> = snapshot
            .changes
            .iter()
            .filter(|c| c.section == section)
            .cloned()
            .collect();
        if changes.is_empty() {
            return;
        }
        let action = match section {
            Section::Staged => Action::Unstage(changes),
            Section::Unstaged | Section::Untracked => Action::Stage(changes),
            Section::Conflicted => return,
        };
        self.enqueue(action);
    }
    pub fn commit(&mut self) {
        if self.can_commit() {
            self.enqueue(Action::Commit(self.commit_message.clone()));
        }
    }
    pub fn badge(&self, path: &Path) -> Option<&'static str> {
        self.snapshot.as_ref()?.badge(path)
    }
    pub fn message(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        if self.outside {
            return "This directory is outside a Git work tree.".into();
        }
        if self.snapshot.is_none() {
            return "Reading repository…".into();
        }
        if self.loading {
            "Refreshing…".into()
        } else {
            String::new()
        }
    }
}
