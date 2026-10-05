//! Regression coverage for keygen's write/sync contract on Unix: what must
//! hold while the 32 key bytes are being saved, and when saving cannot
//! complete (see README "文件权限" for the permission side, covered in
//! keygen_permissions.rs).
//!
//! What is protected here:
//!
//! * Partial writes: if the kernel only accepts part of the buffer per
//!   write() call, the remaining bytes still land -- no byte already written
//!   is duplicated, no later byte is skipped. The final file is exactly the
//!   32 raw key bytes (no newline, header, or trailer), mode 0600.
//! * Recoverable interruption: write() failing with EINTR and then
//!   succeeding still completes the same generation with exit code 0.
//! * Success is only reported after every byte is written *and* fsynced:
//!   exit 0, the usual "Key saved to ..." line on stdout, empty stderr.
//! * Unrecoverable write error after some bytes landed, and fsync failure
//!   after all bytes landed: exit 1, stderr explains the failure (naming
//!   the target), stdout carries no success message, the file created by
//!   this invocation is removed, and neither the parent directory's
//!   permissions nor pre-existing siblings are touched. Key bytes never
//!   appear on stdout/stderr, on success or on failure.
//! * Zero-byte write: write() accepting 0 bytes while bytes remain -- no
//!   progress, yet no errno either -- is neither success nor a recoverable
//!   interruption. Whether it happens before any byte landed or midway,
//!   the command must end (not hang in the save loop) with exit 1, an
//!   explanation naming the target on stderr, empty stdout, and no file
//!   left at the target: no empty file, no partial key, no zero-padded
//!   32-byte stand-in.
//! * Close-stage failure: all 32 bytes written and fsynced, then close()
//!   reporting an unrecoverable error is still a failed save -- exit 1,
//!   empty stdout, stderr naming the target and the close-stage error, and
//!   the file removed even though it holds 32 bytes with mode 0600. If the
//!   system also refuses the removal, stderr keeps the close failure and
//!   additionally warns that cleanup did not complete and this run's key
//!   file may remain at the target; the parent directory's permissions are
//!   never loosened to force the cleanup, and the key is not stashed under
//!   another name. A close() that merely reports EINTR after releasing the
//!   descriptor is not a failure: the save is reported as usual, exit 0.
//! * Refused cleanup after a write/fsync/permission-stage failure: the same
//!   reporting rule as for the close stage -- exit 1, empty stdout, stderr
//!   keeps the original failure reason (unfinished write, failed sync, or
//!   the 0600 permission problem), additionally reports the failed cleanup
//!   and its reason, and warns that this run's file (even an empty one) may
//!   still be present at the target so the user can check and remove it.
//!
//! The faults are injected by the LD_PRELOAD shim (tests/support/permfail),
//! which also traces every write()/fsync() on the key descriptor so the
//! byte stream can be checked for gaps, duplicates, and leaks.
#![cfg(unix)]

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const KEY_LEN: usize = 32;
const MODE_0600: u32 = 0o600;
const UMASKS: [u32; 3] = [0o022, 0o000, 0o777];

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_wrapfile"))
}

