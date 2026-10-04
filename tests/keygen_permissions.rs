//! Regression coverage for the keygen file-permission contract on Unix.
//!
//! What is protected here (see README "文件权限"):
//!
//! * Under umask 0022 / 0000 / 0777 a key is generated successfully only as
//!   exactly 32 raw bytes with final mode exactly 0600 -- owner read+write,
//!   no group/other bits, no extra bits; 0400/0200/0000 are not success.
//! * The file is never group/other-accessible, not even for an instant: the
//!   mode observed right after creation already carries no group/other bits.
//! * Permissions are both set to and *confirmed* as exactly 0600 before any
//!   key byte is written. This cannot be observed from the final file, so a
//!   small LD_PRELOAD shim (tests/support/permfail) traces the program's
//!   open/fchmod/fstat/write calls from inside the process.
//! * If the filesystem rejects the required mode (fchmod fails) or the mode
//!   cannot be confirmed (fstat fails or reports something other than 0600),
//!   keygen exits 1, reports the permission problem on stderr, prints no
//!   success message, writes no key bytes, and removes the partial file --
//!   without touching pre-existing files, directories, symlinks, or the
//!   parent directory.
//! * Existing CLI behaviour (--version, usage/exit code 2) is unchanged.
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
            "wrapfile-keygen-tests-{}-{}-{}",
            std::process::id(),
            label,
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        // Don't let the harness process's own (possibly strict) umask decide
        // the scratch dir's mode: keygen needs a writable parent, and every
        // test sets the umask it cares about inside the child shell.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Tmp { path }
    }

    fn child(&self, name: impl AsRef<Path>) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        // A read-only-parent test may have stripped write permission;
        // restore it so teardown can remove the tree.
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

fn run_version() -> Run {
    let o = Command::new(bin()).arg("--version").output().unwrap();
    Run {
        rc: o.status.code().unwrap_or(-1),
        out: o.stdout,
        err: o.stderr,
    }
}

fn run_args(args: &[&str]) -> Run {
    let o = Command::new(bin()).args(args).output().unwrap();
    Run {
        rc: o.status.code().unwrap_or(-1),
        out: o.stdout,
        err: o.stderr,
    }
}

