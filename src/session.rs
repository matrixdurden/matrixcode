use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRecord {
    Message { role: MessageRole, content: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub id: String,
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub workspace: PathBuf,
    pub provider: Option<String>,
    pub account: Option<String>,
    pub model: Option<String>,
    pub history_cursor: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSession {
    pub metadata: SessionMetadata,
    pub records: Vec<SessionRecord>,
    pub corrupted_records: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionList {
    pub sessions: Vec<SessionMetadata>,
    pub skipped_corrupt: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    pub fn discover() -> io::Result<Self> {
        Ok(Self::new(data_dir()?.join("sessions")))
    }

    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn create(&self, workspace: PathBuf) -> io::Result<SessionMetadata> {
        fs::create_dir_all(&self.root)?;

        let session_dir = loop {
            let id = new_session_id();
            let path = self.root.join(&id);
            match fs::create_dir(&path) {
                Ok(()) => break (id, path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };

        let now = now_millis();
        let metadata = SessionMetadata {
            id: session_dir.0,
            title: "New session".to_owned(),
            created_at: now,
            updated_at: now,
            workspace,
            provider: None,
            account: None,
            model: None,
            history_cursor: 0,
        };

        if let Err(error) = write_metadata(&session_dir.1, &metadata) {
            let _ = fs::remove_dir_all(&session_dir.1);
            return Err(error);
        }

        Ok(metadata)
    }

    pub fn list(&self) -> io::Result<SessionList> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(SessionList {
                    sessions: Vec::new(),
                    skipped_corrupt: 0,
                });
            }
            Err(error) => return Err(error),
        };

        let mut sessions = Vec::new();
        let mut skipped_corrupt = 0;

        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            match read_metadata(&entry.path()) {
                Ok(metadata) => sessions.push(metadata),
                Err(_) => skipped_corrupt += 1,
            }
        }

        sessions.sort_unstable_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| right.id.cmp(&left.id))
        });

        Ok(SessionList {
            sessions,
            skipped_corrupt,
        })
    }

    pub fn load(&self, id: &str) -> io::Result<LoadedSession> {
        validate_session_id(id)?;
        let session_dir = self.root.join(id);
        let metadata = read_metadata(&session_dir)?;
        let records_path = session_dir.join("records.jsonl");

        let file = match File::open(&records_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(LoadedSession {
                    metadata,
                    records: Vec::new(),
                    corrupted_records: 0,
                });
            }
            Err(error) => return Err(error),
        };

        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut records = Vec::new();
        let mut corrupted_records = 0;

        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let record = line.trim_end_matches(&['\r', '\n'][..]);
            if record.is_empty() {
                continue;
            }
            match serde_json::from_str(record) {
                Ok(record) => records.push(record),
                Err(_) => corrupted_records += 1,
            }
        }

        Ok(LoadedSession {
            metadata,
            records,
            corrupted_records,
        })
    }

    pub fn append_message(&self, id: &str, role: MessageRole, content: &str) -> io::Result<()> {
        validate_session_id(id)?;
        let session_dir = self.root.join(id);
        if !session_dir.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "session does not exist",
            ));
        }

        #[derive(Serialize)]
        struct MessageRecord<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            role: MessageRole,
            content: &'a str,
        }

        let record = MessageRecord {
            kind: "message",
            role,
            content,
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(session_dir.join("records.jsonl"))?;
        serde_json::to_writer(&mut file, &record).map_err(json_error)?;
        file.write_all(b"\n")?;
        file.flush()
    }

    pub fn save_metadata(&self, metadata: &SessionMetadata) -> io::Result<()> {
        validate_session_id(&metadata.id)?;
        let session_dir = self.root.join(&metadata.id);
        if !session_dir.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "session does not exist",
            ));
        }
        write_metadata(&session_dir, metadata)
    }

    pub fn touch(&self, metadata: &mut SessionMetadata) -> io::Result<()> {
        metadata.updated_at = now_millis().max(metadata.updated_at);
        self.save_metadata(metadata)
    }

    pub fn delete(&self, id: &str) -> io::Result<()> {
        validate_session_id(id)?;
        match fs::remove_dir_all(self.root.join(id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn write_metadata(session_dir: &Path, metadata: &SessionMetadata) -> io::Result<()> {
    let bytes = serde_json::to_vec(metadata).map_err(json_error)?;
    atomic_write(&session_dir.join("meta.json"), &bytes)
}

fn read_metadata(session_dir: &Path) -> io::Result<SessionMetadata> {
    let primary = session_dir.join("meta.json");
    match read_metadata_file(&primary) {
        Ok(metadata) => Ok(metadata),
        Err(primary_error) => {
            let backup = backup_path(&primary);
            match read_metadata_file(&backup) {
                Ok(metadata) => Ok(metadata),
                Err(_) => Err(primary_error),
            }
        }
    }
}

fn read_metadata_file(path: &Path) -> io::Result<SessionMetadata> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(json_error)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid file name"))?;
    let temp = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        process::id(),
        SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let backup = backup_path(path);

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    if path.exists() {
        match fs::remove_file(&backup) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                let _ = fs::remove_file(&temp);
                return Err(error);
            }
        }
        if let Err(error) = fs::rename(path, &backup) {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
    }

    if let Err(error) = fs::rename(&temp, path) {
        if backup.exists() && !path.exists() {
            let _ = fs::rename(&backup, path);
        }
        let _ = fs::remove_file(&temp);
        return Err(error);
    }

    let _ = fs::remove_file(&backup);
    sync_parent(parent);
    Ok(())
}

#[cfg(unix)]
fn sync_parent(parent: &Path) {
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) {}

fn backup_path(path: &Path) -> PathBuf {
    path.with_extension("bak")
}

fn validate_session_id(id: &str) -> io::Result<()> {
    if !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid session id",
    ))
}