fn permfail_so() -> Option<PathBuf> {
    option_env!("WRAPFILE_PERMFAIL_SO").map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// scratch directory
// ---------------------------------------------------------------------------

struct Tmp {
    path: PathBuf,
}

impl Tmp {
    fn new(label: &str) -> Tmp {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "wrapfile-keygen-write-tests-{}-{}-{}",
            std::process::id(),
            label,
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Tmp { path }
    }

    fn child(&self, name: impl AsRef<Path>) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// running the binary
// ---------------------------------------------------------------------------

struct Run {
    rc: i32,
    out: Vec<u8>,
    err: Vec<u8>,
}

/// Run `wrapfile keygen <key>` after setting the umask inside a /bin/sh
/// wrapper, with the permfail shim preloaded and a trace file configured.
fn run_keygen(key: &Path, umask: u32, so: &Path, extra_env: &[(&str, &str)], trace: &Path) -> Run {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(format!("umask {umask:04o}; exec \"$@\""))
        .arg("sh")
        .arg(bin())
        .arg("keygen")
        .arg(key)
        .env("LD_PRELOAD", so)
        .env("WRAPFILE_TEST_TRACE_FILE", trace);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let o = cmd.output().unwrap();
    Run {
        rc: o.status.code().unwrap_or(-1),
        out: o.stdout,
        err: o.stderr,
    }
}

// ---------------------------------------------------------------------------
// shim trace log
// ---------------------------------------------------------------------------

struct Event {
    kind: String,
    attrs: HashMap<String, String>,
}

impl Event {
    fn get(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).map(String::as_str)
    }

    fn ret(&self) -> Option<i64> {
        self.get("ret").and_then(|v| v.parse().ok())
    }

    fn off(&self) -> Option<u64> {
        self.get("off").and_then(|v| v.parse().ok())
    }

    fn hex_bytes(&self) -> Option<Vec<u8>> {
        let hex = self.get("hex")?;
        assert_eq!(hex.len() % 2, 0, "odd hex in trace line");
        Some(
            (0..hex.len() / 2)
                .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
                .collect(),
        )
    }
}

fn parse_trace(path: &Path) -> Vec<Event> {
    let text = fs::read_to_string(path).unwrap();
    text.lines()
        .map(|line| {
            let mut it = line.split_whitespace();
            let kind = it.next().unwrap_or("").to_string();
            let attrs = it
                .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
                .collect();
            Event { kind, attrs }
        })
        .collect()
}

/// Create the trace file up front (mode 0644) so that it stays readable even
/// when the child runs under umask 0777 -- the shim opens it O_APPEND.
fn prepare_trace(dir: &Path, name: impl AsRef<Path>) -> PathBuf {
    let p = dir.join(name);
    fs::File::create(&p).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
    p
}

/// The successful write() calls, in call order: (offset, bytes landed, bytes).
fn successful_writes(events: &[Event]) -> Vec<(u64, u64, Option<Vec<u8>>)> {
    events
        .iter()
        .filter(|e| e.kind == "WRITE" && e.ret().is_some_and(|r| r > 0))
        .map(|e| {
            (
                e.off().expect("WRITE event needs off="),
                e.ret().unwrap() as u64,
                e.hex_bytes(),
            )
        })
        .collect()
}

/// The landed writes must tile [0, total) exactly: the first starts at 0,
/// each continues where the previous ended -- no byte written twice, none
/// skipped.
fn assert_writes_tile(writes: &[(u64, u64, Option<Vec<u8>>)], total: usize, context: &str) {
    assert!(!writes.is_empty(), "{context}: no bytes were ever written");
    let mut expect = 0u64;
    for (off, n, _) in writes {
        assert_eq!(
            *off, expect,
            "{context}: write at offset {off} overlaps or leaves a gap (expected {expect})"
        );
        expect += n;
    }
    assert_eq!(
        expect as usize, total,
        "{context}: writes must add up to exactly {total} bytes, got {expect}"
    );
}

fn reconstruct(writes: &[(u64, u64, Option<Vec<u8>>)]) -> Vec<u8> {
    writes
        .iter()
        .flat_map(|(_, _, hex)| hex.clone().expect("TRACE_BYTES must be on"))
        .collect()
}

/// The write() calls that accepted 0 bytes while reporting no error.
fn zero_writes<'a>(events: &'a [Event]) -> Vec<&'a Event> {
    events
        .iter()
        .filter(|e| e.kind == "WRITE" && e.ret() == Some(0))
        .collect()
}

/// Bytes the program handed to write() calls that accepted none of them.
fn rejected_offerings(events: &[Event]) -> Vec<u8> {
    zero_writes(events)
        .iter()
        .flat_map(|e| e.hex_bytes().expect("TRACE_BYTES must be on"))
        .collect()
}

// ---------------------------------------------------------------------------
// filesystem assertions
// ---------------------------------------------------------------------------

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

fn contains_slice(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

/// The full success contract: exit 0, empty stderr, only the save-location
/// line on stdout, and exactly 32 raw key bytes in a 0600 file that leaks
/// nowhere.
fn assert_success(key: &Path, run: &Run) -> Vec<u8> {
    assert_eq!(run.rc, 0, "stderr={}", String::from_utf8_lossy(&run.err));
    assert!(
        run.err.is_empty(),
        "stderr must be empty on success, got {}",
        String::from_utf8_lossy(&run.err)
    );
    assert_eq!(
        run.out,
        format!("Key saved to {}\n", key.display()).into_bytes(),
        "stdout must be exactly the save-location message"
    );

    let meta = fs::metadata(key).expect("key file must exist on success");
    assert_eq!(meta.len() as usize, KEY_LEN, "key must be exactly 32 bytes");
    assert_eq!(
        meta.mode() & 0o7777,
        MODE_0600,
        "key file mode must be exactly 0600"
    );

    let bytes = fs::read(key).unwrap();
    assert_eq!(bytes.len(), KEY_LEN);
    assert!(bytes.iter().any(|&b| b != 0), "key must not be all zeroes");
    assert!(
        !contains_slice(&run.out, &bytes) && !contains_slice(&run.err, &bytes),
        "key bytes must never appear on stdout/stderr"
    );
    bytes
}

/// The failure contract shared by the write-error and fsync-error tests:
/// exit 1, an explanation on stderr, no success output, no leaked key
/// material, and no trace of this invocation left in the parent directory.
fn assert_failed_cleanly(
    key: &Path,
    run: &Run,
    dir: &Path,
    allowed_entries: &[&str],
    leaked_candidate: &[u8],
) {
    assert_eq!(run.rc, 1, "an unfinishable save must exit 1");
    assert!(
        run.out.is_empty(),
        "no success message may be printed on failure, got {}",
        String::from_utf8_lossy(&run.out)
    );
    assert!(!run.err.is_empty(), "stderr must explain the failure");
    let stderr = String::from_utf8_lossy(&run.err);
    assert!(
        stderr.contains(&key.display().to_string()),
        "stderr should name the target path, got: {stderr}"
    );
    assert!(
        !contains_slice(&run.out, leaked_candidate)
            && !contains_slice(&run.err, leaked_candidate),
        "key material must never leak to stdout/stderr, even on failure"
    );

    assert!(
        !key.exists(),
        "the file this invocation created must be removed after the failure"
    );
    let mut leftovers: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !allowed_entries.contains(&n.as_str()))
        .collect();
    leftovers.sort();
    assert!(leftovers.is_empty(), "unexpected leftover files: {leftovers:?}");
}