/// Run `wrapfile keygen <key>` after setting the umask inside a /bin/sh
/// wrapper (`umask MMMM; exec "$@"`), optionally with the permfail shim
/// preloaded and a trace file configured.
fn run_keygen(
    key: &Path,
    umask: u32,
    preload_so: Option<&Path>,
    extra_env: &[(&str, &str)],
    trace_file: Option<&Path>,
) -> Run {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(format!("umask {umask:04o}; exec \"$@\""))
        .arg("sh")
        .arg(bin())
        .arg("keygen")
        .arg(key);

    if let Some(so) = preload_so {
        cmd.env("LD_PRELOAD", so);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    if let Some(t) = trace_file {
        cmd.env("WRAPFILE_TEST_TRACE_FILE", t);
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
    index: usize,
}

impl Event {
    fn get(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).map(String::as_str)
    }

    fn mode(&self) -> Option<u32> {
        self.get("mode").and_then(|v| u32::from_str_radix(v, 8).ok())
    }

    fn size(&self) -> Option<u64> {
        self.get("size").and_then(|v| v.parse().ok())
    }

    fn ret(&self) -> Option<i32> {
        self.get("ret").and_then(|v| v.parse().ok())
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

fn first_index(events: &[Event], pred: impl Fn(&Event) -> bool) -> Option<usize> {
    events.iter().position(pred)
}

/// Create the trace file up front (mode 0644) so that it stays readable even
/// when the child runs under umask 0777 -- the shim opens it O_APPEND.
fn prepare_trace(dir: &Path, name: impl AsRef<Path>) -> PathBuf {
    let p = dir.join(name);
    fs::File::create(&p).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
    p
}

// ---------------------------------------------------------------------------
// filesystem assertions
// ---------------------------------------------------------------------------

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

fn assert_key_file_ok(path: &Path, run: &Run) -> Vec<u8> {
    assert_eq!(run.rc, 0, "stderr={}", String::from_utf8_lossy(&run.err));
    assert!(run.err.is_empty(), "unexpected stderr: {}", String::from_utf8_lossy(&run.err));

    let meta = fs::metadata(path).expect("key file should exist on success");
    assert_eq!(meta.len() as usize, KEY_LEN, "key must be exactly 32 bytes");

    let mode = meta.mode() & 0o7777;
    // One assertion that carries the whole contract: exactly 0600 -- owner
    // r+w both present, all group/other bits off, no setuid/setgid/sticky.
    assert_eq!(
        mode, MODE_0600,
        "key file mode must be exactly 0600 (owner rw only), got {mode:04o}"
    );

    let expected_stdout =
        format!("Key saved to {}\n", path.display()).into_bytes();
    assert_eq!(run.out, expected_stdout, "stdout should only announce the save location");

    let bytes = fs::read(path).unwrap();
    assert_eq!(bytes.len(), KEY_LEN);
    assert!(
        !contains_slice(&run.out, &bytes) && !contains_slice(&run.err, &bytes),
        "key bytes must never appear on stdout/stderr"
    );
    bytes
}

fn contains_slice(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|w| w == needle)
}

fn assert_permission_failure_cleaned(path: &Path, run: &Run, dir: &Path, allowed_entries: &[&str]) {
    assert_eq!(run.rc, 1, "permission problems must fail with exit code 1");
    assert!(run.out.is_empty(), "no success message on failure, got {}", String::from_utf8_lossy(&run.out));
    assert!(!run.err.is_empty(), "stderr must explain the failure");
    let stderr = String::from_utf8_lossy(&run.err);
    assert!(
        stderr.contains("0600") || stderr.to_lowercase().contains("permission"),
        "stderr must describe the permission problem, got: {stderr}"
    );
    assert!(!path.exists(), "the failed generation's target must be removed");

    // Nothing else left behind in the parent directory.
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
fn succeeds_with_exactly_0600_under_all_umasks() {
    let so = permfail_so();
    let mut keys: Vec<Vec<u8>> = Vec::new();

    for umask in UMASKS {
        let tmp = Tmp::new("success");
        let key = tmp.child(format!("key-{umask:04o}"));
        let trace = prepare_trace(&tmp.path, "trace.log");

        let run = run_keygen(&key, umask, so.as_deref(), &[], Some(&trace));
        let bytes = assert_key_file_ok(&key, &run);
        keys.push(bytes);

        if let Some(events) = so.as_ref().map(|_| parse_trace(&trace)) {
            // The descriptor the program created was never group/other
            // accessible, not even at birth. Under umask 0777 the owner
            // bits are also stripped at birth (0000) and repaired later.
            let born = events.iter().find(|e| e.kind == "BORN").expect("BORN event");
            assert_eq!(born.mode().unwrap() & 0o077, 0, "group/other bits must be off from creation");
            let expected_born = if umask == 0o777 { 0o000 } else { 0o600 };
            assert_eq!(born.mode().unwrap(), expected_born,
                "umask {umask:04o} should yield birth mode {expected_born:04o}");
        }
    }

    // Distinct runs draw independent random keys; none is the zero key.
    for k in &keys {
        assert!(k.iter().any(|&b| b != 0), "key must not be all zeroes");
    }
    assert_ne!(keys[0], keys[1]);
    assert_ne!(keys[1], keys[2]);
    assert_ne!(keys[0], keys[2]);
}

#[test]
fn permissions_set_and_confirmed_before_any_key_byte() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    for umask in UMASKS {
        let tmp = Tmp::new("before-write");
        let key = tmp.child("key");
        let trace = prepare_trace(&tmp.path, "trace.log");

        let run = run_keygen(&key, umask, Some(&so), &[], Some(&trace));
        assert_eq!(run.rc, 0, "stderr={}", String::from_utf8_lossy(&run.err));
        let events = parse_trace(&trace);

        let first_write = first_index(&events, |e| e.kind == "FIRST_WRITE")
            .expect("the key must eventually be written");
        let w = &events[first_write];

        // Immediately before the first write call: mode is exactly 0600 and
        // size is still 0 -- no key byte preceded the confirmation.
        assert_eq!(w.mode().unwrap(), MODE_0600,
            "mode at first write must be exactly 0600 (umask {umask:04o})");
        assert_eq!(w.size().unwrap(), 0,
            "file must be empty when 0600 is confirmed (umask {umask:04o})");

        // fchmod set 0600 and an fstat confirmed it, both before the write.
        let chmod = first_index(&events, |e| {
            e.kind == "FCHMOD" && e.ret() == Some(0) && e.mode() == Some(MODE_0600)
        }).expect("fchmod(fd, 0600) must succeed");
        let confirm = first_index(&events, |e| {
            e.kind == "FSTAT"
                && e.ret() == Some(0)
                && e.mode() == Some(MODE_0600)
                && e.size() == Some(0)
        }).expect("an fstat must confirm exactly 0600 on an empty file");
        assert!(chmod < first_write, "fchmod must precede the first write");
        assert!(confirm < first_write, "confirmation must precede the first write");

        // Every mode observation before the first write (birth included)
        // carries no group or other bits: the secret is never exposed while
        // open, even for an instant.
        for e in events.iter().take(first_write) {
            if e.kind == "BORN" || e.kind == "FSTAT" {
                if let Some(m) = e.mode() {
                    assert_eq!(m & 0o077, 0,
                        "group/other bits observed before write in event {}: {:04o}", e.index, m);
                }
            }
        }

        // Final on-disk state matches what the trace promised.
        assert_eq!(mode_of(&key), MODE_0600);
        assert_eq!(fs::metadata(&key).unwrap().len() as usize, KEY_LEN);
    }
}

#[test]
fn fchmod_rejection_fails_explains_and_cleans_up() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    for umask in UMASKS {
        let tmp = Tmp::new("fchmod-fail");
        let key = tmp.child("key");
        let sibling = Sentinel::file(&tmp.path, "sibling", b"do-not-touch", 0o644);
        let parent_mode_before = mode_of(&tmp.path);
        let trace = prepare_trace(&tmp.path, "trace.log");

        let run = run_keygen(
            &key,
            umask,
            Some(&so),
            &[("WRAPFILE_TEST_FAIL_FCHMOD", "1")],
            Some(&trace),
        );

        assert_permission_failure_cleaned(&key, &run, &tmp.path, &["sibling", "trace.log"]);
        assert!(
            String::from_utf8_lossy(&run.err).contains("0600"),
            "stderr must name the required mode"
        );
        sibling.assert_untouched();
        assert_eq!(mode_of(&tmp.path), parent_mode_before, "parent dir mode must not change");

        // The secret never reached a file whose mode could not be made 0600.
        let events = parse_trace(&trace);
        assert!(events.iter().any(|e| e.kind == "FCHMOD" && e.ret() == Some(-1)));
        assert!(
            events.iter().all(|e| e.kind != "FIRST_WRITE"),
            "no key bytes may be written when fchmod is rejected"
        );
    }
}

#[test]
fn unconfirmable_mode_fails_and_cleans_up() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    // (extra env, label): fstat itself failing, or the filesystem reporting
    // a stored mode other than 0600 (owner bits missing, or group bits added).
    let cases: &[(&[(&str, &str)], &str)] = &[
        (&[("WRAPFILE_TEST_FAIL_FSTAT", "1")], "fstat-io-failure"),
        (&[("WRAPFILE_TEST_LIE_MODE", "0000")], "reports-0000"),
        (&[("WRAPFILE_TEST_LIE_MODE", "0644")], "reports-0644"),
    ];

    for (env, label) in cases {
        let tmp = Tmp::new("unconfirmable");
        let key = tmp.child(label);
        let sibling = Sentinel::file(&tmp.path, "sibling", b"keep", 0o600);
        let trace = prepare_trace(&tmp.path, format!("{label}.log"));

        let run = run_keygen(&key, 0o022, Some(&so), env, Some(&trace));
        assert_permission_failure_cleaned(
            &key,
            &run,
            &tmp.path,
            &["sibling", &format!("{label}.log")],
        );
        sibling.assert_untouched();

        let events = parse_trace(&trace);
        assert!(
            events.iter().all(|e| e.kind != "FIRST_WRITE"),
            "no key bytes may be written before the mode is confirmed ({label})"
        );
    }
}

