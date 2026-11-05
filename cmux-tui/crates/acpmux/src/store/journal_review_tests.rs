//! Repro tests of the delta review 3 of #35866 at `7f3e036d277` (`r-durability-rsync-reviewer`).
//! Branch `fix/r-durability-rsync-review-inode-reuse-nomerge`; never merged.
//!
//! The checkpoint's write-back (`rewrite_verified`) writes the bytes the open verified over a log
//! segment that it finds at the same path with the same file id (inode) and at least the range's
//! length. An inode number is not an identity: a file system that hands a freed inode out again
//! (ext4, XFS) gives a new file at the same path the old number, and Windows checks nothing.

use super::*;
use cmux_journal::{Crash, SimStorage};
use serde_json::json;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::sync::Arc;
use std::time::Duration;

const CHAT: &str = "chat-a";
const WAIT: Duration = Duration::from_secs(10);

fn config() -> StoreConfig {
    StoreConfig { journal: true, ..StoreConfig::default() }
}

fn journal_config() -> cmux_journal::Config {
    cmux_journal::Config {
        retry_min: Duration::from_millis(1),
        retry_max: Duration::from_millis(5),
        ..cmux_journal::Config::default()
    }
}

fn open(home: &Path, journal: &SimStorage) -> Box<dyn Store> {
    open_with_journal(&config(), home, Arc::new(journal.clone()), journal_config())
        .unwrap_or_else(|e| panic!("open the store: {e:#}"))
}

fn record(seq: u64, kind: &str) -> EventRecord {
    EventRecord {
        seq,
        at: 1_000 + seq,
        dir: "mux".into(),
        kind: kind.into(),
        msg: json!({ "n": seq }),
        host_seq: None,
    }
}

fn meta(id: &str, name: &str) -> SessionMeta {
    serde_json::from_value(json!({
        "schema": META_SCHEMA,
        "id": id,
        "name": name,
        "harness": "claude",
        "family": "claude",
        "cwd": "/tmp",
        "status": "idle",
        "createdAt": 1,
        "updatedAt": 1,
        "lastSeq": 0,
    }))
    .unwrap_or_else(|e| panic!("a test meta: {e}"))
}

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
}

/// Gives a folder its mode 0o700 back when dropped, also when the test panics (so the temporary
/// home can be removed).
struct Writable(PathBuf);

impl Drop for Writable {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
    }
}

/// Whether this user can still make a file in a folder of mode 0o500 (root can).
fn mode_stops_writes(folder: &Path) -> bool {
    set_mode(folder, 0o500);
    let probe = folder.join("rsync-review-probe");
    let made = std::fs::File::create(&probe).is_ok();
    let _ = std::fs::remove_file(&probe);
    set_mode(folder, 0o700);
    !made
}

/// chat-a replayed after a kill on the open's fast path (its log's range from line 2 to line 5
/// verified at the open, `Order::read_segments`), with every checkpoint held off before the
/// write-back: chat-b's folder takes no new file, so its overlay image cannot be written and the
/// checkpoint fails in `project_metas` (no freeze; the verified ranges stay listed). Returns the
/// store, chat-b's folder guard, chat-a's first segment and that segment's inode.
fn replayed_chat_with_the_write_back_pending(
    home: &Path,
    seed: u64,
) -> (Box<dyn Store>, SimStorage, Writable, PathBuf, u64) {
    let journal = SimStorage::new(seed);
    let store = open(home, &journal);
    store.append(CHAT, &record(1, "created")).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(store.wait_durable(WAIT).unwrap_or_else(|e| panic!("{e:#}")), "never durable");
    store.flush().unwrap_or_else(|e| panic!("{e:#}"));
    for seq in 2..=5 {
        store.append(CHAT, &record(seq, "queued")).unwrap_or_else(|e| panic!("{e:#}"));
    }
    store.save(&meta("chat-b", "renamed")).unwrap_or_else(|e| panic!("{e:#}"));
    assert!(store.wait_durable(WAIT).unwrap_or_else(|e| panic!("{e:#}")), "never durable");
    let killed = journal.restart(Crash::Process);
    drop(store);
    let folder_b = home.join("sessions").join("chat-b");
    set_mode(&folder_b, 0o500);
    let guard = Writable(folder_b);
    let store = open(home, &killed);
    let held = store.flush();
    assert!(held.is_err(), "the checkpoint went on although chat-b's image cannot be written");
    let segment = home.join("sessions").join(CHAT).join("events").join("000001.ndjson");
    let ino = std::fs::metadata(&segment).unwrap_or_else(|e| panic!("{e}")).ino();
    (store, killed, guard, segment, ino)
}

/// chat-a made again under the same id (a fixed id, as the trigger bridge mints): eight lines,
/// more bytes than the old chat's verified range ends at. Returns its log's bytes.
fn make_the_chat_again(store: &dyn Store, segment: &Path) -> Vec<u8> {
    for seq in 1..=8 {
        store.append(CHAT, &record(seq, "made-again")).unwrap_or_else(|e| panic!("{e:#}"));
    }
    assert!(store.wait_durable(WAIT).unwrap_or_else(|e| panic!("{e:#}")), "never durable");
    std::fs::read(segment).unwrap_or_else(|e| panic!("{e}"))
}

