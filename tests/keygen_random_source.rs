//! Regression coverage for keygen's contract when the OS cryptographic
//! random source cannot deliver the 32 key bytes (see README "拒绝覆盖与
//! 路径要求": 随机源不可用 must fail the command without leaving anything
//! behind).
//!
//! What is protected here:
//!
//! * If the random source fails before a single byte is delivered, keygen
//!   exits 1, explains on stderr that the cryptographic random source is
//!   unavailable, prints nothing on stdout, and creates nothing -- no empty
//!   file, no partial key, no stand-in key, no other new files.
//! * If the source delivers some bytes and then fails before the key is
//!   complete, the same holds: the partial material is never padded with
//!   zeroes into a saved file, and it never appears on stdout/stderr in raw
//!   or hex form.
//! * A source that only returns short reads is NOT a failure: the command
//!   keeps drawing until it has all 32 bytes and succeeds normally.
//! * The failure never disturbs pre-existing directory entries: siblings,
//!   the parent directory's permissions, an existing file at the target
//!   path, and a symlink (plus its referent) all survive byte-for-byte.
//! * The injection is scoped to the faulted run: with the shim loaded but
//!   no fault configured the real OS random source is used, --version and
//!   argument errors behave as published, and a retry after a transient
//!   failure succeeds.
//!
//! The faults are injected by the LD_PRELOAD shim tests/support/randtrap,
//! which traps the guest's getrandom(2) syscall with seccomp and emulates
//! it in a SIGSYS handler (the getrandom crate issues the syscall with
//! inline assembly, so symbol interposition cannot see it). Bytes the shim
//! hands out follow a fixed pattern (0x80, 0x81, ... by draw index) so the
//! tests can recognise them in files and output.
#![cfg(unix)]

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

fn randtrap_so() -> Option<PathBuf> {
    option_env!("WRAPFILE_RANDTRAP_SO").map(PathBuf::from)
}

/// The byte the randtrap shim hands to the guest at global draw index `i`
/// (see pattern_byte() in randtrap.c).
fn pattern_byte(i: usize) -> u8 {
    (0x80 + (i % 64)) as u8
}

/// The first `n` bytes the shim would deliver.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(pattern_byte).collect()
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
            "wrapfile-keygen-random-tests-{}-{}-{}",
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
/// wrapper, with the randtrap shim preloaded and a trace file configured.
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

/// Run `wrapfile <args...>` directly, with the randtrap shim preloaded.
fn run_bin(args: &[&str], so: &Path, extra_env: &[(&str, &str)], trace: &Path) -> Run {
    let mut cmd = Command::new(bin());
    cmd.args(args)
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

/// Create the trace file up front (mode 0644) so that it stays readable
/// even when the child runs under umask 0777 -- the shim opens it O_APPEND.
fn prepare_trace(dir: &Path, name: impl AsRef<Path>) -> PathBuf {
    let p = dir.join(name);
    fs::File::create(&p).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
    p
}

/// False when the shim could not arm the seccomp trap on this system (or
/// was not loaded at all); the caller then skips instead of failing.
fn trap_armed(trace: &Path) -> bool {
    let text = fs::read_to_string(trace).unwrap();
    if text.lines().any(|l| l == "TRAP armed") {
        return true;
    }
    eprintln!("skipping: randtrap shim could not arm the seccomp trap here");
    false
}

#[derive(Debug)]
struct GrEvent {
    req: usize,
    base: usize, // global draw index of the first byte this call hands out
    ret: i64,    // negative: the call failed with the injected errno
}

impl GrEvent {
    fn failed(&self) -> bool {
        self.ret < 0
    }

    fn delivered(&self) -> usize {
        if self.ret > 0 { self.ret as usize } else { 0 }
    }

    /// The pattern bytes this call handed to the guest.
    fn bytes(&self) -> Vec<u8> {
        (self.base..self.base + self.delivered())
            .map(pattern_byte)
            .collect()
    }
}

/// The GETRANDOM lines of the trace, in call order.
fn getrandom_events(trace: &Path) -> Vec<GrEvent> {
    let text = fs::read_to_string(trace).unwrap();
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            if parts.next() != Some("GETRANDOM") {
                return None;
            }
            let mut req = None;
            let mut base = None;
            let mut ret = None;
            for kv in parts {
                let (k, v) = kv.split_once('=')?;
                match k {
                    "req" => req = v.parse().ok(),
                    "base" => base = v.parse().ok(),
                    "ret" => ret = v.parse().ok(),
                    _ => {}
                }
            }
            Some(GrEvent { req: req?, base: base?, ret: ret? })
        })
        .collect()
}

