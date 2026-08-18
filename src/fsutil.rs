use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde::de::DeserializeOwned;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(json_error)?;
    atomic_replace(path, false, |file| file.write_all(&bytes))
}

pub fn atomic_write_json_durable<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(json_error)?;
    atomic_replace(path, true, |file| file.write_all(&bytes))
}

pub fn read_json_with_backup<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    match read_json(path) {
        Ok(value) => Ok(value),
        Err(primary_error) => match read_json(&backup_path(path)) {
            Ok(value) => Ok(value),
            Err(_) => Err(primary_error),
        },
    }
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(json_error)
}

pub fn atomic_copy(source: &Path, destination: &Path) -> io::Result<()> {
    atomic_replace(destination, false, |target| {
        let mut source = File::open(source)?;
        io::copy(&mut source, target)?;
        Ok(())
    })
}

pub fn write_new_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = parent(path)?;
    fs::create_dir_all(parent)?;
    let temp = temp_path(path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;

    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    drop(file);

    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    sync_parent(parent);
    Ok(())
}

fn atomic_replace<F>(path: &Path, keep_backup: bool, write: F) -> io::Result<()>
where
    F: FnOnce(&mut File) -> io::Result<()>,
{
    let parent = parent(path)?;
    fs::create_dir_all(parent)?;
    let temp = temp_path(path);
    let backup = backup_path(path);

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    if let Err(error) = write(&mut file).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    drop(file);

    let had_previous = path.exists();
    if had_previous {
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

    if keep_backup {
        if !had_previous {
            let _ = fs::copy(path, &backup);
        }
    } else {
        let _ = fs::remove_file(&backup);
    }
    sync_parent(parent);
    Ok(())
}

pub fn backup_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("matrixcode");
    parent.join(format!(".{name}.bak"))
}

fn temp_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("matrixcode");
    parent.join(format!(
        ".{name}.tmp-{}-{}",
        process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn parent(path: &Path) -> io::Result<&Path> {
    path.parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))
}

#[cfg(unix)]
fn sync_parent(parent: &Path) {
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) {}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

pub fn files_equal(left: &Path, right: &Path, expected_len: u64) -> io::Result<bool> {
    let left_meta = match fs::metadata(left) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let right_meta = fs::metadata(right)?;
    if !left_meta.is_file()
        || !right_meta.is_file()
        || left_meta.len() != expected_len
        || right_meta.len() != expected_len
    {
        return Ok(false);
    }

    let mut left = File::open(left)?;
    let mut right = File::open(right)?;
    let mut left_buf = [0_u8; 64 * 1024];
    let mut right_buf = [0_u8; 64 * 1024];
    loop {
        let left_read = left.read(&mut left_buf)?;
        let right_read = right.read(&mut right_buf)?;
        if left_read != right_read || left_buf[..left_read] != right_buf[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "matrixcode-fsutil-test-{}-{}",
                process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
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
    fn durable_json_keeps_recovery_copy() {
        let temp = TestDir::new();
        let path = temp.0.join("state.json");
        atomic_write_json_durable(&path, &41_u64).expect("write state");
        assert!(backup_path(&path).exists());
        fs::write(&path, b"broken").expect("corrupt primary");
        assert_eq!(read_json_with_backup::<u64>(&path).expect("recover"), 41);
    }

    #[test]
    fn atomic_copy_does_not_leave_backup_in_workspace() {
        let temp = TestDir::new();
        let source = temp.0.join("source");
        let destination = temp.0.join("destination");
        fs::write(&source, b"new").expect("source");
        fs::write(&destination, b"old").expect("destination");
        atomic_copy(&source, &destination).expect("atomic copy");
        assert_eq!(fs::read(&destination).expect("read destination"), b"new");
        assert!(!backup_path(&destination).exists());
    }

    #[test]
    fn backup_names_include_full_file_name() {
        let temp = TestDir::new();
        let json = backup_path(&temp.0.join("same.json"));
        let text = backup_path(&temp.0.join("same.txt"));
        assert_ne!(json, text);
    }
}
