//! Regression coverage for the keygen write path on Unix: how the 32 raw
//! key bytes reach the disk, and what happens when that save is interrupted
//! or fails partway through.
//!
//! What is protected here (complementing tests/keygen_permissions.rs, which
//! covers the permission contract):
//!
//! * Partial writes: if the kernel accepts only some of the key bytes per
//!   write() call, the remaining bytes are still saved -- continuing exactly
//!   where the previous call stopped, with no byte duplicated or dropped.
//! * Recoverable interruption: write() calls that fail with EINTR and then
//!   succeed again are retried within the same generation, not treated as a
//!   final failure.
//! * Success shape: the finished file is exactly the 32 raw key bytes (no
//!   newline, header, or trailer), mode 0600; the command exits 0 with the
//!   usual "Key saved to ..." line on stdout and an empty stderr -- and only
//!   after every byte was written and fsync() succeeded.
//! * Unrecoverable write error after some bytes were already accepted, and
//!   fsync() failure after all bytes were written: exit 1, stderr explains
//!   the failure, stdout carries no success message, and the file created by
//!   this invocation is removed -- while the parent directory's permissions
//!   and pre-existing sibling files stay byte-for-byte identical.
//! * Neither success nor failure output ever contains key bytes.
//! * A live symlink at the target path is refused like any other
//!   pre-existing object and left untouched.
//!
//! All of this is observed through the permfail LD_PRELOAD shim
//! (tests/support/permfail), which traces and faults the process's own
//! write()/fsync() calls on the descriptor it created.
#![cfg(unix)]

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const KEY_LEN: usize = 32;
const MODE_0600: u32 = 0o600;

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

/// Run `wrapfile keygen <key>` under the given umask with the permfail shim
/// preloaded, fault-injection env vars set, and a trace file configured.
fn run_keygen(key: &Path, umask: u32, so: &Path, extra_env: &[(&str, &str)], trace: &Path) -> Run {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(format!("umask {umask:04o}; exec \"$@\""))
        .arg("sh")
        .arg(bin())
        .arg("keygen")
        .arg(key);
    cmd.env("LD_PRELOAD", so);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.env("WRAPFILE_TEST_TRACE_FILE", trace);

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
    index: usize,
}

impl Event {
    fn get(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).map(String::as_str)
    }

    fn ret(&self) -> Option<i32> {
        self.get("ret").and_then(|v| v.parse().ok())
    }

    fn off(&self) -> u64 {
        self.get("off").unwrap().parse().unwrap()
    }

    fn len(&self) -> usize {
        self.get("len").unwrap().parse().unwrap()
    }

    fn hex_bytes(&self) -> Vec<u8> {
        hex_to_bytes(self.get("hex").unwrap())
    }
}

fn parse_trace(path: &Path) -> Vec<Event> {
    let text = fs::read_to_string(path).unwrap();
    text.lines()
        .enumerate()
        .map(|(index, line)| {
            let mut it = line.split_whitespace();
            let kind = it.next().unwrap_or("").to_string();
            let attrs = it
                .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
                .collect();
            Event { kind, attrs, index }
        })
        .collect()
}