fn new_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("s-{nanos:x}-{:x}-{counter:x}", process::id())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(target_os = "windows")]
fn data_dir() -> io::Result<PathBuf> {
    env_path("LOCALAPPDATA").map(|path| path.join("MatrixCode"))
}

#[cfg(target_os = "macos")]
fn data_dir() -> io::Result<PathBuf> {
    env_path("HOME").map(|path| path.join("Library/Application Support/MatrixCode"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn data_dir() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path).join("matrixcode"));
    }
    env_path("HOME").map(|path| path.join(".local/share/matrixcode"))
}

#[cfg(not(any(unix, target_os = "windows")))]
fn data_dir() -> io::Result<PathBuf> {
    env_path("HOME").map(|path| path.join(".matrixcode"))
}

fn env_path(name: &str) -> io::Result<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("environment variable {name} is not set"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = env::temp_dir().join(format!(
                "matrixcode-test-{}-{}",
                process::id(),
                SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create test dir");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn persists_and_loads_session_content() {
        let temp = TestDir::new();
        let store = SessionStore::new(temp.0.join("sessions"));
        let metadata = store
            .create(PathBuf::from("/workspace"))
            .expect("create session");
        store
            .append_message(&metadata.id, MessageRole::User, "hello")
            .expect("append message");

        let loaded = store.load(&metadata.id).expect("load session");
        assert_eq!(loaded.metadata.id, metadata.id);
        assert_eq!(loaded.corrupted_records, 0);
        assert_eq!(
            loaded.records,
            vec![SessionRecord::Message {
                role: MessageRole::User,
                content: "hello".to_owned(),
            }]
        );
    }

    #[test]
    fn list_reads_metadata_without_parsing_session_content() {
        let temp = TestDir::new();
        let store = SessionStore::new(temp.0.join("sessions"));
        let metadata = store
            .create(PathBuf::from("/workspace"))
            .expect("create session");
        fs::write(
            store.root.join(&metadata.id).join("records.jsonl"),
            b"not-json\n",
        )
        .expect("write corrupt records");

        let listed = store.list().expect("list sessions");
        assert_eq!(listed.sessions.len(), 1);
        assert_eq!(listed.skipped_corrupt, 0);
    }

    #[test]
    fn load_recovers_valid_records_around_corruption() {
        let temp = TestDir::new();
        let store = SessionStore::new(temp.0.join("sessions"));
        let metadata = store
            .create(PathBuf::from("/workspace"))
            .expect("create session");
        let records = store.root.join(&metadata.id).join("records.jsonl");
        fs::write(
            records,
            b"{\"type\":\"message\",\"role\":\"user\",\"content\":\"one\"}\nBROKEN\n{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"two\"}\n",
        )
        .expect("write records");

        let loaded = store.load(&metadata.id).expect("load session");
        assert_eq!(loaded.records.len(), 2);
        assert_eq!(loaded.corrupted_records, 1);
    }

    #[test]
    fn metadata_falls_back_to_backup_when_primary_is_corrupt() {
        let temp = TestDir::new();
        let store = SessionStore::new(temp.0.join("sessions"));
        let metadata = store
            .create(PathBuf::from("/workspace"))
            .expect("create session");
        let primary = store.root.join(&metadata.id).join("meta.json");
        let backup = backup_path(&primary);
        fs::copy(&primary, &backup).expect("copy backup");
        fs::write(&primary, b"{").expect("corrupt primary");

        let loaded = store.load(&metadata.id).expect("recover metadata");
        assert_eq!(loaded.metadata, metadata);
    }

    #[test]
    fn delete_is_idempotent() {
        let temp = TestDir::new();
        let store = SessionStore::new(temp.0.join("sessions"));
        let metadata = store
            .create(PathBuf::from("/workspace"))
            .expect("create session");
        store.delete(&metadata.id).expect("delete session");
        store.delete(&metadata.id).expect("delete session again");
        assert!(store.list().expect("list sessions").sessions.is_empty());
    }

    #[test]
    fn rejects_path_traversal_ids() {
        let temp = TestDir::new();
        let store = SessionStore::new(temp.0.join("sessions"));
        let error = store.load("../escape").expect_err("reject traversal");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