/// Key material must not surface on stdout/stderr in any encoding: not as
/// raw bytes, and not as a hex (or other text) rendering.
fn assert_key_material_not_leaked(run: &Run, material: &[u8], what: &str) {
    assert!(
        !contains_slice(&run.out, material) && !contains_slice(&run.err, material),
        "{what} must never appear on stdout/stderr as raw bytes"
    );
    let hex: String = material.iter().map(|b| format!("{b:02x}")).collect();
    let out = String::from_utf8_lossy(&run.out);
    let err = String::from_utf8_lossy(&run.err);
    assert!(
        !out.contains(&hex) && !err.contains(&hex),
        "{what} must never appear on stdout/stderr in hex form"
    );
}

/// The contract when the save fails AND the system refuses to remove this
/// run's file: exit 1, empty stdout, stderr names the target, keeps the
/// original failure reason (`stage_reason`), reports the refused cleanup,
/// and warns that this run's file may remain -- and the file really does
/// remain, with nothing stashed under another name.
fn assert_failed_with_residue(
    key: &Path,
    run: &Run,
    dir: &Path,
    allowed_entries: &[&str],
    stage_reason: &str,
) {
    assert_eq!(run.rc, 1, "a failed save with refused cleanup must exit 1");
    assert!(
        run.out.is_empty(),
        "no success message may be printed, got {}",
        String::from_utf8_lossy(&run.out)
    );
    let stderr = String::from_utf8_lossy(&run.err);
    assert!(
        stderr.contains(&key.display().to_string()),
        "stderr should name the target path, got: {stderr}"
    );
    // The original failure reason is still reported...
    assert!(
        stderr.contains(stage_reason),
        "stderr must keep the original failure reason ({stage_reason:?}), got: {stderr}"
    );
    // ...the cleanup failure is reported too (not silently dropped, and not
    // replacing the save error)...
    assert!(
        stderr.contains("cleanup"),
        "stderr must report the failed cleanup, got: {stderr}"
    );
    // ...and the user is told this run's file may remain -- never that it
    // is gone.
    assert!(
        stderr.contains("may still be present"),
        "stderr must warn that this run's file may remain, got: {stderr}"
    );

    // The refused removal means this run's file really is still at the
    // target -- the warning above is what keeps it from being mistaken for
    // a successfully saved key.
    assert!(
        key.exists(),
        "with unlink refused, this run's file remains at the target"
    );
    // Nothing was stashed under another name and no other residue appeared.
    let mut expected: Vec<String> = allowed_entries.iter().map(|s| s.to_string()).collect();
    expected.push(key.file_name().unwrap().to_string_lossy().into_owned());
    expected.sort();
    let mut entries: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(entries, expected, "no renamed copy or other leftover may appear");
}

// A pre-existing object that must survive a failed keygen byte-for-byte.
struct Sentinel {
    path: PathBuf,
    bytes: Vec<u8>,
    mode: u32,
    inode: u64,
}