fn hex_to_bytes(hex: &str) -> Vec<u8> {
    assert_eq!(hex.len() % 2, 0, "hex dump must have an even length");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

/// Create the trace file up front (mode 0644) so it stays readable even
/// when the child runs under a strict umask -- the shim opens it O_APPEND.
fn prepare_trace(dir: &Path, name: impl AsRef<Path>) -> PathBuf {
    let p = dir.join(name);
    fs::File::create(&p).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
    p
}

/// The WRITE events in order, reconstructed into the exact byte stream the
/// kernel accepted. Asserts the stream is contiguous: every write continues
/// exactly where the previous one stopped (no duplication, no gap).
fn accepted_byte_stream(events: &[Event]) -> Vec<u8> {
    let mut stream = Vec::new();
    for e in events.iter().filter(|e| e.kind == "WRITE") {
        assert_eq!(
            e.off() as usize,
            stream.len(),
            "write at event {} must continue where the previous write stopped",
            e.index
        );
        let bytes = e.hex_bytes();
        assert_eq!(bytes.len(), e.len(), "hex dump length must match len");
        assert!(e.len() > 0, "a successful write must accept at least one byte");
        stream.extend(bytes);
    }
    stream
}

fn fsync_succeeded_after_last_write(events: &[Event]) -> bool {
    let last_write = events
        .iter()
        .rposition(|e| e.kind == "WRITE")
        .expect("the key must have been written");
    events
        .iter()
        .skip(last_write + 1)
        .any(|e| e.kind == "FSYNC" && e.ret() == Some(0))
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

/// The complete success contract: exit 0, empty stderr, the exact save
/// announcement on stdout, and a 0600 file holding exactly 32 raw key bytes
/// that appear nowhere in the program's output.
fn assert_key_file_ok(path: &Path, run: &Run) -> Vec<u8> {
    assert_eq!(run.rc, 0, "stderr={}", String::from_utf8_lossy(&run.err));
    assert!(
        run.err.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&run.err)
    );

    let meta = fs::metadata(path).expect("key file should exist on success");
    assert_eq!(meta.len() as usize, KEY_LEN, "key must be exactly 32 bytes");
    assert_eq!(
        meta.mode() & 0o7777,
        MODE_0600,
        "key file mode must be exactly 0600"
    );

    let expected_stdout = format!("Key saved to {}\n", path.display()).into_bytes();
    assert_eq!(
        run.out, expected_stdout,
        "stdout should only announce the save location"
    );

    let bytes = fs::read(path).unwrap();
    assert_eq!(bytes.len(), KEY_LEN);
    assert!(
        !contains_slice(&run.out, &bytes) && !contains_slice(&run.err, &bytes),
        "key bytes must never appear on stdout/stderr"
    );
    bytes
}

/// The complete failure contract for a generation that could not finish:
/// exit 1, no success output, an explanation on stderr, and the file this
/// invocation created removed -- with nothing else in the parent directory
/// disturbed.
fn assert_failed_generation_cleaned(
    key: &Path,
    run: &Run,
    dir: &Path,
    allowed_entries: &[&str],
    leaked_key_prefix: &[u8],
) {
    assert_eq!(run.rc, 1, "a failed save must exit 1");
    assert!(
        run.out.is_empty(),
        "no success message on failure, got {}",
        String::from_utf8_lossy(&run.out)
    );
    assert!(!run.err.is_empty(), "stderr must explain the failure");
    let stderr = String::from_utf8_lossy(&run.err);
    assert!(
        stderr.contains("wrapfile keygen") && stderr.contains(&key.display().to_string()),
        "stderr must name the failed generation and its target, got: {stderr}"
    );
    assert!(
        !contains_slice(&run.out, leaked_key_prefix)
            && !contains_slice(&run.err, leaked_key_prefix),
        "key bytes that reached the disk must never leak into the output"
    );
    assert!(
        !key.exists(),
        "the failed generation's target must be removed"
    );

    let mut leftovers: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !allowed_entries.contains(&n.as_str()))
        .collect();
    leftovers.sort();
    assert!(leftovers.is_empty(), "unexpected leftover files: {leftovers:?}");
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
        Sentinel {
            path,
            bytes: bytes.to_vec(),
            mode,
            inode: meta.ino(),
        }
    }

    fn assert_untouched(&self) {
        let meta = fs::metadata(&self.path).expect("sentinel must still exist");
        assert_eq!(meta.ino(), self.inode, "sentinel inode changed (object replaced)");
        assert_eq!(meta.mode() & 0o7777, self.mode, "sentinel permissions changed");
        assert_eq!(
            fs::read(&self.path).unwrap(),
            self.bytes,
            "sentinel contents changed"
        );
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// The kernel accepts only part of the buffer per write() call: the rest of
/// the key must still be saved, continuing exactly where each call stopped.
#[test]
fn partial_writes_are_continued_not_restarted() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    for chunk in [1usize, 7, 31] {
        let tmp = Tmp::new("partial");
        let key = tmp.child("key");
        let trace = prepare_trace(&tmp.path, "trace.log");
        let chunk_str = chunk.to_string();

        let run = run_keygen(
            &key,
            0o022,
            &so,
            &[("WRAPFILE_TEST_PARTIAL_WRITE", &chunk_str)],
            &trace,
        );

        let file_bytes = assert_key_file_ok(&key, &run);
        let events = parse_trace(&trace);

        let write_count = events.iter().filter(|e| e.kind == "WRITE").count();
        assert!(
            write_count > 1,
            "chunk size {chunk} must force multiple write() calls"
        );
        for e in events.iter().filter(|e| e.kind == "WRITE") {
            assert!(
                e.len() <= chunk,
                "the shim must never accept more than {chunk} bytes per call"
            );
        }

        // The accepted stream is contiguous and totals exactly the key; the
        // file on disk is precisely that stream -- no byte written twice,
        // none missing, nothing appended.
        let stream = accepted_byte_stream(&events);
        assert_eq!(stream.len(), KEY_LEN, "exactly 32 key bytes must be accepted");
        assert_eq!(stream, file_bytes, "file must equal the accepted byte stream");

        // The success message only comes after a successful fsync.
        assert!(
            fsync_succeeded_after_last_write(&events),
            "fsync must succeed after the last write before success is reported"
        );
    }
}

