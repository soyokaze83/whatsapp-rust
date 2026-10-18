//! A SQLite VFS that refuses database paths that pass through a symbolic link.
//!
//! diesel opens connections with fixed flags, so `SQLITE_OPEN_NOFOLLOW` can't
//! be passed through it. This VFS wraps the `unix` VFS and does what that
//! flag does: the unix VFS reports `SQLITE_OK_SYMLINK` when it resolved a link
//! while building the full path, and this VFS turns that into
//! `SQLITE_CANTOPEN_SYMLINK`. Databases attached through a connection opened
//! with this VFS, including `VACUUM INTO` targets, use it too.

use std::ffi::{CStr, c_char, c_int};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, Ordering};

use libsqlite3_sys as ffi;
use wacore::store::error::{Result, StoreError};

const VFS_NAME: &CStr = c"whatsapp-rust-nofollow";

static BASE_VFS: AtomicPtr<ffi::sqlite3_vfs> = AtomicPtr::new(std::ptr::null_mut());
static REGISTERED: OnceLock<std::result::Result<(), c_int>> = OnceLock::new();

/// Registers the VFS once per process. It never becomes the default VFS.
pub(crate) fn register() -> Result<()> {
    REGISTERED.get_or_init(register_once).map_err(|code| {
        StoreError::InvalidConfig(format!("SQLite VFS registration failed: {code}"))
    })
}

fn register_once() -> std::result::Result<(), c_int> {
    // SAFETY: sqlite3_initialize is safe to call at any time and from any thread.
    let rc = unsafe { ffi::sqlite3_initialize() };
    if rc != ffi::SQLITE_OK {
        return Err(rc);
    }
    // Asked for by name: only the unix VFS reports SQLITE_OK_SYMLINK.
    // SAFETY: the name is a valid C string. SQLite returns a pointer it owns
    // for the life of the process, or null.
    let base = unsafe { ffi::sqlite3_vfs_find(c"unix".as_ptr()) };
    if base.is_null() {
        return Err(ffi::SQLITE_ERROR);
    }
    // SAFETY: `base` is a valid, registered VFS that SQLite never frees. The
    // copy keeps its `pAppData` and file size, which the unix VFS methods read
    // from the VFS they're called with.
    let mut vfs = unsafe { std::ptr::read(base) };
    vfs.zName = VFS_NAME.as_ptr();
    vfs.pNext = std::ptr::null_mut();
    vfs.xFullPathname = Some(full_pathname);
    BASE_VFS.store(base, Ordering::Release);
    // SQLite keeps the pointer for the rest of the process, so the VFS is leaked.
    let vfs = Box::into_raw(Box::new(vfs));
    // SAFETY: `vfs` is a valid VFS that is never freed or moved.
    let rc = unsafe { ffi::sqlite3_vfs_register(vfs, 0) };
    if rc == ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(rc)
    }
}

unsafe extern "C" fn full_pathname(
    _vfs: *mut ffi::sqlite3_vfs,
    path: *const c_char,
    out_len: c_int,
    out: *mut c_char,
) -> c_int {
    let base = BASE_VFS.load(Ordering::Acquire);
    // SAFETY: SQLite only calls this after registration stored `base`, which
    // stays valid for the life of the process.
    let Some(resolve) = (unsafe { base.as_ref() }).and_then(|base| base.xFullPathname) else {
        return ffi::SQLITE_ERROR;
    };
    // SAFETY: the path and buffer are the ones SQLite passed for this call.
    // `base` stands in for this VFS, which is a copy of it with only the name
    // and this method replaced.
    match unsafe { resolve(base, path, out_len, out) } {
        ffi::SQLITE_OK_SYMLINK => ffi::SQLITE_CANTOPEN_SYMLINK,
        rc => rc,
    }
}