impl Sentinel {
    fn file(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> Sentinel {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        let meta = fs::metadata(&path).unwrap();
        Sentinel { path, bytes: bytes.to_vec(), mode, inode: meta.ino() }
    }

    fn assert_untouched(&self) {
        let meta = fs::metadata(&self.path).expect("sentinel must still exist");
        assert_eq!(meta.ino(), self.inode, "sentinel inode changed (object replaced)");
        assert_eq!(meta.mode() & 0o7777, self.mode, "sentinel permissions changed");
        assert_eq!(fs::read(&self.path).unwrap(), self.bytes, "sentinel contents changed");
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[test]
fn partial_writes_save_the_full_key_exactly_once() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    for umask in UMASKS {
        let tmp = Tmp::new("partial");
        let key = tmp.child("key");
        let trace = prepare_trace(&tmp.path, "trace.log");

        // The kernel accepts only one byte per write() call.
        let run = run_keygen(
            &key,
            umask,
            &so,
            &[
                ("WRAPFILE_TEST_PARTIAL_WRITE", "1"),
                ("WRAPFILE_TEST_TRACE_BYTES", "1"),
            ],
            &trace,
        );

        let on_disk = assert_success(&key, &run);

        let events = parse_trace(&trace);
        let writes = successful_writes(&events);
        assert!(
            writes.len() >= KEY_LEN,
            "one-byte writes should mean at least 32 write() calls, got {}",
            writes.len()
        );
        // No written byte repeated, no later byte dropped...
        assert_writes_tile(&writes, KEY_LEN, "partial writes");
        // ...and the bytes the program handed to the kernel are exactly the
        // bytes that ended up in the file -- nothing reordered or invented.
        assert_eq!(
            reconstruct(&writes),
            on_disk,
            "file content must equal the concatenation of the write() calls"
        );

        // Success was reported only after the data was synced to disk.
        let last_write = events
            .iter()
            .rposition(|e| e.kind == "WRITE" && e.ret().is_some_and(|r| r > 0))
            .unwrap();
        let fsynced = events
            .iter()
            .position(|e| e.kind == "FSYNC" && e.ret() == Some(0))
            .expect("the key must be fsynced before success is reported");
        assert!(fsynced > last_write, "fsync must follow the last write");
    }
}

#[test]
fn interrupted_writes_are_retried_until_the_key_is_saved() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let tmp = Tmp::new("eintr");
    let key = tmp.child("key");
    let trace = prepare_trace(&tmp.path, "trace.log");

    // The first five write() calls are interrupted before any byte is
    // consumed; afterwards the system recovers (and still only takes three
    // bytes at a time). The same generation must complete, not fail.
    let run = run_keygen(
        &key,
        0o022,
        &so,
        &[
            ("WRAPFILE_TEST_WRITE_EINTR", "5"),
            ("WRAPFILE_TEST_PARTIAL_WRITE", "3"),
            ("WRAPFILE_TEST_TRACE_BYTES", "1"),
        ],
        &trace,
    );

    let on_disk = assert_success(&key, &run);

    let events = parse_trace(&trace);
    let eintrs = events
        .iter()
        .filter(|e| e.kind == "WRITE" && e.get("errno") == Some("EINTR"))
        .count();
    assert_eq!(eintrs, 5, "all injected EINTRs must have been observed");

    // The interruption consumed nothing: the bytes that eventually landed
    // still tile [0, 32) exactly once and match the file.
    let writes = successful_writes(&events);
    assert_writes_tile(&writes, KEY_LEN, "writes after EINTR");
    assert_eq!(reconstruct(&writes), on_disk);
}

#[test]
fn unrecoverable_write_error_fails_and_removes_the_partial_file() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let tmp = Tmp::new("write-error");
    let key = tmp.child("key");
    let sibling = Sentinel::file(&tmp.path, "sibling", b"do-not-touch", 0o644);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    // Ten bytes land (4 + 4 + 2), then every further write fails with EIO.
    let run = run_keygen(
        &key,
        0o022,
        &so,
        &[
            ("WRAPFILE_TEST_PARTIAL_WRITE", "4"),
            ("WRAPFILE_TEST_FAIL_WRITE_AFTER", "10"),
            ("WRAPFILE_TEST_TRACE_BYTES", "1"),
        ],
        &trace,
    );

    let events = parse_trace(&trace);
    let writes = successful_writes(&events);
    // The failure really happened mid-write: part of the key was on disk.
    assert_writes_tile(&writes, 10, "writes before the error");
    assert!(
        events
            .iter()
            .any(|e| e.kind == "WRITE" && e.get("errno") == Some("EIO")),
        "the unrecoverable write error must have been injected"
    );
    assert!(
        events.iter().all(|e| e.kind != "FSYNC"),
        "an incomplete key must never be synced as if complete"
    );
    let partial_key = reconstruct(&writes);
    assert_eq!(partial_key.len(), 10);

    assert_failed_cleanly(&key, &run, &tmp.path, &["sibling", "trace.log"], &partial_key);
    sibling.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent directory permissions must be untouched"
    );

    // The failure was transient: a retry without the fault saves a full key.
    let trace2 = prepare_trace(&tmp.path, "trace2.log");
    let run2 = run_keygen(&key, 0o022, &so, &[], &trace2);
    assert_success(&key, &run2);
    sibling.assert_untouched();
}