/// write() interrupted by EINTR, then healthy again: the same generation
/// completes instead of failing.
#[test]
fn interrupted_writes_are_retried_until_completion() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    // (extra env, expected EINTR count): interruptions alone, and
    // interruptions interleaved with partial writes.
    let cases: &[(&[(&str, &str)], u64)] = &[
        (&[("WRAPFILE_TEST_EINTR_WRITES", "3")], 3),
        (
            &[
                ("WRAPFILE_TEST_EINTR_WRITES", "2"),
                ("WRAPFILE_TEST_PARTIAL_WRITE", "5"),
            ],
            2,
        ),
    ];

    for (env, expected_eintr) in cases {
        let tmp = Tmp::new("eintr");
        let key = tmp.child("key");
        let trace = prepare_trace(&tmp.path, "trace.log");

        let run = run_keygen(&key, 0o022, &so, env, &trace);

        let file_bytes = assert_key_file_ok(&key, &run);
        let events = parse_trace(&trace);

        let eintr_count = events.iter().filter(|e| e.kind == "WRITE_EINTR").count() as u64;
        assert_eq!(
            eintr_count, *expected_eintr,
            "every injected EINTR must have been absorbed by a retry"
        );

        let stream = accepted_byte_stream(&events);
        assert_eq!(stream.len(), KEY_LEN);
        assert_eq!(stream, file_bytes);
        assert!(fsync_succeeded_after_last_write(&events));
    }
}

/// An unrecoverable write error after part of the key was already accepted:
/// fail loudly, remove the partial file, touch nothing else.
#[test]
fn unrecoverable_write_error_fails_and_cleans_up() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let tmp = Tmp::new("write-fail");
    let key = tmp.child("key");
    let sibling = Sentinel::file(&tmp.path, "sibling", b"do-not-touch", 0o644);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    // The first 10 bytes land on disk, then every further write fails.
    let run = run_keygen(
        &key,
        0o022,
        &so,
        &[
            ("WRAPFILE_TEST_PARTIAL_WRITE", "10"),
            ("WRAPFILE_TEST_FAIL_WRITE_AFTER", "10"),
        ],
        &trace,
    );

    let events = parse_trace(&trace);
    let accepted = accepted_byte_stream(&events);
    assert_eq!(
        accepted.len(),
        10,
        "exactly the bytes accepted before the failure must be on record"
    );
    assert!(
        events.iter().any(|e| e.kind == "WRITE_FAIL"),
        "the unrecoverable write error must be visible in the trace"
    );

    assert_failed_generation_cleaned(&key, &run, &tmp.path, &["sibling", "trace.log"], &accepted);
    sibling.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent dir mode must not change"
    );
}

/// All 32 bytes written, then the flush to disk fails: same failure
/// contract -- no success message, and the incomplete save is removed.
#[test]
fn fsync_failure_fails_and_cleans_up() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let tmp = Tmp::new("fsync-fail");
    let key = tmp.child("key");
    let sibling = Sentinel::file(&tmp.path, "sibling", b"keep-me", 0o600);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    let run = run_keygen(&key, 0o022, &so, &[("WRAPFILE_TEST_FAIL_FSYNC", "1")], &trace);

    let events = parse_trace(&trace);
    // The whole key was written before the flush failed...
    let accepted = accepted_byte_stream(&events);
    assert_eq!(accepted.len(), KEY_LEN, "all 32 bytes were written before fsync");
    assert!(
        events.iter().any(|e| e.kind == "FSYNC" && e.ret() == Some(-1)),
        "the failed fsync must be visible in the trace"
    );

    // ...yet the command must not report success, and the file -- which was
    // never durably saved -- must be gone. The full key is known from the
    // trace; none of it may leak into the program's output.
    assert_failed_generation_cleaned(&key, &run, &tmp.path, &["sibling", "trace.log"], &accepted);
    sibling.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent dir mode must not change"
    );
}

/// A symlink at the target path -- dangling or pointing at a live file --
/// is a pre-existing object: refuse to generate and leave it untouched.
#[test]
fn existing_symlink_is_refused_and_preserved() {
    for umask in [0o022u32, 0o000, 0o777] {
        // --- live symlink pointing at an existing file ---
        let tmp = Tmp::new("live-symlink");
        let target = Sentinel::file(&tmp.path, "real-key", b"existing-secret", 0o600);
        let link = tmp.child("link");
        std::os::unix::fs::symlink(&target.path, &link).unwrap();

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(format!("umask {umask:04o}; exec \"$@\""))
            .arg("sh")
            .arg(bin())
            .arg("keygen")
            .arg(&link);
        let o = cmd.output().unwrap();
        let run = Run {
            rc: o.status.code().unwrap_or(-1),
            out: o.stdout,
            err: o.stderr,
        };

        assert_eq!(run.rc, 1, "a live symlink at the target must be refused");
        assert!(run.out.is_empty(), "no success message on refusal");
        assert!(!run.err.is_empty(), "stderr must explain the refusal");

        let md = fs::symlink_metadata(&link).expect("symlink itself must remain");
        assert!(md.file_type().is_symlink(), "the symlink must not be replaced");
        assert_eq!(fs::read_link(&link).unwrap(), target.path);
        target.assert_untouched();
    }
}