pub(crate) fn database_uri(path: &str) -> String {
    let mut uri = String::from(if path.starts_with('/') {
        "file://"
    } else {
        "file:"
    });
    for c in path.chars() {
        match c {
            '%' => uri.push_str("%25"),
            '?' => uri.push_str("%3F"),
            '#' => uri.push_str("%23"),
            c => uri.push(c),
        }
    }
    uri.push_str("?vfs=");
    uri.push_str(&VFS_NAME.to_string_lossy());
    uri
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use wacore::store::traits::*;

    use crate::SqliteStore;

    /// A private directory with no link anywhere in its path.
    struct ResolvedDir(PathBuf);

    impl ResolvedDir {
        fn new() -> Self {
            use portable_atomic::AtomicU64;
            use std::sync::atomic::Ordering;
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let base = std::fs::canonicalize(std::env::temp_dir()).expect("resolve temp dir");
            let dir = base.join(format!("nofollow_test_{}_{}", std::process::id(), id));
            std::fs::create_dir(&dir).expect("create test dir");
            Self(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for ResolvedDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn text(path: &Path) -> &str {
        path.to_str().expect("UTF-8 test path")
    }

    #[tokio::test]
    async fn opens_and_reopens_a_database_without_links() {
        let dir = ResolvedDir::new();
        let database = dir.path("weird ?#% name.db");

        let store = SqliteStore::new_nofollow(text(&database))
            .await
            .expect("open");
        store.create().await.expect("create device");
        drop(store);

        let reopened = SqliteStore::new_nofollow(text(&database))
            .await
            .expect("reopen");
        assert!(reopened.load().await.expect("load").is_some());
        assert!(database.is_file());
    }

    #[tokio::test]
    async fn refuses_a_link_at_the_database_path_and_leaves_its_target_untouched() {
        let dir = ResolvedDir::new();
        let target = dir.path("target.db");
        drop(
            SqliteStore::new_nofollow(text(&target))
                .await
                .expect("create target"),
        );
        let before = std::fs::read(&target).expect("read target");
        let link = dir.path("protocol.db");
        std::os::unix::fs::symlink(&target, &link).expect("link");

        assert!(SqliteStore::new_nofollow(text(&link)).await.is_err());
        assert_eq!(std::fs::read(&target).expect("read target"), before);
        assert!(!dir.path("target.db-wal").exists());
    }

    #[tokio::test]
    async fn refuses_a_dangling_link_at_the_database_path_without_creating_its_target() {
        let dir = ResolvedDir::new();
        let target = dir.path("missing.db");
        let link = dir.path("protocol.db");
        std::os::unix::fs::symlink(&target, &link).expect("link");

        assert!(SqliteStore::new_nofollow(text(&link)).await.is_err());
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn refuses_a_link_in_a_directory_of_the_database_path() {
        let dir = ResolvedDir::new();
        let real = dir.path("real");
        std::fs::create_dir(&real).expect("create dir");
        let linked = dir.path("linked");
        std::os::unix::fs::symlink(&real, &linked).expect("link");

        assert!(
            SqliteStore::new_nofollow(text(&linked.join("protocol.db")))
                .await
                .is_err()
        );
        assert!(!real.join("protocol.db").exists());
    }

    #[tokio::test]
    async fn snapshot_refuses_a_link_at_its_target_and_leaves_its_target_untouched() {
        let dir = ResolvedDir::new();
        let database = dir.path("protocol.db");
        let store = SqliteStore::new_nofollow(text(&database))
            .await
            .expect("open");
        store.create().await.expect("create device");
        let target = dir.path("elsewhere");
        // The snapshot name holds the current second, so cover every second it may use.
        let now = wacore::time::now_secs();
        for timestamp in now..now + 30 {
            let name = format!("{}.snapshot-{timestamp}-label", text(&database));
            std::os::unix::fs::symlink(&target, name).expect("link");
        }

        assert!(store.snapshot_db("label", None).await.is_err());
        assert!(!target.exists());

        std::fs::write(&target, b"").expect("write empty target");
        assert!(store.snapshot_db("label", None).await.is_err());
        assert_eq!(std::fs::read(&target).expect("read target"), b"");
    }

    #[tokio::test]
    async fn snapshot_refuses_a_link_at_its_sidecar_and_leaves_its_target_untouched() {
        let dir = ResolvedDir::new();
        let database = dir.path("protocol.db");
        let store = SqliteStore::new_nofollow(text(&database))
            .await
            .expect("open");
        store.create().await.expect("create device");
        let target = dir.path("elsewhere");
        let now = wacore::time::now_secs();
        for timestamp in now..now + 30 {
            let name = format!("{}.snapshot-{timestamp}-label.json", text(&database));
            std::os::unix::fs::symlink(&target, name).expect("link");
        }

        assert!(store.snapshot_db("label", Some(b"{}")).await.is_err());
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn snapshot_writes_a_reopenable_copy_without_links() {
        let dir = ResolvedDir::new();
        let database = dir.path("weird ?#% name.db");
        let store = SqliteStore::new_nofollow(text(&database))
            .await
            .expect("open");
        store.create().await.expect("create device");

        store
            .snapshot_db("label", Some(b"{}"))
            .await
            .expect("snapshot");

        let prefix = format!("{}.snapshot-", text(&database));
        let snapshot = std::fs::read_dir(&dir.0)
            .expect("list dir")
            .map(|entry| entry.expect("entry").path())
            .find(|path| text(path).starts_with(&prefix) && text(path).ends_with("-label"))
            .expect("snapshot file");
        let restored = SqliteStore::new_nofollow(text(&snapshot))
            .await
            .expect("open snapshot");
        assert!(restored.load().await.expect("load").is_some());
        assert_eq!(
            std::fs::read(format!("{}.json", text(&snapshot))).expect("sidecar"),
            b"{}"
        );
    }
}