#[test]
fn fsync_failure_after_full_write_fails_and_removes_the_file() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let tmp = Tmp::new("fsync-error");
    let key = tmp.child("key");
    let sibling = Sentinel::file(&tmp.path, "sibling", b"keep-me", 0o600);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    // All 32 bytes are written, but syncing them to disk fails.
    let run = run_keygen(
        &key,
        0o022,
        &so,
        &[
            ("WRAPFILE_TEST_FAIL_FSYNC", "1"),
            ("WRAPFILE_TEST_TRACE_BYTES", "1"),
        ],
        &trace,
    );

    let events = parse_trace(&trace);
    let writes = successful_writes(&events);
    // The whole key was written before the sync failed...
    assert_writes_tile(&writes, KEY_LEN, "writes before fsync");
    assert!(
        events
            .iter()
            .any(|e| e.kind == "FSYNC" && e.ret() == Some(-1)),
        "the fsync failure must have been injected"
    );
    // ...so the full key is the material that must not leak.
    let full_key = reconstruct(&writes);
    assert_eq!(full_key.len(), KEY_LEN);

    assert_failed_cleanly(&key, &run, &tmp.path, &["sibling", "trace.log"], &full_key);
    sibling.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent directory permissions must be untouched"
    );
}

#[test]
fn zero_byte_write_before_any_progress_fails_and_leaves_no_file() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let tmp = Tmp::new("write-zero-start");
    let key = tmp.child("key");
    let sibling = Sentinel::file(&tmp.path, "sibling", b"do-not-touch", 0o644);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    // The very first write() accepts 0 bytes and reports no error. This is
    // not a partial write (nothing advances) and not an interruption (no
    // errno): the command must give up, not loop inside the save.
    let run = run_keygen(
        &key,
        0o022,
        &so,
        &[
            ("WRAPFILE_TEST_WRITE_ZERO_AFTER", "0"),
            ("WRAPFILE_TEST_TRACE_BYTES", "1"),
        ],
        &trace,
    );

    let events = parse_trace(&trace);
    // No byte ever landed...
    assert!(
        successful_writes(&events).is_empty(),
        "no key byte may be written when write() accepts nothing"
    );
    // ...and the zero-acceptance really was injected (this run is not a
    // pass because the fault never triggered).
    assert!(
        !zero_writes(&events).is_empty(),
        "the zero-byte write must have been injected"
    );
    assert!(
        events.iter().all(|e| e.kind != "FSYNC"),
        "a key that was never written must never be synced as if complete"
    );
    // The whole key was offered and rejected; it is the material that must
    // not leak.
    let full_key = rejected_offerings(&events);
    assert_eq!(full_key.len(), KEY_LEN);

    assert_failed_cleanly(&key, &run, &tmp.path, &["sibling", "trace.log"], &full_key);
    assert_key_material_not_leaked(&run, &full_key, "the rejected key");
    // stderr must say the save could not be completed -- not a usage error,
    // and not anything that reads as success.
    let stderr = String::from_utf8_lossy(&run.err);
    assert!(
        stderr.contains("incomplete write"),
        "stderr should explain the key file was not fully written, got: {stderr}"
    );

    sibling.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent directory permissions must be untouched"
    );

    // Nothing was left behind -- not even an empty file: a fault-free retry
    // at the same path succeeds (O_EXCL would fail on any leftover).
    let trace2 = prepare_trace(&tmp.path, "trace2.log");
    let run2 = run_keygen(&key, 0o022, &so, &[], &trace2);
    assert_success(&key, &run2);
    sibling.assert_untouched();
}

