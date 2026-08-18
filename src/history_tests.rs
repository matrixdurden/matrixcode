use std::fs;
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::history::{FileMutation, HistoryError, HistoryState, HistoryStore};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "matrixcode-history-test-{}-{}",
            process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create test dir");
        Self(path)
    }

    fn store(&self) -> (HistoryStore, PathBuf) {
        let workspace = self.0.join("workspace");
        let history = self.0.join("history");
        fs::create_dir_all(&workspace).expect("create workspace");
        let store = HistoryStore::open(history, workspace.clone()).expect("open history");
        (store, workspace)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn commit_undo_and_redo_restore_exact_bytes() {
    let temp = TestDir::new();
    let (store, workspace) = temp.store();
    let target = workspace.join("src/auth.rs");
    fs::create_dir_all(target.parent().expect("parent")).expect("create src");
    fs::write(&target, b"before\n").expect("write before");

    store
        .record_and_apply(vec![FileMutation::write(
            "src/auth.rs",
            b"after\n".to_vec(),
        )])
        .expect("commit change");
    assert_eq!(fs::read(&target).expect("read after"), b"after\n");
    assert_eq!(store.state().expect("state").cursor, 1);

    store.undo().expect("undo");
    assert_eq!(fs::read(&target).expect("read before"), b"before\n");
    assert_eq!(store.state().expect("state").cursor, 0);

    store.redo().expect("redo");
    assert_eq!(fs::read(&target).expect("read after redo"), b"after\n");
    assert_eq!(store.state().expect("state").cursor, 1);
}

#[test]
fn undo_conflict_never_overwrites_user_edit() {
    let temp = TestDir::new();
    let (store, workspace) = temp.store();
    let target = workspace.join("auth.rs");
    fs::write(&target, b"before").expect("write before");
    store
        .record_and_apply(vec![FileMutation::write("auth.rs", b"agent".to_vec())])
        .expect("commit");
    fs::write(&target, b"user edit").expect("user edit");

    let error = store.undo().expect_err("undo must conflict");
    assert!(matches!(error, HistoryError::Conflict(_)));
    assert_eq!(fs::read(&target).expect("read user edit"), b"user edit");
    assert_eq!(store.state().expect("state").cursor, 1);
}

#[test]
fn multi_file_conflict_aborts_before_touching_any_file() {
    let temp = TestDir::new();
    let (store, workspace) = temp.store();
    let a = workspace.join("a.txt");
    let b = workspace.join("b.txt");
    fs::write(&a, b"a0").expect("write a");
    fs::write(&b, b"b0").expect("write b");
    store
        .record_and_apply(vec![
            FileMutation::write("a.txt", b"a1".to_vec()),
            FileMutation::write("b.txt", b"b1".to_vec()),
        ])
        .expect("commit");
    fs::write(&b, b"manual").expect("manual b");

    assert!(matches!(store.undo(), Err(HistoryError::Conflict(_))));
    assert_eq!(fs::read(&a).expect("read a"), b"a1");
    assert_eq!(fs::read(&b).expect("read b"), b"manual");
}

#[test]
fn redo_is_invalidated_by_new_commit_after_undo() {
    let temp = TestDir::new();
    let (store, workspace) = temp.store();
    let target = workspace.join("file.txt");
    fs::write(&target, b"zero").expect("write zero");
    store
        .record_and_apply(vec![FileMutation::write("file.txt", b"one".to_vec())])
        .expect("commit one");
    store.undo().expect("undo");
    store
        .record_and_apply(vec![FileMutation::write("file.txt", b"two".to_vec())])
        .expect("commit two");

    assert!(matches!(store.redo(), Err(HistoryError::NothingToRedo)));
    assert_eq!(fs::read(&target).expect("read two"), b"two");
    assert_eq!(
        store.state().expect("state"),
        HistoryState { cursor: 1, tip: 1 }
    );
}

#[test]
fn undo_and_redo_handle_created_files() {
    let temp = TestDir::new();
    let (store, workspace) = temp.store();
    let target = workspace.join("created.txt");
    store
        .record_and_apply(vec![FileMutation::write(
            "created.txt",
            b"created".to_vec(),
        )])
        .expect("create file");
    assert_eq!(fs::read(&target).expect("read created"), b"created");
    store.undo().expect("undo create");
    assert!(!target.exists());
    store.redo().expect("redo create");
    assert_eq!(fs::read(&target).expect("read recreated"), b"created");
}

#[test]
fn undo_restores_deleted_file() {
    let temp = TestDir::new();
    let (store, workspace) = temp.store();
    let target = workspace.join("delete.txt");
    fs::write(&target, b"keep me").expect("write target");
    store
        .record_and_apply(vec![FileMutation::delete("delete.txt")])
        .expect("delete");
    assert!(!target.exists());
    store.undo().expect("undo delete");
    assert_eq!(fs::read(&target).expect("restored"), b"keep me");
}

#[test]
fn rejects_parent_paths_and_symlinks() {
    let temp = TestDir::new();
    let (store, workspace) = temp.store();
    assert!(matches!(
        store.record_and_apply(vec![FileMutation::write("../escape", b"x".to_vec())]),
        Err(HistoryError::InvalidPath(_))
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let outside = temp.0.join("outside");
        fs::create_dir_all(&outside).expect("outside");
        symlink(&outside, workspace.join("link")).expect("symlink");
        assert!(matches!(
            store.record_and_apply(vec![FileMutation::write("link/file", b"x".to_vec())]),
            Err(HistoryError::InvalidPath(_))
        ));
    }
}
