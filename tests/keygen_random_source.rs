//! Regression coverage for keygen when the OS cryptographic random source
//! cannot supply the key bytes (see README "拒绝覆盖与路径要求", which
//! promises failure when "随机源不可用").
//!
//! The program draws all 32 key bytes *before* creating anything at the
//! target path, so a random-source failure must look exactly like an
//! operation that never started:
//!
//! * exit code 1;
//! * stderr states that the cryptographic random source is unavailable, and
//!   includes the reason the source gave (only a stable, OS-independent
//!   phrasing is asserted -- the trailing OS error text varies with libc);
//! * stdout is completely empty (no save-location message);
//! * the target carries neither an empty file, a partial key, a zero-padded
//!   "key", nor a plausible-looking replacement key; nothing else this
//!   invocation created is left in the parent directory;
//! * any random bytes already obtained never appear on stdout/stderr, as raw
//!   bytes or in a text encoding (hex dump).
//!
//! Two failure timings are injected with the permfail LD_PRELOAD shim:
//! failure before a single random byte is obtained, and failure after a
//! prefix of the key was obtained but before the 32 bytes were complete.
//! Short reads and EINTR that merely interrupt a call are *not* failures:
//! the program keeps drawing until the key is complete and saves normally.
//!
//! The faults are reached only when getrandom's random acquisition goes
//! through libc's getrandom(2) (interposable by the shim); on Linux the
//! checked-in .cargo/config.toml selects that backend for test builds.
#![cfg(unix)]

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const KEY_LEN: usize = 32;

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

/// Run `wrapfile keygen <key>` with the permfail shim preloaded, the given
/// WRAPFILE_TEST_* knobs, and a shim trace file.
fn run_keygen(key: &Path, so: &Path, extra_env: &[(&str, &str)], trace: &Path) -> Run {
    let o = Command::new(bin())
        .arg("keygen")
        .arg(key)
        .env("LD_PRELOAD", so)
        .env("WRAPFILE_TEST_TRACE_FILE", trace)
        .envs(extra_env.iter().copied())
        .output()
        .unwrap();
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

/// Create the trace file up front so the shim can append to it.
fn prepare_trace(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    fs::File::create(&p).unwrap();
    p
}

/// Concatenation of the bytes successful getrandom() calls produced, in
/// call order.
fn obtained_random_bytes(events: &[Event]) -> Vec<u8> {
    events
        .iter()
        .filter(|e| e.kind == "GETRANDOM" && e.ret().is_some_and(|r| r > 0))
        .flat_map(|e| e.hex_bytes().expect("trace must carry hex bytes"))
        .collect()
}

fn contains_slice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------------------
// filesystem assertions
// ---------------------------------------------------------------------------

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

/// The random-source failure contract: exit 1, empty stdout, an explanation
/// on stderr that names the cryptographic random source and a reason, no
/// file at the target, nothing else left behind, and none of the random
/// bytes obtained before the failure (raw or hex-encoded) in the output.
fn assert_random_failure(
    key: &Path,
    run: &Run,
    dir: &Path,
    allowed_entries: &[&str],
    obtained: &[u8],
) {
    assert_eq!(run.rc, 1, "an unavailable random source must exit 1");
    assert!(
        run.out.is_empty(),
        "stdout must stay empty when no key could be generated, got {}",
        String::from_utf8_lossy(&run.out)
    );

    let stderr = String::from_utf8_lossy(&run.err);
    let lower = stderr.to_lowercase();
    assert!(
        lower.contains("cryptographic random source unavailable"),
        "stderr must state the cryptographic random source is unavailable, got: {stderr}"
    );
    // The reason the source gave must follow the stable prefix, but the
    // test must not depend on the exact OS/libc wording ("Input/output
    // error" vs "OS Error: 5" vs locale text): a non-empty reason after
    // "unavailable:" is the contract.
    let after = lower
        .split("cryptographic random source unavailable")
        .nth(1)
        .expect("prefix presence was just checked");
    let reason = after.strip_prefix(':').unwrap_or(after);
    assert!(
        !reason.trim().is_empty(),
        "stderr must include the reason the random source gave, got: {stderr}"
    );

    // No target object in any state -- absent, not empty/partial/complete.
    assert!(
        fs::symlink_metadata(key).is_err(),
        "no file, symlink or other object may be left at the target path"
    );

    // Nothing else created by this invocation in the parent directory.
    let mut leftovers: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !allowed_entries.contains(&n.as_str()))
        .collect();
    leftovers.sort();
    assert!(leftovers.is_empty(), "unexpected leftover files: {leftovers:?}");

    // The random bytes obtained before the failure are discarded, never
    // dumped -- neither verbatim nor in a hex/text encoding.
    if !obtained.is_empty() {
        assert!(
            !contains_slice(&run.out, obtained) && !contains_slice(&run.err, obtained),
            "obtained random bytes must not appear verbatim in output"
        );
        let hex: Vec<u8> = obtained
            .iter()
            .flat_map(|b| format!("{b:02x}").into_bytes())
            .collect();
        let hex_upper: Vec<u8> = obtained
            .iter()
            .flat_map(|b| format!("{b:02X}").into_bytes())
            .collect();
        assert!(
            !run.out.windows(hex.len()).any(|w| w == hex)
                && !run.err.windows(hex.len()).any(|w| w == hex)
                && !run.out.windows(hex_upper.len()).any(|w| w == hex_upper)
                && !run.err.windows(hex_upper.len()).any(|w| w == hex_upper),
            "obtained random bytes must not appear hex-encoded in output"
        );
    }
}