#[test]
fn zero_byte_write_after_partial_progress_removes_the_partial_file() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let tmp = Tmp::new("write-zero-midway");
    let key = tmp.child("key");
    let sibling = Sentinel::file(&tmp.path, "sibling", b"keep-me", 0o600);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    // Ten bytes land, then write() accepts 0 bytes without an error.
    let run = run_keygen(
        &key,
        0o022,
        &so,
        &[
            ("WRAPFILE_TEST_WRITE_ZERO_AFTER", "10"),
            ("WRAPFILE_TEST_TRACE_BYTES", "1"),
        ],
        &trace,
    );

    let events = parse_trace(&trace);
    let writes = successful_writes(&events);
    // The stall really happened mid-save: part of the key was on disk.
    assert_writes_tile(&writes, 10, "writes before the zero-byte write");
    assert!(
        !zero_writes(&events).is_empty(),
        "the zero-byte write must have been injected"
    );
    assert!(
        events.iter().all(|e| e.kind != "FSYNC"),
        "an incomplete key must never be synced as if complete"
    );
    let partial_key = reconstruct(&writes);
    assert_eq!(partial_key.len(), 10);
    // Landed prefix + rejected remainder reconstruct the full key.
    let mut full_key = partial_key.clone();
    full_key.extend(rejected_offerings(&events));
    assert_eq!(full_key.len(), KEY_LEN);

    assert_failed_cleanly(&key, &run, &tmp.path, &["sibling", "trace.log"], &full_key);
    // Neither the full key nor the 10-byte fragment that briefly sat on
    // disk may leak, as raw bytes or in hex.
    assert_key_material_not_leaked(&run, &full_key, "the rejected key");
    assert_key_material_not_leaked(&run, &partial_key, "the saved key fragment");
    let stderr = String::from_utf8_lossy(&run.err);
    assert!(
        stderr.contains("incomplete write"),
        "stderr should explain the key file was not fully written, got: {stderr}"
    );

    sibling.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent directory permissions must be untouched"
    );

    // The 10-byte partial file was removed (not zero-padded to 32 bytes,
    // not left behind): a fault-free retry at the same path succeeds.
    let trace2 = prepare_trace(&tmp.path, "trace2.log");
    let run2 = run_keygen(&key, 0o022, &so, &[], &trace2);
    assert_success(&key, &run2);
    sibling.assert_untouched();
}

#[test]
fn close_error_after_fsync_fails_and_removes_the_file() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let tmp = Tmp::new("close-error");
    let key = tmp.child("key");
    let sibling = Sentinel::file(&tmp.path, "sibling", b"do-not-touch", 0o644);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    // All 32 bytes are written and fsynced, then close() reports an
    // unrecoverable I/O error. The file on disk looks complete (32 bytes,
    // mode 0600) -- the save must still be treated as failed.
    let run = run_keygen(
        &key,
        0o022,
        &so,
        &[
            ("WRAPFILE_TEST_FAIL_CLOSE", "1"),
            ("WRAPFILE_TEST_TRACE_BYTES", "1"),
        ],
        &trace,
    );

    let events = parse_trace(&trace);
    let writes = successful_writes(&events);
    // The whole key was written and synced before the close failed...
    assert_writes_tile(&writes, KEY_LEN, "writes before close");
    assert!(
        events
            .iter()
            .any(|e| e.kind == "FSYNC" && e.ret() == Some(0)),
        "the key must have been fully synced before the close"
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == "CLOSE" && e.get("errno") == Some("EIO")),
        "the unrecoverable close error must have been injected"
    );
    // ...so the full key is the material that must not leak.
    let full_key = reconstruct(&writes);
    assert_eq!(full_key.len(), KEY_LEN);

    assert_failed_cleanly(&key, &run, &tmp.path, &["sibling", "trace.log"], &full_key);
    assert_key_material_not_leaked(&run, &full_key, "the closed-over key");
    // stderr must make clear the failure happened while closing the key
    // file -- not read as a usage error, a permission problem, or success.
    let stderr = String::from_utf8_lossy(&run.err);
    assert!(
        stderr.contains("closing the key file"),
        "stderr should say the failure happened at close time, got: {stderr}"
    );

    sibling.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent directory permissions must be untouched"
    );

    // The failed run left nothing behind: a fault-free retry at the same
    // path succeeds (O_EXCL would fail on any leftover).
    let trace2 = prepare_trace(&tmp.path, "trace2.log");
    let run2 = run_keygen(&key, 0o022, &so, &[], &trace2);
    assert_success(&key, &run2);
    sibling.assert_untouched();
}

#[test]
fn close_eintr_after_fsync_still_reports_success() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let tmp = Tmp::new("close-eintr");
    let key = tmp.child("key");
    let trace = prepare_trace(&tmp.path, "trace.log");

    // close() releases the descriptor but reports EINTR: an interruption,
    // not an unrecoverable failure. The save is complete and must be
    // reported as usual.
    let run = run_keygen(
        &key,
        0o022,
        &so,
        &[("WRAPFILE_TEST_CLOSE_EINTR", "1")],
        &trace,
    );

    assert_success(&key, &run);

    let events = parse_trace(&trace);
    assert!(
        events
            .iter()
            .any(|e| e.kind == "CLOSE" && e.get("errno") == Some("EINTR")),
        "the EINTR-on-close must have been injected"
    );
}

