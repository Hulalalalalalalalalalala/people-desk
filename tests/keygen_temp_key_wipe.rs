//! Regression coverage for keygen's temporary-key wipe on Unix (see README
//! "内存中的临时密钥清零"): the 32-byte buffer that directly holds the
//! generated key must be overwritten with zeroes before the operation
//! returns -- on success and on every failure path, and the erasure must
//! survive an optimizing compiler.
//!
//! File-level checks cannot see that guarantee: the key file is written
//! before the wipe and the process exits after it, so neither the saved
//! bytes, the output, nor the exit code can tell a wiped buffer from a
//! leaked one. These tests therefore watch the buffer itself, in-process,
//! via the LD_PRELOAD shim tests/support/keywipe:
//!
//! * The shim traps the guest's getrandom(2) with seccomp (like randtrap)
//!   and records the address of the key buffer and every byte the random
//!   source delivers into it (KEYFILL) -- so the tests can prove the run
//!   really handled non-zero secret bytes, and that the saved file holds
//!   exactly those bytes (a wipe that ran *before* the save would leave a
//!   zeroed or truncated file).
//! * Once the save is over (the key file's close, a failed create, or a
//!   random source that dried up mid-key) the shim read-only-protects the
//!   stack page(s) holding the buffer and single-steps every subsequent
//!   store to them. All 32 bytes must be observed stored as zero (WIPE
//!   confirmed) before the command prints its result (OUTPUT); a non-zero
//!   store into the buffer first (WIPE violated) or reaching process exit
//!   without a full wipe (WIPE missing) fails the test. This catches a
//!   wipe that was removed, that covers only part of the buffer, or that
//!   the optimizer deleted as dead stores.
//!
//! What is protected here:
//!
//! * Success (pattern and real /dev/urandom sources, under loose and
//!   strict umasks): exit 0, exactly the save-location message on stdout,
//!   the file holding precisely the 32 delivered bytes with mode 0600 --
//!   and the buffer observed fully zeroed before that message is printed.
//! * Random source failing after delivering only part of the key: exit 1,
//!   no success message, no file created, the delivered fragment leaking
//!   nowhere -- and the whole 32-byte buffer (the fragment plus the
//!   never-written tail) observed zeroed before the error is reported.
//! * Write failing mid-save after some key bytes already landed on disk:
//!   exit 1, the incomplete file cleaned up, the full in-memory key still
//!   observed zeroed before the error is reported.
//!
//! The same assertions run against the unoptimized (`cargo test`) and the
//! optimized (`cargo test --release`) binary, since the promise holds for
//! both; the volatile-write wipe must not be optimized away.
//!
//! The shim needs Linux with seccomp on x86_64/aarch64 and a C compiler
//! (built on demand, see tests/common); where it cannot run, these tests
//! state the reason and skip.
#![cfg(unix)]