/// Delta review 3 of #35866, D3, with a stand-in for a reused inode: the deleted chat's segment
/// is kept alive by a second link and linked back as the new chat's first segment, emptied. So
/// the new chat's file has the old file id (as a file system that hands the freed inode out again
/// gives it) and its own content. The checkpoint must not write the deleted chat's bytes into it.
#[test]
fn rsync_review5_a_deleted_chats_write_back_never_lands_in_a_new_chats_log() {
    let home = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    assert!(
        mode_stops_writes(home.path()),
        "this test needs a user that a folder's mode stops (not root)"
    );
    let (store, _journal, guard, segment, ino) =
        replayed_chat_with_the_write_back_pending(home.path(), 111);
    let kept = home.path().join("kept-old-segment");
    std::fs::hard_link(&segment, &kept).unwrap_or_else(|e| panic!("{e}"));
    // chat-a is deleted (a purge) while the open's write-back is still pending...
    store.delete(CHAT).unwrap_or_else(|e| panic!("{e:#}"));
    // ...and made again: its first segment gets the old file id (the stand-in), empty.
    let events = home.path().join("sessions").join(CHAT).join("events");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&events)
        .unwrap_or_else(|e| panic!("{e}"));
    std::fs::hard_link(&kept, &segment).unwrap_or_else(|e| panic!("{e}"));
    std::fs::remove_file(&kept).unwrap_or_else(|e| panic!("{e}"));
    std::fs::OpenOptions::new()
        .write(true)
        .open(&segment)
        .and_then(|file| file.set_len(0))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(std::fs::metadata(&segment).unwrap_or_else(|e| panic!("{e}")).ino(), ino);
    let before = make_the_chat_again(store.as_ref(), &segment);
    // chat-b's folder takes files again: the next checkpoint goes on, with its write-back.
    drop(guard);
    store.flush().unwrap_or_else(|e| panic!("{e:#}"));
    let after = std::fs::read(&segment).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        after == before,
        "the checkpoint wrote the deleted chat's verified bytes into the new chat's log (another file with the same file id):\nbefore: {}\nafter:  {}",
        String::from_utf8_lossy(&before),
        String::from_utf8_lossy(&after)
    );
}

/// The file system type at `path`, by name where known.
#[cfg(target_os = "linux")]
fn fs_type(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return "unknown".into();
    };
    // SAFETY: an all-zero statfs is a valid value for the call to fill.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and a buffer the call fills.
    if unsafe { libc::statfs(c_path.as_ptr(), &mut buf) } != 0 {
        return "unknown".into();
    }
    #[allow(clippy::unnecessary_cast)]
    let kind = buf.f_type as i64;
    match kind {
        0xEF53 => "ext4".into(),
        0x0102_1994 => "tmpfs".into(),
        0x5846_5342 => "xfs".into(),
        0x9123_683E => "btrfs".into(),
        0x794C_7630 => "overlayfs".into(),
        other => format!("{other:#x}"),
    }
}

/// D3 with a real reused inode, no second link (Linux): chat-a is deleted, which frees its
/// segment's inode, and made again; every free inode below the old one in that group is taken by
/// a filler first, so the new chat's first segment gets the freed inode, as ext4 hands out the
/// lowest free inode of the folder's group. The home is on the disk (`/var/tmp`), not a tmpfs.
/// When the file system does not give the inode out again, the test says so (inconclusive).
#[cfg(target_os = "linux")]
#[test]
fn rsync_review5_a_reused_inode_never_takes_a_deleted_chats_bytes() {
    use std::os::unix::fs::OpenOptionsExt;
    let home = tempfile::Builder::new()
        .prefix("rsync-review5-")
        .tempdir_in("/var/tmp")
        .unwrap_or_else(|e| panic!("tempdir: {e}"));
    let fs = fs_type(home.path());
    assert!(
        mode_stops_writes(home.path()),
        "this test needs a user that a folder's mode stops (not root)"
    );
    let (store, _journal, guard, segment, ino) =
        replayed_chat_with_the_write_back_pending(home.path(), 112);
    store.delete(CHAT).unwrap_or_else(|e| panic!("{e:#}"));
    let events = home.path().join("sessions").join(CHAT).join("events");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&events)
        .unwrap_or_else(|e| panic!("{e}"));
    let mut fillers = Vec::new();
    let mut freed_is_next = false;
    let mut last = 0;
    for n in 0..30_000u32 {
        let path = events.join(format!(".filler-{n}"));
        std::fs::File::create(&path).unwrap_or_else(|e| panic!("{e}"));
        last = std::fs::metadata(&path).unwrap_or_else(|e| panic!("{e}")).ino();
        if last == ino {
            std::fs::remove_file(&path).unwrap_or_else(|e| panic!("{e}"));
            freed_is_next = true;
            break;
        }
        fillers.push(path);
        if last > ino {
            break;
        }
    }
    let made = if freed_is_next {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&segment)
            .unwrap_or_else(|e| panic!("{e}"));
        std::fs::metadata(&segment).unwrap_or_else(|e| panic!("{e}")).ino()
    } else {
        0
    };
    for path in &fillers {
        let _ = std::fs::remove_file(path);
    }
    assert!(
        made == ino,
        "inconclusive: the file system ({fs}) did not give the deleted segment's inode {ino} out again (fillers: {}, the last filler's inode {last}, the new segment's {made})",
        fillers.len()
    );
    let before = make_the_chat_again(store.as_ref(), &segment);
    drop(guard);
    store.flush().unwrap_or_else(|e| panic!("{e:#}"));
    let after = std::fs::read(&segment).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        after == before,
        "the checkpoint wrote the deleted chat's verified bytes into the new chat's log (a reused inode {ino}, file system {fs}):\nbefore: {}\nafter:  {}",
        String::from_utf8_lossy(&before),
        String::from_utf8_lossy(&after)
    );
}