#[test]
fn close_error_with_unlink_refused_reports_both_and_warns_of_residue() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let tmp = Tmp::new("close-error-unlink-refused");
    let key = tmp.child("key");
    let sibling = Sentinel::file(&tmp.path, "sibling", b"keep-me", 0o600);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    // close() fails unrecoverably, and the system then refuses to remove
    // this run's file. The command must still exit 1 and must say both:
    // the close failure and the unfinished cleanup.
    let run = run_keygen(
        &key,
        0o022,
        &so,
        &[
            ("WRAPFILE_TEST_FAIL_CLOSE", "1"),
            ("WRAPFILE_TEST_FAIL_UNLINK", "1"),
            ("WRAPFILE_TEST_TRACE_BYTES", "1"),
        ],
        &trace,
    );

    let events = parse_trace(&trace);
    let writes = successful_writes(&events);
    assert_writes_tile(&writes, KEY_LEN, "writes before close");
    assert!(
        events
            .iter()
            .any(|e| e.kind == "CLOSE" && e.get("errno") == Some("EIO")),
        "the unrecoverable close error must have been injected"
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == "UNLINK" && e.get("errno") == Some("EACCES")),
        "the refused cleanup must have been injected"
    );
    let full_key = reconstruct(&writes);
    assert_eq!(full_key.len(), KEY_LEN);

    assert_eq!(run.rc, 1, "a failed save with refused cleanup must exit 1");
    assert!(
        run.out.is_empty(),
        "no success message may be printed, got {}",
        String::from_utf8_lossy(&run.out)
    );
    let stderr = String::from_utf8_lossy(&run.err);
    assert!(
        stderr.contains(&key.display().to_string()),
        "stderr should name the target path, got: {stderr}"
    );
    // The original close failure is still reported...
    assert!(
        stderr.contains("closing the key file"),
        "stderr must keep the close failure reason, got: {stderr}"
    );
    // ...and the user is told the cleanup did not complete and this run's
    // key file may remain -- not just "save failed", never "no residue".
    assert!(
        stderr.contains("may still be present"),
        "stderr must warn that this run's key file may remain, got: {stderr}"
    );
    assert_key_material_not_leaked(&run, &full_key, "the closed-over key");

    // The refused removal means this run's file really is still at the
    // target -- the warning above is what keeps it from being mistaken for
    // a successfully saved key.
    assert!(
        key.exists(),
        "with unlink refused, this run's file remains at the target"
    );
    // Nothing was stashed under another name and no other residue appeared.
    let mut entries: Vec<String> = fs::read_dir(&tmp.path)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec![
            "key".to_string(),
            "sibling".to_string(),
            "trace.log".to_string()
        ],
        "no renamed copy or other leftover may appear"
    );
    sibling.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent directory permissions must never be loosened to force cleanup"
    );

    // Remove the residue manually; a fault-free retry then succeeds.
    fs::remove_file(&key).unwrap();
    let trace2 = prepare_trace(&tmp.path, "trace2.log");
    let run2 = run_keygen(&key, 0o022, &so, &[], &trace2);
    assert_success(&key, &run2);
    sibling.assert_untouched();
}

#[test]
fn write_error_with_unlink_refused_reports_both_and_warns_of_residue() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let tmp = Tmp::new("write-error-unlink-refused");
    let key = tmp.child("key");
    let sibling = Sentinel::file(&tmp.path, "sibling", b"do-not-touch", 0o644);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    // Ten bytes land, write() then fails unrecoverably, and the system
    // refuses to remove the partial file. The command must report both the
    // write failure and the unfinished cleanup.
    let run = run_keygen(
        &key,
        0o022,
        &so,
        &[
            ("WRAPFILE_TEST_PARTIAL_WRITE", "4"),
            ("WRAPFILE_TEST_FAIL_WRITE_AFTER", "10"),
            ("WRAPFILE_TEST_FAIL_UNLINK", "1"),
            ("WRAPFILE_TEST_TRACE_BYTES", "1"),
        ],
        &trace,
    );

    let events = parse_trace(&trace);
    let writes = successful_writes(&events);
    assert_writes_tile(&writes, 10, "writes before the error");
    assert!(
        events
            .iter()
            .any(|e| e.kind == "WRITE" && e.get("errno") == Some("EIO")),
        "the unrecoverable write error must have been injected"
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == "UNLINK" && e.get("errno") == Some("EACCES")),
        "the refused cleanup must have been injected"
    );
    let partial_key = reconstruct(&writes);
    assert_eq!(partial_key.len(), 10);

    assert_failed_with_residue(
        &key,
        &run,
        &tmp.path,
        &["sibling", "trace.log"],
        "could not write key file",
    );
    assert_key_material_not_leaked(&run, &partial_key, "the partial key");

    // The residue is the 10-byte partial file -- not padded to 32 bytes,
    // not renamed, and not to be mistaken for a saved key.
    assert_eq!(
        fs::read(&key).unwrap().len(),
        10,
        "the residue must be the partial file exactly as the failed run left it"
    );
    sibling.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent directory permissions must never be loosened to force cleanup"
    );

    // Remove the residue manually; a fault-free retry then succeeds.
    fs::remove_file(&key).unwrap();
    let trace2 = prepare_trace(&tmp.path, "trace2.log");
    let run2 = run_keygen(&key, 0o022, &so, &[], &trace2);
    assert_success(&key, &run2);
    sibling.assert_untouched();
}