mod common;

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
            "wrapfile-keygen-wipe-tests-{}-{}-{}",
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
/// wrapper, with the given shim(s) preloaded (already joined with ':')
/// and a trace file configured.
fn run_keygen(key: &Path, umask: u32, preload: &str, extra_env: &[(&str, &str)], trace: &Path) -> Run {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(format!("umask {umask:04o}; exec \"$@\""))
        .arg("sh")
        .arg(bin())
        .arg("keygen")
        .arg(key)
        .env("LD_PRELOAD", preload)
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

/// Create the trace file up front (mode 0644) so that it stays readable
/// even when the child runs under umask 0777 -- the shims open it O_APPEND.
fn prepare_trace(dir: &Path, name: impl AsRef<Path>) -> PathBuf {
    let p = dir.join(name);
    fs::File::create(&p).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
    p
}

fn trace_lines(trace: &Path) -> Vec<String> {
    fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// False when the shim could not arm the seccomp trap on this system (or
/// was not loaded at all); the caller then skips instead of failing.
fn trap_armed(lines: &[String]) -> bool {
    if lines.iter().any(|l| l == "TRAP armed") {
        return true;
    }
    eprintln!("skipping: keywipe shim could not arm the seccomp trap here");
    false
}

/// The bytes the random source delivered into the key buffer, from the
/// KEYFILL line, and how many they were (a source that dried up mid-key
/// delivers fewer than 32).
fn keyfill(lines: &[String]) -> (usize, Vec<u8>) {
    let line = lines
        .iter()
        .find(|l| l.starts_with("KEYFILL "))
        .expect("the run must have drawn key bytes (no KEYFILL line)");
    let mut n = None;
    let mut hex = None;
    for kv in line.split_whitespace().skip(1) {
        if let Some((k, v)) = kv.split_once('=') {
            match k {
                "n" => n = Some(v.parse::<usize>().unwrap()),
                "hex" => hex = Some(v.to_string()),
                _ => {}
            }
        }
    }
    let n = n.unwrap();
    let hex = hex.unwrap();
    assert_eq!(hex.len(), n * 2, "KEYFILL hex must match the byte count");
    let bytes = (0..n)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
        .collect();
    (n, bytes)
}

/// Assert the wipe lifecycle recorded in the trace: the watch engaged,
/// all 32 buffer bytes were observed stored as zero, and that happened
/// before the command printed anything -- no violation, no missing wipe.
fn assert_wipe_confirmed_before_output(lines: &[String], expected_fd: &str) {
    if lines.iter().any(|l| l.starts_with("WATCH unavailable")) {
        eprintln!("skipping: keywipe shim could not watch the buffer here");
        return;
    }
    assert!(
        lines.iter().any(|l| l.starts_with("WATCH armed")),
        "the buffer watch must have engaged: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.starts_with("WIPE violated")),
        "a non-zero store hit the key buffer before it was fully wiped: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.starts_with("WIPE missing")),
        "the process exited without the key buffer being fully wiped: {lines:?}"
    );
    let confirmed = lines
        .iter()
        .position(|l| l == "WIPE confirmed zeroed=32")
        .unwrap_or_else(|| {
            panic!("all 32 key-buffer bytes must have been observed wiped: {lines:?}")
        });
    let output = lines
        .iter()
        .position(|l| l == &format!("OUTPUT fd={expected_fd}"))
        .unwrap_or_else(|| panic!("the command must have printed its result: {lines:?}"));
    assert!(
        confirmed < output,
        "the key buffer must be wiped before the operation reports its result: {lines:?}"
    );
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

fn assert_no_leftovers(dir: &Path, allowed_entries: &[&str]) {
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

/// Shared assertions for a successful generation: the published success
/// contract, the saved file holding exactly the bytes the random source
/// delivered (so the wipe cannot have run before the save, and no byte is
/// missing or padded), and the in-memory buffer observed fully wiped
/// before the success message was printed.
fn assert_success_with_wipe(run: &Run, key: &Path, lines: &[String], umask: u32) {
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

    // The run really handled non-zero secret bytes.
    let (n, delivered) = keyfill(lines);
    assert_eq!(n, KEY_LEN, "a full 32-byte key must have been drawn");
    assert!(
        delivered.iter().any(|&b| b != 0),
        "the drawn key must contain non-zero secret bytes"
    );

    // The saved file is exactly those bytes: the wipe did not run before
    // the save (no all-zero file) and no byte is missing or padded.
    let saved = fs::read(key).unwrap();
    assert_eq!(saved.len(), KEY_LEN, "key must be exactly 32 bytes");
    assert_eq!(
        saved, delivered,
        "the file must hold exactly the bytes the source delivered"
    );
    assert_eq!(
        mode_of(key),
        MODE_0600,
        "key file mode must be exactly 0600 (umask {umask:04o})"
    );

    // And those bytes were wiped from the buffer before the success
    // message was printed.
    assert_wipe_confirmed_before_output(lines, "1");

    // The key never leaks to stdout/stderr.
    assert!(
        !contains_slice(&run.out, &delivered) && !contains_slice(&run.err, &delivered),
        "key bytes must never appear on stdout/stderr"
    );
}

#[test]
fn successful_keygen_wipes_temporary_key() {
    let Some(so) = common::keywipe_so() else {
        eprintln!("skipping: keywipe shim not available on this target");
        return;
    };
    let preload = so.display().to_string();

    // A deterministic, recognisable byte source, under loose and strict
    // umasks alike.
    for umask in UMASKS {
        let tmp = Tmp::new("wipe-success-pattern");
        let key = tmp.child("key");
        let trace = prepare_trace(&tmp.path, "trace.log");

        let run = run_keygen(
            &key,
            umask,
            &preload,
            &[("WRAPFILE_TEST_KEYWIPE_PATTERN", "1")],
            &trace,
        );
        let lines = trace_lines(&trace);
        if !trap_armed(&lines) {
            return;
        }
        assert_success_with_wipe(&run, &key, &lines, umask);
    }
}

#[test]
fn successful_keygen_with_real_random_source_wipes_temporary_key() {
    let Some(so) = common::keywipe_so() else {
        eprintln!("skipping: keywipe shim not available on this target");
        return;
    };
    let preload = so.display().to_string();

    // The same lifecycle with bytes drawn from the real OS random source
    // (the shim serves /dev/urandom and records what it delivered).
    let tmp = Tmp::new("wipe-success-real");
    let key = tmp.child("key");
    let trace = prepare_trace(&tmp.path, "trace.log");

    let run = run_keygen(&key, 0o022, &preload, &[("WRAPFILE_TEST_KEYWIPE", "1")], &trace);
    let lines = trace_lines(&trace);
    if !trap_armed(&lines) {
        return;
    }
    assert_success_with_wipe(&run, &key, &lines, 0o022);
}

#[test]
fn partial_random_failure_wipes_fragment() {
    let Some(so) = common::keywipe_so() else {
        eprintln!("skipping: keywipe shim not available on this target");
        return;
    };
    let preload = so.display().to_string();

    let tmp = Tmp::new("wipe-rand-partial");
    let key = tmp.child("key");
    let sibling = Sentinel::file(&tmp.path, "sibling", b"do-not-touch", 0o644);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    // The random source dries up after handing 18 bytes to the key fill:
    // some but fewer than 32 key bytes are delivered, then the draw fails
    // unrecoverably.
    let run = run_keygen(
        &key,
        0o022,
        &preload,
        &[("WRAPFILE_TEST_KEYWIPE_FAIL_AFTER", "18")],
        &trace,
    );
    let lines = trace_lines(&trace);
    if !trap_armed(&lines) {
        return;
    }

    // The scenario really happened: a non-empty fragment of non-zero
    // secret bytes reached the buffer before the source failed.
    let (n, fragment) = keyfill(&lines);
    assert!(
        n > 0 && n < KEY_LEN,
        "the key fill must have received some but not all of the 32 bytes, got {n}"
    );
    assert!(
        fragment.iter().any(|&b| b != 0),
        "the delivered fragment must contain non-zero secret bytes"
    );

    // The published failure contract: exit 1, no success message, an
    // explanation naming the random source, and nothing left behind.
    assert_eq!(run.rc, 1, "an unfinishable random draw must exit 1");
    assert!(
        run.out.is_empty(),
        "stdout must stay empty on random-source failure, got {}",
        String::from_utf8_lossy(&run.out)
    );
    let stderr = String::from_utf8_lossy(&run.err);
    assert!(
        stderr.contains("cryptographic random source unavailable"),
        "stderr must state that the cryptographic random source is unavailable, got: {stderr}"
    );
    assert!(
        !key.exists(),
        "no file may be left at the target path after a random-source failure"
    );
    assert_no_leftovers(&tmp.path, &["sibling", "trace.log"]);
    sibling.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent directory permissions must be untouched"
    );

    // The fragment is secret material in waiting: it leaks nowhere, and
    // the whole 32-byte buffer -- fragment and never-written tail alike --
    // was wiped before the error was reported.
    assert!(
        !contains_slice(&run.out, &fragment) && !contains_slice(&run.err, &fragment),
        "partially drawn random bytes must never appear on stdout/stderr"
    );
    let hex: String = fragment.iter().map(|b| format!("{b:02x}")).collect();
    assert!(
        !stderr.contains(&hex) && !String::from_utf8_lossy(&run.out).contains(&hex),
        "partially drawn random bytes must not appear hex-encoded either"
    );
    assert_wipe_confirmed_before_output(&lines, "2");
}

#[test]
fn write_failure_after_partial_save_wipes_full_key() {
    let (Some(keywipe), Some(permfail)) = (common::keywipe_so(), common::permfail_so()) else {
        eprintln!("skipping: keywipe/permfail shims not available on this target");
        return;
    };
    let preload = format!("{}:{}", keywipe.display(), permfail.display());

    let tmp = Tmp::new("wipe-write-failure");
    let key = tmp.child("key");
    let sibling = Sentinel::file(&tmp.path, "sibling", b"keep-me", 0o600);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    // A full 32-byte key is drawn, but the save's write() fails
    // unrecoverably after 10 bytes have landed on disk.
    let run = run_keygen(
        &key,
        0o022,
        &preload,
        &[
            ("WRAPFILE_TEST_KEYWIPE_PATTERN", "1"),
            ("WRAPFILE_TEST_FAIL_WRITE_AFTER", "10"),
        ],
        &trace,
    );
    let lines = trace_lines(&trace);
    if !trap_armed(&lines) {
        return;
    }

    // The scenario really happened: a full non-zero key was drawn, and
    // exactly 10 of its bytes reached the disk before the write failed
    // (the write events come from the permfail shim, same trace file).
    let (n, delivered) = keyfill(&lines);
    assert_eq!(n, KEY_LEN, "a full 32-byte key must have been drawn");
    assert!(
        delivered.iter().any(|&b| b != 0),
        "the drawn key must contain non-zero secret bytes"
    );
    let written: usize = lines
        .iter()
        .filter(|l| l.starts_with("WRITE off="))
        .filter_map(|l| l.split_whitespace().find_map(|kv| kv.strip_prefix("ret=")))
        .filter_map(|r| r.parse::<usize>().ok())
        .sum();
    assert_eq!(written, 10, "exactly 10 key bytes must have landed on disk");
    assert!(
        lines.iter().any(|l| l.contains("WRITE ret=-1 errno=EIO")),
        "the write must have failed unrecoverably mid-save: {lines:?}"
    );

    // The published failure contract: exit 1, no success message, an
    // explanation naming the target, the incomplete file cleaned up, and
    // pre-existing objects untouched.
    assert_eq!(run.rc, 1, "a failed save must exit 1");
    assert!(
        run.out.is_empty(),
        "no success message may be printed, got {}",
        String::from_utf8_lossy(&run.out)
    );
    let stderr = String::from_utf8_lossy(&run.err);
    assert!(
        stderr.contains("could not write key file"),
        "stderr must report the unfinished write, got: {stderr}"
    );
    assert!(
        stderr.contains("key"),
        "stderr must name the target path, got: {stderr}"
    );
    assert!(
        !key.exists(),
        "the incomplete file created by this run must be removed"
    );
    assert_no_leftovers(&tmp.path, &["sibling", "trace.log"]);
    sibling.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent directory permissions must be untouched"
    );

    // Even though 10 key bytes reached the disk, the full in-memory key
    // was wiped before the failure was reported.
    assert!(
        !contains_slice(&run.out, &delivered) && !contains_slice(&run.err, &delivered),
        "key bytes must never appear on stdout/stderr"
    );
    assert_wipe_confirmed_before_output(&lines, "2");
}

#[test]
fn shim_without_activation_leaves_keygen_unchanged() {
    let Some(so) = common::keywipe_so() else {
        eprintln!("skipping: keywipe shim not available on this target");
        return;
    };
    let preload = so.display().to_string();

    // Shim preloaded but no mode configured: it stays inert, the real OS
    // random source is used, and the usual success contract holds.
    let tmp = Tmp::new("wipe-inert");
    let key = tmp.child("key");
    let trace = prepare_trace(&tmp.path, "trace.log");

    let run = run_keygen(&key, 0o022, &preload, &[], &trace);
    assert_eq!(run.rc, 0, "stderr={}", String::from_utf8_lossy(&run.err));
    assert!(run.err.is_empty(), "stderr must be empty on success");
    assert_eq!(
        run.out,
        format!("Key saved to {}\n", key.display()).into_bytes(),
        "stdout must be exactly the save-location message"
    );
    let bytes = fs::read(&key).unwrap();
    assert_eq!(bytes.len(), KEY_LEN, "key must be exactly 32 bytes");
    assert_eq!(mode_of(&key), MODE_0600, "key file mode must be exactly 0600");
    assert!(bytes.iter().any(|&b| b != 0), "key must not be all zeroes");
    assert!(
        trace_lines(&trace).is_empty(),
        "an unconfigured shim must not intercept anything"
    );
}