/// The events that make up keygen's own draw of the 32 key bytes. The
/// process runtime may draw randomness of its own before main() (and the
/// getrandom crate probes the syscall once with a zero-length request), so
/// the key fill is identified as the final run of calls starting with a
/// full-length (32-byte) request.
fn key_fill_events(events: &[GrEvent]) -> &[GrEvent] {
    let start = events
        .iter()
        .rposition(|e| e.req == KEY_LEN)
        .expect("keygen must attempt to draw the 32 key bytes");
    &events[start..]
}

/// The bytes the key fill received, reconstructed from the trace.
fn key_fill_bytes(events: &[GrEvent]) -> Vec<u8> {
    key_fill_events(events)
        .iter()
        .flat_map(GrEvent::bytes)
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

/// The failure contract for an unusable random source: exit 1, an
/// explanation naming the random source on stderr, nothing on stdout, and
/// no trace of this invocation left in the parent directory -- in
/// particular no file at the target path that could be mistaken for a key
/// (empty, partial, zero-padded, or a full-length stand-in).
fn assert_failed_random(key: &Path, run: &Run, dir: &Path, allowed_entries: &[&str]) {
    assert_eq!(run.rc, 1, "an unfinishable random draw must exit 1");
    assert!(
        run.out.is_empty(),
        "stdout must stay empty on random-source failure, got {}",
        String::from_utf8_lossy(&run.out)
    );
    let stderr = String::from_utf8_lossy(&run.err);
    assert!(
        stderr.contains("cryptographic random source unavailable"),
        "stderr must state that the cryptographic random source is \
         unavailable (without pinning the OS-specific error text), got: {stderr}"
    );

    assert!(
        !key.exists(),
        "no file may be left at the target path after a random-source failure"
    );
    assert_no_leftovers(dir, allowed_entries);
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

#[test]
fn random_source_unavailable_before_any_byte_saves_nothing() {
    let Some(so) = randtrap_so() else {
        eprintln!("skipping: randtrap shim not available on this target");
        return;
    };

    let tmp = Tmp::new("rand-fail-empty");
    let key = tmp.child("key");
    let sibling = Sentinel::file(&tmp.path, "sibling", b"do-not-touch", 0o644);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    // The random source reports it cannot deliver anything at all.
    let run = run_keygen(
        &key,
        0o022,
        &so,
        &[("WRAPFILE_TEST_GETRANDOM_FAIL", "1")],
        &trace,
    );
    if !trap_armed(&trace) {
        return;
    }

    // The injected failure really was hit, before any key byte was drawn.
    let events = getrandom_events(&trace);
    let fill = key_fill_events(&events);
    assert!(
        fill.iter().all(GrEvent::failed),
        "no key byte may have been delivered in this scenario: {fill:?}"
    );

    assert_failed_random(&key, &run, &tmp.path, &["sibling", "trace.log"]);
    sibling.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent directory permissions must be untouched"
    );

    // The failure was transient: a retry without the fault saves a full key.
    let trace2 = prepare_trace(&tmp.path, "trace2.log");
    let run2 = run_keygen(&key, 0o022, &so, &[], &trace2);
    assert_eq!(run2.rc, 0, "stderr={}", String::from_utf8_lossy(&run2.err));
    let bytes = fs::read(&key).unwrap();
    assert_eq!(bytes.len(), KEY_LEN, "the retry must save a full 32-byte key");
    assert_eq!(mode_of(&key), MODE_0600);
    sibling.assert_untouched();
}

#[test]
fn random_source_failure_after_partial_bytes_saves_nothing() {
    let Some(so) = randtrap_so() else {
        eprintln!("skipping: randtrap shim not available on this target");
        return;
    };

    let tmp = Tmp::new("rand-fail-partial");
    let key = tmp.child("key");
    let sibling = Sentinel::file(&tmp.path, "sibling", b"keep-me", 0o600);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    // The random source dries up after handing out 18 bytes in total: the
    // process runtime's own startup draw consumes some of that, and the
    // key fill gets the rest -- some bytes, but fewer than 32 -- before
    // the source fails for good.
    let run = run_keygen(
        &key,
        0o022,
        &so,
        &[("WRAPFILE_TEST_GETRANDOM_FAIL_AFTER", "18")],
        &trace,
    );
    if !trap_armed(&trace) {
        return;
    }

    // The scenario really happened: the key fill drew some but not all of
    // the 32 bytes, and then the draw failed unrecoverably.
    let events = getrandom_events(&trace);
    let fill = key_fill_events(&events);
    let drawn = key_fill_bytes(&events);
    assert!(
        !drawn.is_empty() && drawn.len() < KEY_LEN,
        "the key fill must have drawn some but not all of the 32 bytes: {fill:?}"
    );
    assert!(
        fill.last().is_some_and(GrEvent::failed),
        "the key fill must have ended in the injected unrecoverable error: {fill:?}"
    );

    assert_failed_random(&key, &run, &tmp.path, &["sibling", "trace.log"]);
    sibling.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent directory permissions must be untouched"
    );

    // The bytes already drawn are secret key material in waiting: they must
    // not leak to stdout/stderr, neither as raw bytes nor as text-encoded
    // (hex) diagnostics, and they must not be padded with zeroes into a
    // saved file -- there is no file at all.
    assert!(
        !contains_slice(&run.out, &drawn) && !contains_slice(&run.err, &drawn),
        "partially drawn random bytes must never appear on stdout/stderr"
    );
    let hex: String = drawn.iter().map(|b| format!("{b:02x}")).collect();
    assert!(
        !String::from_utf8_lossy(&run.out).contains(&hex)
            && !String::from_utf8_lossy(&run.err).contains(&hex),
        "partially drawn random bytes must not appear hex-encoded either"
    );
}