#[test]
fn existing_objects_are_never_modified() {
    // No shim needed: the target already exists, so creation must fail.
    for umask in UMASKS {
        // --- pre-existing regular file with a non-0600 mode and contents ---
        let tmp = Tmp::new("existing-file");
        let secret = b"pre-existing-key-material";
        let existing = Sentinel::file(&tmp.path, "key", secret, 0o640);
        let run = run_keygen(&existing.path, umask, None, &[], None);
        assert_eq!(run.rc, 1);
        assert!(run.out.is_empty());
        existing.assert_untouched();

        // --- pre-existing directory ---
        let tmp = Tmp::new("existing-dir");
        let dir = tmp.child("adir");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let run = run_keygen(&dir, umask, None, &[], None);
        assert_eq!(run.rc, 1);
        assert!(run.out.is_empty());
        assert!(dir.is_dir());
        assert_eq!(mode_of(&dir), 0o755, "existing directory mode must not change");

        // --- dangling symlink: never followed, never replaced ---
        let tmp = Tmp::new("dangling-symlink");
        let target = tmp.child("does-not-exist");
        let link = tmp.child("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let run = run_keygen(&link, umask, None, &[], None);
        assert_eq!(run.rc, 1);
        assert!(run.out.is_empty());
        let md = fs::symlink_metadata(&link).expect("symlink itself must remain");
        assert!(md.file_type().is_symlink(), "dangling symlink must not be replaced");
        assert_eq!(fs::read_link(&link).unwrap(), target);
        assert!(!target.exists(), "must not create the symlink target through the link");

        // --- missing parent directory: not auto-created ---
        let tmp = Tmp::new("missing-parent");
        let key = tmp.child("missing/sub/key");
        let run = run_keygen(&key, umask, None, &[], None);
        assert_eq!(run.rc, 1);
        assert!(run.out.is_empty());
        assert!(!tmp.child("missing").exists(), "parent directories must not be created");
    }
}

#[test]
fn failure_leaves_parent_and_siblings_untouched() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let tmp = Tmp::new("siblings");
    let key = tmp.child("key");
    let sibling = Sentinel::file(&tmp.path, "sibling", b"sibling-bytes", 0o644);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    let run = run_keygen(
        &key,
        0o022,
        Some(&so),
        &[("WRAPFILE_TEST_FAIL_FCHMOD", "1")],
        Some(&trace),
    );
    assert_eq!(run.rc, 1);
    assert!(!key.exists());
    sibling.assert_untouched();
    assert_eq!(mode_of(&tmp.path), parent_mode_before, "parent directory must be untouched");

    // A retry *without* the fault succeeds normally and produces only the key.
    let trace2 = prepare_trace(&tmp.path, "trace2.log");
    let run2 = run_keygen(&key, 0o022, Some(&so), &[], Some(&trace2));
    assert_key_file_ok(&key, &run2);
    sibling.assert_untouched();
}

#[test]
fn version_and_argument_handling_unchanged() {
    // --version
    let v = run_version();
    assert_eq!(v.rc, 0);
    assert_eq!(String::from_utf8_lossy(&v.out), "wrapfile 0.1.0\n");
    assert!(v.err.is_empty());

    let tmp = Tmp::new("cli");

    // missing path / extra args / unknown option all exit 2 and create files.
    let cases: &[&[&str]] = &[
        &["keygen"],
        &["keygen", "a", "b"],
        &["keygen", "--bogus"],
    ];
    for args in cases {
        let before: Vec<String> = fs::read_dir(&tmp.path)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let run = run_args(args);
        assert_eq!(run.rc, 2, "args {args:?} should exit 2");
        assert!(
            String::from_utf8_lossy(&run.err).contains("Usage"),
            "usage must be printed on stderr for {args:?}"
        );
        let after: Vec<String> = fs::read_dir(&tmp.path)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(before, after, "argument errors must not create files ({args:?})");
    }

    // Totally unknown invocation -> 2 as well.
    let run = run_args(&["frobnicate"]);
    assert_eq!(run.rc, 2);
}