/// The normal success contract after a fault-free retry.
fn assert_success(key: &Path, run: &Run) {
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
    assert_eq!(meta.mode() & 0o7777, 0o600, "key file mode must be exactly 0600");
    let bytes = fs::read(key).unwrap();
    assert_eq!(bytes.len(), KEY_LEN);
    assert!(bytes.iter().any(|&b| b != 0), "key must not be all zeroes");
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

/// The random source fails before producing even one byte: the command must
/// fail exactly as if it had never touched the filesystem.
#[test]
fn random_source_failure_before_any_byte_saves_nothing() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let tmp = Tmp::new("rand-immediate");
    let key = tmp.child("key");
    let sibling = Sentinel::file(&tmp.path, "sibling", b"do-not-touch", 0o644);
    let parent_mode_before = mode_of(&tmp.path);
    let trace = prepare_trace(&tmp.path, "trace.log");

    let run = run_keygen(
        &key,
        &so,
        &[
            // The very first draw fails irrecoverably (EIO); the partial
            // knob is irrelevant here but keeps the scenario unambiguous.
            ("WRAPFILE_TEST_RAND_FAIL_AFTER", "0"),
            ("WRAPFILE_TEST_TRACE_BYTES", "1"),
        ],
        &trace,
    );

    let events = parse_trace(&trace);
    assert!(
        events
            .iter()
            .any(|e| e.kind == "GETRANDOM" && e.ret() == Some(-1)),
        "the random-source failure must have been injected"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.kind == "GETRANDOM" && e.ret().is_some_and(|r| r > 0)),
        "no random byte may have been obtained before the failure"
    );
    // Randomness is requested before the target path is ever opened: the
    // failure leaves the filesystem exactly as it was.
    let first_random = events.iter().position(|e| e.kind == "GETRANDOM");
    let first_open = events.iter().position(|e| e.kind == "OPEN");
    assert!(first_random.is_some(), "a random draw must have been attempted");
    assert!(
        first_open.is_none()
            || first_random.is_some_and(|r| first_open.is_some_and(|o| r < o)),
        "random bytes must be drawn before the key file is created"
    );

    assert_random_failure(&key, &run, &tmp.path, &["sibling", "trace.log"], &[]);
    sibling.assert_untouched();
    assert_eq!(
        mode_of(&tmp.path),
        parent_mode_before,
        "parent directory permissions must be untouched"
    );

    // The fault was transient: a retry on a healthy random source saves a
    // full, valid key.
    let trace2 = prepare_trace(&tmp.path, "trace2.log");
    let run2 = run_keygen(&key, &so, &[], &trace2);
    assert_success(&key, &run2);
    sibling.assert_untouched();
}

