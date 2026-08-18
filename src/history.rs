use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::fsutil::{
    atomic_copy, atomic_write_json, atomic_write_json_durable, files_equal, read_json,
    read_json_with_backup, write_new_file,
};

static BLOB_COUNTER: AtomicU64 = AtomicU64::new(0);

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMutation {
    pub path: PathBuf,
    pub contents: Option<Vec<u8>>,
}

#[allow(dead_code)]
impl FileMutation {
    pub fn write(path: impl Into<PathBuf>, contents: impl Into<Vec<u8>>) -> Self {
        Self {
            path: path.into(),
            contents: Some(contents.into()),
        }
    }

    pub fn delete(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            contents: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSet {
    pub id: u64,
    pub files: Vec<FileChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: PathBuf,
    before: Snapshot,
    after: Snapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Snapshot {
    Missing,
    Blob { id: String, len: u64 },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryState {
    pub cursor: u64,
    pub tip: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PendingKind {
    Commit,
    Undo,
    Redo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PendingOperation {
    kind: PendingKind,
    change_set: ChangeSet,
    target: HistoryState,
}

#[derive(Debug)]
pub enum HistoryError {
    Io(io::Error),
    Conflict(Vec<PathBuf>),
    InvalidPath(PathBuf),
    NothingToUndo,
    NothingToRedo,
}

impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Conflict(paths) => {
                write!(f, "file changed outside MatrixCode: ")?;
                for (index, path) in paths.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", path.display())?;
                }
                Ok(())
            }
            Self::InvalidPath(path) => write!(f, "unsafe workspace path: {}", path.display()),
            Self::NothingToUndo => write!(f, "nothing to undo"),
            Self::NothingToRedo => write!(f, "nothing to redo"),
        }
    }
}

impl std::error::Error for HistoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for HistoryError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Debug, Clone)]
pub struct HistoryStore {
    root: PathBuf,
    workspace: PathBuf,
}

impl HistoryStore {
    pub fn open(root: PathBuf, workspace: PathBuf) -> Result<Self, HistoryError> {
        let workspace = fs::canonicalize(workspace)?;
        if !workspace.is_dir() {
            return Err(HistoryError::InvalidPath(workspace));
        }

        let store = Self { root, workspace };
        fs::create_dir_all(store.blobs_dir())?;
        fs::create_dir_all(store.turns_dir())?;
        store.recover_pending()?;
        let state = store.state()?;
        store.prune_after(state.tip)?;
        Ok(store)
    }

    pub fn state(&self) -> Result<HistoryState, HistoryError> {
        let path = self.state_path();
        match read_json_with_backup(&path) {
            Ok(state) => Ok(state),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(HistoryState::default()),
            Err(error) => Err(error.into()),
        }
    }