#[test]
fn fsync_error_with_unlink_refused_reports_both_and_warns_of_residue() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let tmp = Tmp::new("fsync-error-unlink-refused");
    let key = tmp.child("key");
    let sibling = Sentinel::file(&tmp.path, "sibling", b"keep-me", 0o600);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    // All 32 bytes are written, the sync fails, and the system refuses to
    // remove the file. Even though the residue holds a full 32-byte key,
    // the save did not complete: both failures must be reported.
    let run = run_keygen(
        &key,
        0o022,
        &so,
        &[
            ("WRAPFILE_TEST_FAIL_FSYNC", "1"),
            ("WRAPFILE_TEST_FAIL_UNLINK", "1"),
            ("WRAPFILE_TEST_TRACE_BYTES", "1"),
        ],
        &trace,
    );

    let events = parse_trace(&trace);
    let writes = successful_writes(&events);
    assert_writes_tile(&writes, KEY_LEN, "writes before fsync");
    assert!(
        events
            .iter()
            .any(|e| e.kind == "FSYNC" && e.ret() == Some(-1)),
        "the fsync failure must have been injected"
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == "UNLINK" && e.get("errno") == Some("EACCES")),
        "the refused cleanup must have been injected"
    );
    let full_key = reconstruct(&writes);
    assert_eq!(full_key.len(), KEY_LEN);

    assert_failed_with_residue(
        &key,
        &run,
        &tmp.path,
        &["sibling", "trace.log"],
        "could not flush key file",
    );
    assert_key_material_not_leaked(&run, &full_key, "the unsynced key");

    // The residue happens to hold 32 bytes, but it was never synced: it is
    // not a saved key and must not be reported as one.
    assert_eq!(fs::read(&key).unwrap().len(), KEY_LEN);
    sibling.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent directory permissions must never be loosened to force cleanup"
    );

    // Remove the residue manually; a fault-free retry then succeeds.
    fs::remove_file(&key).unwrap();
    let trace2 = prepare_trace(&tmp.path, "trace2.log");
    let run2 = run_keygen(&key, 0o022, &so, &[], &trace2);
    assert_success(&key, &run2);
    sibling.assert_untouched();
}

#[test]
fn zero_byte_write_with_unlink_refused_warns_about_empty_residue() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let tmp = Tmp::new("write-zero-unlink-refused");
    let key = tmp.child("key");
    let sibling = Sentinel::file(&tmp.path, "sibling", b"do-not-touch", 0o644);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    // The very first write() accepts 0 bytes, and the system then refuses
    // to remove the file. The residue is a mere empty file -- the warning
    // must still be honest about it possibly sitting at the target.
    let run = run_keygen(
        &key,
        0o022,
        &so,
        &[
            ("WRAPFILE_TEST_WRITE_ZERO_AFTER", "0"),
            ("WRAPFILE_TEST_FAIL_UNLINK", "1"),
            ("WRAPFILE_TEST_TRACE_BYTES", "1"),
        ],
        &trace,
    );

    let events = parse_trace(&trace);
    assert!(
        successful_writes(&events).is_empty(),
        "no key byte may be written when write() accepts nothing"
    );
    assert!(
        !zero_writes(&events).is_empty(),
        "the zero-byte write must have been injected"
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == "UNLINK" && e.get("errno") == Some("EACCES")),
        "the refused cleanup must have been injected"
    );
    let full_key = rejected_offerings(&events);
    assert_eq!(full_key.len(), KEY_LEN);

    assert_failed_with_residue(
        &key,
        &run,
        &tmp.path,
        &["sibling", "trace.log"],
        "incomplete write",
    );
    assert_key_material_not_leaked(&run, &full_key, "the rejected key");

    // The residue really is the empty file this run created.
    assert_eq!(
        fs::read(&key).unwrap().len(),
        0,
        "the residue must be the empty file exactly as the failed run left it"
    );
    sibling.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent directory permissions must never be loosened to force cleanup"
    );

    // Remove the residue manually; a fault-free retry then succeeds.
    fs::remove_file(&key).unwrap();
    let trace2 = prepare_trace(&tmp.path, "trace2.log");
    let run2 = run_keygen(&key, 0o022, &so, &[], &trace2);
    assert_success(&key, &run2);
    sibling.assert_untouched();
}