/// The random source hands over part of the key, then fails irrecoverably
/// before the 32 bytes are complete. The partial bytes must not be
/// zero-padded into a file, must not be printed in any form, and the target
/// must never come into existence.
#[test]
fn random_source_failure_after_partial_bytes_saves_nothing() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    // Several prefix lengths: the guarantee must not depend on where in the
    // 32-byte draw the source gives up.
    for prefix in [1usize, 7, 12, 31] {
        let tmp = Tmp::new("rand-partial");
        let key = tmp.child(format!("key-{prefix}"));
        let sibling = Sentinel::file(&tmp.path, "sibling", b"keep-me", 0o600);
        let parent_mode_before = mode_of(&tmp.path);
        let trace = prepare_trace(&tmp.path, &format!("trace-{prefix}.log"));

        let run = run_keygen(
            &key,
            &so,
            &[
                // Short 5-byte reads so the prefix spans several calls,
                // then EIO once `prefix` bytes have been obtained.
                ("WRAPFILE_TEST_RAND_PARTIAL", "5"),
                ("WRAPFILE_TEST_RAND_FAIL_AFTER", &prefix.to_string()),
                ("WRAPFILE_TEST_TRACE_BYTES", "1"),
            ],
            &trace,
        );

        let events = parse_trace(&trace);
        let obtained = obtained_random_bytes(&events);
        assert_eq!(
            obtained.len(),
            prefix,
            "exactly {prefix} random bytes must have been obtained, then failure"
        );
        assert!(
            events
                .iter()
                .any(|e| e.kind == "GETRANDOM" && e.get("errno") == Some("EIO")),
            "the unrecoverable random-source error must have been injected (prefix={prefix})"
        );
        // Every obtained byte came from the real source: the trace carried
        // exactly `prefix` bytes (checked above), which the program had to
        // discard rather than pad to 32 and save.

        assert_random_failure(
            &key,
            &run,
            &tmp.path,
            &["sibling", &format!("trace-{prefix}.log")],
            &obtained,
        );
        sibling.assert_untouched();
        assert_eq!(
            mode_of(&tmp.path),
            parent_mode_before,
            "parent directory permissions must be untouched"
        );

        // No partial/zero-padded lookalike can be hiding under another name
        // either: the only regular files are the sentinel and the trace.
        let entries: Vec<String> = fs::read_dir(&tmp.path)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries.len(),
            2,
            "no replacement or temporary key file may be created, found {entries:?}"
        );
    }
}

/// A short read (fewer bytes than requested, call still succeeds) and an
/// EINTR interruption are ordinary conditions, not random-source failures:
/// keygen must keep drawing and save exactly one complete 32-byte key.
#[test]
fn short_and_interrupted_random_reads_still_complete_the_key() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let cases: &[(&[(&str, &str)], &str)] = &[
        // One byte at a time: 32 successful short reads, no error at all.
        (&[("WRAPFILE_TEST_RAND_PARTIAL", "1")], "one-byte-reads"),
        // Seven bytes at a time leaves ragged boundaries (32 = 4*7+4).
        (&[("WRAPFILE_TEST_RAND_PARTIAL", "7")], "seven-byte-reads"),
        // First four calls interrupted with EINTR before producing bytes,
        // then 9-byte short reads complete the key: still one generation.
        (
            &[
                ("WRAPFILE_TEST_RAND_EINTR", "4"),
                ("WRAPFILE_TEST_RAND_PARTIAL", "9"),
            ],
            "eintr-then-short",
        ),
    ];

    for (env, label) in cases {
        let tmp = Tmp::new("rand-recover");
        let key = tmp.child("key");
        let trace = prepare_trace(&tmp.path, &format!("{label}.log"));

        let mut full_env: Vec<(&str, &str)> = env.to_vec();
        full_env.push(("WRAPFILE_TEST_TRACE_BYTES", "1"));
        let run = run_keygen(&key, &so, &full_env, &trace);
        assert_success(&key, &run);

        let events = parse_trace(&trace);
        let eintrs = events
            .iter()
            .filter(|e| e.kind == "GETRANDOM" && e.get("errno") == Some("EINTR"))
            .count();
        if *label == "eintr-then-short" {
            assert_eq!(eintrs, 4, "all injected EINTRs must have been observed");
        } else {
            assert_eq!(eintrs, 0);
        }
        // Successful draws tile exactly [0, 32): no byte drawn twice, none
        // dropped, none replaced by zeroes.
        let mut expect = 0u64;
        for e in events
            .iter()
            .filter(|e| e.kind == "GETRANDOM" && e.ret().is_some_and(|r| r > 0))
        {
            assert_eq!(e.off().unwrap(), expect, "short reads must resume where the previous ended ({label})");
            expect += e.ret().unwrap() as u64;
        }
        assert_eq!(expect as usize, KEY_LEN, "draws must total exactly 32 bytes ({label})");

        // The bytes drawn are exactly the bytes saved: no re-draw with
        // different contents, no padding.
        assert_eq!(obtained_random_bytes(&events), fs::read(&key).unwrap());
    }
}