    #[allow(dead_code)]
    pub fn record_and_apply(
        &self,
        mutations: Vec<FileMutation>,
    ) -> Result<ChangeSet, HistoryError> {
        self.recover_pending()?;
        let state = self.state()?;
        let id = state.cursor.saturating_add(1);
        let mut seen = HashSet::with_capacity(mutations.len());
        let mut normalized = Vec::with_capacity(mutations.len());

        for mutation in mutations {
            let relative = normalize_relative(&mutation.path)?;
            if !seen.insert(relative.clone()) {
                return Err(HistoryError::InvalidPath(relative));
            }
            self.checked_workspace_path(&relative)?;
            normalized.push((relative, mutation.contents));
        }

        let mut files = Vec::with_capacity(normalized.len());
        for (relative, contents) in normalized {
            let target = self.checked_workspace_path(&relative)?;
            let before = match self.capture_current(&target) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    self.cleanup_file_changes(&files);
                    return Err(error);
                }
            };
            let after = match contents {
                Some(contents) => match self.store_bytes(&contents) {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        self.cleanup_snapshot(&before);
                        self.cleanup_file_changes(&files);
                        return Err(error);
                    }
                },
                None => Snapshot::Missing,
            };
            files.push(FileChange {
                path: relative,
                before,
                after,
            });
        }

        let change_set = ChangeSet { id, files };
        let pending = PendingOperation {
            kind: PendingKind::Commit,
            change_set: change_set.clone(),
            target: HistoryState { cursor: id, tip: id },
        };
        self.execute_pending(&pending)?;
        Ok(change_set)
    }

    pub fn undo(&self) -> Result<ChangeSet, HistoryError> {
        self.recover_pending()?;
        let state = self.state()?;
        if state.cursor == 0 {
            return Err(HistoryError::NothingToUndo);
        }
        let change_set = self.read_turn(state.cursor)?;
        let pending = PendingOperation {
            kind: PendingKind::Undo,
            change_set: change_set.clone(),
            target: HistoryState {
                cursor: state.cursor - 1,
                tip: state.tip,
            },
        };
        self.execute_pending(&pending)?;
        Ok(change_set)
    }

    pub fn redo(&self) -> Result<ChangeSet, HistoryError> {
        self.recover_pending()?;
        let state = self.state()?;
        if state.cursor >= state.tip {
            return Err(HistoryError::NothingToRedo);
        }
        let change_set = self.read_turn(state.cursor + 1)?;
        let pending = PendingOperation {
            kind: PendingKind::Redo,
            change_set: change_set.clone(),
            target: HistoryState {
                cursor: state.cursor + 1,
                tip: state.tip,
            },
        };
        self.execute_pending(&pending)?;
        Ok(change_set)
    }

    fn execute_pending(&self, pending: &PendingOperation) -> Result<(), HistoryError> {
        let conflicts = self.conflicts(pending, SnapshotSide::Expected)?;
        if !conflicts.is_empty() {
            return Err(HistoryError::Conflict(conflicts));
        }

        atomic_write_json(&self.pending_path(), pending)?;
        self.apply_desired(pending)?;
        self.finalize_pending(pending)
    }

    fn recover_pending(&self) -> Result<(), HistoryError> {
        let path = self.pending_path();
        let pending: PendingOperation = match read_json(&path) {
            Ok(pending) => pending,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };

        let mut all_expected = true;
        let mut all_desired = true;
        let mut conflicts = Vec::new();
        for change in &pending.change_set.files {
            let target = self.checked_workspace_path(&change.path)?;
            let expected = pending.expected(change);
            let desired = pending.desired(change);
            let matches_expected = self.snapshot_matches(&target, expected)?;
            let matches_desired = self.snapshot_matches(&target, desired)?;
            all_expected &= matches_expected;
            all_desired &= matches_desired;
            if !matches_expected && !matches_desired {
                conflicts.push(change.path.clone());
            }
        }

        if !conflicts.is_empty() {
            return Err(HistoryError::Conflict(conflicts));
        }
        if all_desired {
            return self.finalize_pending(&pending);
        }
        if all_expected {
            if pending.kind == PendingKind::Commit {
                self.cleanup_change_set_blobs(&pending.change_set);
            }
            remove_if_exists(&path)?;
            return Ok(());
        }

        self.apply_desired(&pending)?;
        self.finalize_pending(&pending)
    }

    fn apply_desired(&self, pending: &PendingOperation) -> Result<(), HistoryError> {
        for change in &pending.change_set.files {
            let target = self.checked_workspace_path(&change.path)?;
            let expected = pending.expected(change);
            let desired = pending.desired(change);

            if self.snapshot_matches(&target, desired)? {
                continue;
            }
            if !self.snapshot_matches(&target, expected)? {
                return Err(HistoryError::Conflict(vec![change.path.clone()]));
            }
            self.apply_snapshot(&target, desired)?;
            if !self.snapshot_matches(&target, desired)? {
                return Err(HistoryError::Io(io::Error::other(
                    "filesystem verification failed after change",
                )));
            }
        }
        Ok(())
    }

    fn finalize_pending(&self, pending: &PendingOperation) -> Result<(), HistoryError> {
        let conflicts = self.conflicts(pending, SnapshotSide::Desired)?;
        if !conflicts.is_empty() {
            return Err(HistoryError::Conflict(conflicts));
        }

        let replaced = if pending.kind == PendingKind::Commit {
            read_json::<ChangeSet>(&self.turn_path(pending.change_set.id)).ok()
        } else {
            None
        };
        if pending.kind == PendingKind::Commit {
            atomic_write_json(&self.turn_path(pending.change_set.id), &pending.change_set)?;
        }
        atomic_write_json_durable(&self.state_path(), &pending.target)?;
        remove_if_exists(&self.pending_path())?;
        if pending.kind == PendingKind::Commit {
            if let Some(replaced) = replaced {
                if replaced != pending.change_set {
                    self.cleanup_change_set_blobs(&replaced);
                }
            }
            self.prune_after(pending.target.tip)?;
        }
        Ok(())
    }

    fn conflicts(
        &self,
        pending: &PendingOperation,
        side: SnapshotSide,
    ) -> Result<Vec<PathBuf>, HistoryError> {
        let mut conflicts = Vec::new();
        for change in &pending.change_set.files {
            let target = self.checked_workspace_path(&change.path)?;
            let snapshot = match side {
                SnapshotSide::Expected => pending.expected(change),
                SnapshotSide::Desired => pending.desired(change),
            };
            if !self.snapshot_matches(&target, snapshot)? {
                conflicts.push(change.path.clone());
            }
        }
        Ok(conflicts)
    }

    fn apply_snapshot(&self, target: &Path, snapshot: &Snapshot) -> Result<(), HistoryError> {
        match snapshot {
            Snapshot::Missing => match fs::remove_file(target) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            },
            Snapshot::Blob { id, .. } => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                atomic_copy(&self.blob_path(id), target)?;
                Ok(())
            }
        }
    }

    fn capture_current(&self, target: &Path) -> Result<Snapshot, HistoryError> {
        let metadata = match fs::symlink_metadata(target) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Snapshot::Missing),
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(HistoryError::InvalidPath(target.to_path_buf()));
        }

        let id = new_blob_id();
        let path = self.blob_path(&id);
        fs::copy(target, &path)?;
        let file = fs::OpenOptions::new().read(true).open(&path)?;
        file.sync_all()?;
        Ok(Snapshot::Blob {
            id,
            len: metadata.len(),
        })
    }

    fn store_bytes(&self, bytes: &[u8]) -> Result<Snapshot, HistoryError> {
        let id = new_blob_id();
        write_new_file(&self.blob_path(&id), bytes)?;
        Ok(Snapshot::Blob {
            id,
            len: bytes.len() as u64,
        })
    }

    fn snapshot_matches(&self, target: &Path, snapshot: &Snapshot) -> Result<bool, HistoryError> {
        match snapshot {
            Snapshot::Missing => match fs::symlink_metadata(target) {
                Ok(_) => Ok(false),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
                Err(error) => Err(error.into()),
            },
            Snapshot::Blob { id, len } => {
                if fs::symlink_metadata(target)
                    .map(|metadata| metadata.file_type().is_symlink())
                    .unwrap_or(false)
                {
                    return Ok(false);
                }
                Ok(files_equal(target, &self.blob_path(id), *len)?)
            }
        }
    }

    fn checked_workspace_path(&self, relative: &Path) -> Result<PathBuf, HistoryError> {
        let relative = normalize_relative(relative)?;
        let mut current = self.workspace.clone();
        for component in relative.components() {
            let Component::Normal(component) = component else {
                return Err(HistoryError::InvalidPath(relative));
            };
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(HistoryError::InvalidPath(relative));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(current)
    }

    fn read_turn(&self, id: u64) -> Result<ChangeSet, HistoryError> {
        Ok(read_json_with_backup(&self.turn_path(id))?)
    }

    fn prune_after(&self, tip: u64) -> Result<(), HistoryError> {
        let entries = match fs::read_dir(self.turns_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let Some(id) = parse_turn_id(&entry.file_name().to_string_lossy()) else {
                continue;
            };
            if id <= tip {
                continue;
            }
            if let Ok(change_set) = read_json::<ChangeSet>(&entry.path()) {
                self.cleanup_change_set_blobs(&change_set);
            }
            remove_if_exists(&entry.path())?;
        }
        Ok(())
    }

    fn cleanup_change_set_blobs(&self, change_set: &ChangeSet) {
        self.cleanup_file_changes(&change_set.files);
    }

    fn cleanup_file_changes(&self, changes: &[FileChange]) {
        for change in changes {
            self.cleanup_snapshot(&change.before);
            self.cleanup_snapshot(&change.after);
        }
    }

    fn cleanup_snapshot(&self, snapshot: &Snapshot) {
        if let Snapshot::Blob { id, .. } = snapshot {
            let _ = fs::remove_file(self.blob_path(id));
        }
    }

    fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }

    fn pending_path(&self) -> PathBuf {
        self.root.join("pending.json")
    }

    fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }

    fn blob_path(&self, id: &str) -> PathBuf {
        self.blobs_dir().join(id)
    }

    fn turns_dir(&self) -> PathBuf {
        self.root.join("turns")
    }

    fn turn_path(&self, id: u64) -> PathBuf {
        self.turns_dir().join(format!("{id}.json"))
    }
}

#[derive(Debug, Clone, Copy)]
enum SnapshotSide {
    Expected,
    Desired,
}

impl PendingOperation {
    fn expected<'a>(&self, change: &'a FileChange) -> &'a Snapshot {
        match self.kind {
            PendingKind::Commit | PendingKind::Redo => &change.before,
            PendingKind::Undo => &change.after,
        }
    }

    fn desired<'a>(&self, change: &'a FileChange) -> &'a Snapshot {
        match self.kind {
            PendingKind::Commit | PendingKind::Redo => &change.after,
            PendingKind::Undo => &change.before,
        }
    }
}

fn normalize_relative(path: &Path) -> Result<PathBuf, HistoryError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(HistoryError::InvalidPath(path.to_path_buf()));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => normalized.push(component),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(HistoryError::InvalidPath(path.to_path_buf()));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(HistoryError::InvalidPath(path.to_path_buf()));
    }
    Ok(normalized)
}

fn parse_turn_id(name: &str) -> Option<u64> {
    name.strip_suffix(".json")?.parse().ok()
}

fn new_blob_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = BLOB_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("b-{nanos:x}-{:x}-{counter:x}", process::id())
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