#[test]
fn short_reads_from_random_source_still_complete_the_key() {
    let Some(so) = randtrap_so() else {
        eprintln!("skipping: randtrap shim not available on this target");
        return;
    };

    for umask in UMASKS {
        let tmp = Tmp::new("rand-short-reads");
        let key = tmp.child("key");
        let trace = prepare_trace(&tmp.path, "trace.log");

        // A source that only ever returns 7 bytes per call but always
        // succeeds: not a failure, the command must keep drawing.
        let run = run_keygen(
            &key,
            umask,
            &so,
            &[("WRAPFILE_TEST_GETRANDOM_PARTIAL", "7")],
            &trace,
        );
        if !trap_armed(&trace) {
            return;
        }

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

        // Several short reads really happened and none was treated as a
        // failure.
        let events = getrandom_events(&trace);
        let fill = key_fill_events(&events);
        assert!(
            fill.len() >= 2,
            "short reads should mean multiple getrandom calls: {fill:?}"
        );
        assert!(
            fill.iter().all(|e| !e.failed() && e.delivered() <= 7),
            "every call was a successful short read: {fill:?}"
        );

        // Every byte in the file came from the random source, in order --
        // nothing re-fetched, invented, or zero-padded.
        let expected = key_fill_bytes(&events);
        assert_eq!(expected.len(), KEY_LEN, "the draws must add up to 32 bytes");
        let bytes = fs::read(&key).unwrap();
        assert_eq!(bytes.len(), KEY_LEN, "key must be exactly 32 bytes");
        assert_eq!(
            bytes, expected,
            "the file must hold exactly the bytes the source delivered"
        );
        assert_eq!(
            mode_of(&key),
            MODE_0600,
            "key file mode must be exactly 0600 (umask {umask:04o})"
        );
    }
}