/// With the shim loaded but no fault knobs set, every other invocation still
/// draws from the real OS random source: the test scaffolding must not make
/// random acquisition fail or stall on its own.
#[test]
fn preloaded_shim_without_fault_knobs_uses_the_real_source() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let mut seen = Vec::new();
    for i in 0..2 {
        let tmp = Tmp::new("rand-real");
        let key = tmp.child(format!("key-{i}"));
        let trace = prepare_trace(&tmp.path, "trace.log");
        // Deliberately no WRAPFILE_TEST_RAND_* variables at all.
        let run = run_keygen(&key, &so, &[], &trace);
        let bytes = {
            assert_success(&key, &run);
            fs::read(&key).unwrap()
        };
        let events = parse_trace(&trace);
        assert!(
            events
                .iter()
                .any(|e| e.kind == "GETRANDOM" && e.ret().is_some_and(|r| r > 0)),
            "random bytes must come through the (transparent) shim"
        );
        assert!(
            events
                .iter()
                .all(|e| !(e.kind == "GETRANDOM" && e.ret() == Some(-1))),
            "no random call may fail without a fault knob"
        );
        seen.push(bytes);
    }
    // Independent real draws; trivial collision probability, and guards
    // against the shim accidentally returning a fixed buffer.
    assert_ne!(seen[0], seen[1], "two keys must be independently random");
}

/// If the target already exists (regular file or symlink) and the random
/// source also fails, the pre-existing object -- and what the link points
/// at -- must survive untouched. Random acquisition happens first, so this
/// only requires that its error path performs no cleanup of objects it did
/// not create; the test pins that behavior.
#[test]
fn random_failure_never_touches_an_existing_target() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    // --- pre-existing regular file ---
    let tmp = Tmp::new("rand-existing-file");
    let existing = Sentinel::file(&tmp.path, "key", b"pre-existing-key-material", 0o640);
    let trace = prepare_trace(&tmp.path, "trace.log");
    let run = run_keygen(
        &existing.path,
        &so,
        &[("WRAPFILE_TEST_RAND_FAIL_AFTER", "0")],
        &trace,
    );
    assert_eq!(run.rc, 1);
    assert!(run.out.is_empty());
    existing.assert_untouched();

    // --- pre-existing symlink, including its pointed-to object ---
    let tmp = Tmp::new("rand-existing-link");
    let backing = Sentinel::file(&tmp.path, "backing", b"linked-target-bytes", 0o600);
    let link = tmp.child("link");
    std::os::unix::fs::symlink(&backing.path, &link).unwrap();
    let trace = prepare_trace(&tmp.path, "trace.log");
    let run = run_keygen(
        &link,
        &so,
        &[
            ("WRAPFILE_TEST_RAND_PARTIAL", "6"),
            ("WRAPFILE_TEST_RAND_FAIL_AFTER", "12"),
            ("WRAPFILE_TEST_TRACE_BYTES", "1"),
        ],
        &trace,
    );
    assert_eq!(run.rc, 1);
    assert!(run.out.is_empty());
    let md = fs::symlink_metadata(&link).expect("symlink itself must remain");
    assert!(md.file_type().is_symlink(), "symlink must not be replaced");
    assert_eq!(fs::read_link(&link).unwrap(), backing.path, "link target must not change");
    backing.assert_untouched();

    // The partial prefix never reached the filesystem.
    let events = parse_trace(&trace);
    assert_eq!(obtained_random_bytes(&events).len(), 12);
    assert!(
        fs::read_dir(&tmp.path)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .all(|n| n == "backing" || n == "link" || n == "trace.log"),
        "no stray file may be created on the random failure"
    );
}