#[test]
fn random_failure_preserves_existing_file_target() {
    let Some(so) = randtrap_so() else {
        eprintln!("skipping: randtrap shim not available on this target");
        return;
    };

    let tmp = Tmp::new("rand-fail-existing-file");
    // The target path is already taken by a regular file.
    let target = Sentinel::file(&tmp.path, "key", b"existing-content", 0o640);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    let run = run_keygen(
        &tmp.child("key"),
        0o022,
        &so,
        &[("WRAPFILE_TEST_GETRANDOM_FAIL", "1")],
        &trace,
    );
    if !trap_armed(&trace) {
        return;
    }

    assert_eq!(run.rc, 1, "the command must fail");
    assert!(
        run.out.is_empty(),
        "no success message may be printed, got {}",
        String::from_utf8_lossy(&run.out)
    );
    // The random failure must not delete, rewrite, or re-permission the
    // file that was already there.
    target.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent directory permissions must be untouched"
    );
    assert_no_leftovers(&tmp.path, &["key", "trace.log"]);
}

#[test]
fn random_failure_preserves_symlink_target_and_referent() {
    let Some(so) = randtrap_so() else {
        eprintln!("skipping: randtrap shim not available on this target");
        return;
    };

    let tmp = Tmp::new("rand-fail-symlink");
    let referent = Sentinel::file(&tmp.path, "real", b"real-data", 0o600);
    let link = tmp.child("key");
    std::os::unix::fs::symlink(&referent.path, &link).unwrap();
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    let run = run_keygen(
        &link,
        0o022,
        &so,
        &[("WRAPFILE_TEST_GETRANDOM_FAIL", "1")],
        &trace,
    );
    if !trap_armed(&trace) {
        return;
    }

    assert_eq!(run.rc, 1, "the command must fail");
    assert!(run.out.is_empty(), "no success message may be printed");
    // The link itself is still the same symlink...
    let meta = fs::symlink_metadata(&link).expect("the symlink must still exist");
    assert!(meta.file_type().is_symlink(), "the symlink must not be replaced");
    assert_eq!(
        fs::read_link(&link).unwrap(),
        referent.path,
        "the symlink must still point where it pointed"
    );
    // ...and the object it points to is untouched.
    referent.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent directory permissions must be untouched"
    );
    assert_no_leftovers(&tmp.path, &["real", "key", "trace.log"]);
}

#[test]
fn shim_without_fault_uses_the_real_random_source() {
    let Some(so) = randtrap_so() else {
        eprintln!("skipping: randtrap shim not available on this target");
        return;
    };

    let tmp = Tmp::new("rand-real");
    let key = tmp.child("key");
    let trace = prepare_trace(&tmp.path, "trace.log");

    // Shim preloaded but no fault configured: the real OS random source is
    // used and the usual success contract holds.
    let run = run_keygen(&key, 0o022, &so, &[], &trace);
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
    assert_ne!(
        bytes,
        pattern(KEY_LEN),
        "without a configured fault the shim must not fabricate key bytes"
    );
    assert!(
        !contains_slice(&run.out, &bytes) && !contains_slice(&run.err, &bytes),
        "key bytes must never appear on stdout/stderr"
    );
    // The shim never armed, so it trapped nothing.
    assert!(
        getrandom_events(&trace).is_empty(),
        "an unconfigured shim must not intercept the random source"
    );
}

#[test]
fn injection_does_not_affect_version_or_argument_handling() {
    let Some(so) = randtrap_so() else {
        eprintln!("skipping: randtrap shim not available on this target");
        return;
    };

    let tmp = Tmp::new("rand-fail-cli");
    let trace = prepare_trace(&tmp.path, "trace.log");
    let fault = [("WRAPFILE_TEST_GETRANDOM_FAIL", "1")];

    // --version never touches the random source, fault or no fault.
    let run = run_bin(&["--version"], &so, &fault, &trace);
    if !trap_armed(&trace) {
        return;
    }
    assert_eq!(run.rc, 0);
    assert_eq!(run.out, b"wrapfile 0.1.0\n");
    assert!(run.err.is_empty());

    // Argument errors are still rejected with exit code 2 and usage on
    // stderr, before any random draw happens.
    let run = run_bin(&["keygen"], &so, &fault, &trace);
    assert_eq!(run.rc, 2);
    assert!(run.out.is_empty());
    assert!(
        String::from_utf8_lossy(&run.err).contains("Usage:"),
        "usage must be shown for a missing key file path"
    );
    assert_no_leftovers(&tmp.path, &["trace.log"]);
}
