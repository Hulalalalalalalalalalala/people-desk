//! Regression coverage for the temporary key's in-memory lifecycle on Unix:
//! the 32 bytes the OS random source delivers are held directly in one
//! stack buffer while the key is saved, and that whole buffer is overwritten
//! with zeroes *before the keygen operation returns* -- under both the
//! unoptimized and the optimized (release) build.
//!
//! Why this needs more than the existing tests
//! --------------------------------------------
//! File deletion, an empty stdout/stderr, and a 0/1 exit code cannot prove
//! the wipe happened: the whole process image vanishes on exit, and even a
//! still-secret stack slot could be overwritten later by an unrelated frame.
//! The promise in README ("内存中的临时密钥清零") is about the buffer that
//! directly held the random bytes while the operation was in flight, so the
//! buffer has to be inspected *during* the run:
//!
//!   * that non-zero secret bytes really were delivered and really were
//!     held through the save (the saved file must still be that exact key,
//!     not zeroes and not truncated);
//!   * and that those bytes were all overwritten before the operation
//!     returned (in particular before its stdout/stderr report).
//!
//! The tests drive the real wrapfile binary under
//! `tests/support/keylife/keylife`, an *external* ptrace observer (not an
//! LD_PRELOAD shim): it forks, its child PTRACE_TRACEME's itself and execs
//! wrapfile, and the observer locates the 32-byte buffer at the getrandom
//! syscall, reads it across the file operations, and single-steps the guest
//! after the save (success -- including a close that merely reports EINTR),
//! after the emulated random-source failure, after a mid-save write failure,
//! after a permission setup (fchmod) or confirmation (fstat) failure, and
//! after an unrecoverable close error (with the cleanup both allowed
//! and refused) to watch the whole buffer become zero before the first
//! stdout/stderr write. For a partial-draw failure it also plants a
//! non-zero sentinel in the slots the random source never delivered (those
//! slots stay zero from initialization), so a wipe covering only the
//! delivered prefix is caught too.
//!
//! The random source is made deterministic and fail-able by the existing
//! randtrap shim (fixed 0x80.. byte pattern; seccomp SIGSYS emulation), and
//! the write, close, and permission-stage failures by the existing permfail
//! shim. Nothing in the product changes.
//!
//! Every scenario is run against the debug binary (`CARGO_BIN_EXE_wrapfile`)
//! and a freshly built `--release` binary, since an optimizer deleting a
//! dead plain `fill(0)` is precisely one of the regressions this guards.
#![cfg(unix)]

mod common;

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const KEY_LEN: usize = 32;
const MODE_0600: u32 = 0o600;

// ---------------------------------------------------------------------------
// binaries: the profile under which `cargo test` runs, plus a release build
// ---------------------------------------------------------------------------

fn debug_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_wrapfile"))
}

/// Ensure a release/optimized wrapfile is available and return its path.
/// `None` (reason printed) if one cannot be produced here, so the release
/// cases skip instead of failing.
///
/// When the tests themselves run under `cargo test --release`, the test
/// binary already lives in target/release and that wrapfile is reused.
/// Otherwise a release wrapfile is built into a *separate* target
/// directory under CARGO_TARGET_TMPDIR: running `cargo build` against the
/// main target dir from inside a test would block on Cargo's target lock
/// for as long as the whole test process runs (a self-deadlock).
fn release_bin() -> Option<PathBuf> {
    static READY: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    READY.get_or_init(|| {
        let debug = debug_bin();
        if debug
            .components()
            .any(|c| c.as_os_str() == "release")
        {
            return Some(debug);
        }

        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("wrapfile-release-for-zeroing-tests");
        let _ = fs::create_dir_all(&scratch);
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let status = Command::new(&cargo)
            .args(["build", "--release", "--offline", "--quiet"])
            .current_dir(&manifest)
            .env("CARGO_TARGET_DIR", &scratch)
            .status();
        let path = scratch.join("release").join("wrapfile");
        match status {
            Ok(s) if s.success() && path.is_file() => Some(path),
            Ok(s) => {
                eprintln!(
                    "skipping release-profile zeroing cases: building the \
                     release guest exited with {s}"
                );
                None
            }
            Err(e) => {
                eprintln!(
                    "skipping release-profile zeroing cases: cannot run \
                     `{cargo}` to build the release guest: {e}"
                );
                None
            }
        }
    })
    .clone()
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
            "wrapfile-keygen-zeroing-tests-{}-{}-{}",
            std::process::id(),
            label,
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Tmp { path }
    }

    fn child(&self, name: &str) -> PathBuf {
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
// observer event stream
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Event {
    name: String,
    attrs: HashMap<String, String>,
}

impl Event {
    fn get(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).map(String::as_str)
    }
    fn usize(&self, key: &str) -> Option<usize> {
        self.get(key).and_then(|v| v.parse().ok())
    }
    fn i64(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(|v| v.parse().ok())
    }
}

struct Observation {
    rc: i32,
    observer_err: String,
    events: Vec<Event>,
    fail: Option<String>,
}

impl Observation {
    fn named(&self, name: &str) -> Vec<&Event> {
        self.events.iter().filter(|e| e.name == name).collect()
    }
    fn first(&self, name: &str) -> Option<&Event> {
        self.events.iter().find(|e| e.name == name)
    }
    fn index(&self, name: &str) -> Option<usize> {
        self.events.iter().position(|e| e.name == name)
    }
    fn secret(&self) -> Vec<u8> {
        let hex = self
            .first("FILL_COMPLETE")
            .and_then(|e| e.get("secret"))
            .expect("FILL_COMPLETE secret=");
        assert_eq!(hex.len() % 2, 0);
        (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }
}

fn contains_slice(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

fn parse_events(stdout: &[u8]) -> Vec<Event> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let tag = it.next()?;
            if tag != "EVENT" {
                return None;
            }
            let mut attrs = HashMap::new();
            let mut name = String::new();
            for kv in it {
                let (k, v) = kv.split_once('=')?;
                if k == "name" {
                    name = v.to_string();
                } else {
                    attrs.insert(k.to_string(), v.to_string());
                }
            }
            (!name.is_empty()).then_some(Event { name, attrs })
        })
        .collect()
}

fn fail_reason(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .find_map(|l| l.strip_prefix("FAIL reason=").map(str::to_string))
}

/// Run the keylife observer around `wrapfile keygen <key>` with the given
/// LD_PRELOAD libraries and extra environment.
fn observe(
    observer: &Path,
    bin: &Path,
    scenario: &str,
    key: &Path,
    preloads: &[PathBuf],
    extra_env: &[(&str, &str)],
    trace: &Path,
) -> Observation {
    let dir = key.parent().unwrap();
    let out_cap = dir.join("guest.stdout");
    let err_cap = dir.join("guest.stderr");
    for f in [&out_cap, &err_cap] {
        let _ = fs::remove_file(f);
    }

    let preload = preloads
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(":");

    let output = Command::new(observer)
        .arg(scenario)
        .arg(&out_cap)
        .arg(&err_cap)
        .arg("--")
        .arg(bin)
        .arg("keygen")
        .arg(key)
        .env("LD_PRELOAD", preload)
        .env("WRAPFILE_TEST_TRACE_FILE", trace)
        .envs(extra_env.iter().copied())
        .output()
        .expect("run keylife observer");

    Observation {
        rc: output.status.code().unwrap_or(-1),
        observer_err: String::from_utf8_lossy(&output.stderr).into_owned(),
        events: parse_events(&output.stdout),
        fail: fail_reason(&output.stdout),
    }
}

fn guest_captured(key: &Path) -> (Vec<u8>, Vec<u8>) {
    let dir = key.parent().unwrap();
    let out = fs::read(dir.join("guest.stdout")).unwrap_or_default();
    let err = fs::read(dir.join("guest.stderr")).unwrap_or_default();
    (out, err)
}

// ---------------------------------------------------------------------------
// shim / observer availability
// ---------------------------------------------------------------------------

struct Support {
    observer: PathBuf,
    randtrap: PathBuf,
    permfail: PathBuf,
}

fn support() -> Option<Support> {
    let observer = common::keylife_bin()?;
    let randtrap = common::randtrap_so()?;
    let permfail = common::permfail_so()?;
    Some(Support { observer, randtrap, permfail })
}

/// True when the randtrap shim armed its seccomp trap in the guest (a
/// machine without seccomp must skip, not fail).
fn trap_armed(trace: &Path) -> bool {
    match fs::read_to_string(trace) {
        Ok(t) => t.lines().any(|l| l == "TRAP armed"),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// randtrap trace helpers
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct GrEvent {
    req: usize,
    base: usize,
    ret: i64,
}

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

fn pattern_byte(i: usize) -> u8 {
    (0x80 + (i % 64)) as u8
}

/// The random bytes the key fill received, reconstructed from the trace.
fn key_fill_bytes(events: &[GrEvent]) -> Vec<u8> {
    let start = events
        .iter()
        .rposition(|e| e.req == KEY_LEN)
        .expect("keygen must draw the 32 key bytes");
    events[start..]
        .iter()
        .filter(|e| e.ret > 0)
        .flat_map(|e| (0..e.ret as usize).map(move |i| pattern_byte(e.base + i)))
        .collect()
}

/// Global draw index at which keygen's own 32-byte fill starts: the `base`
/// of its first (req=32) call. Bytes before that are the process runtime's
/// own startup consumption and vary across environments, so a fail-after-N
/// scenario is calibrated from this instead of a hard-coded N.
fn key_fill_base(events: &[GrEvent]) -> usize {
    events
        .iter()
        .rposition(|e| e.req == KEY_LEN)
        .map(|i| events[i].base)
        .expect("keygen must draw the 32 key bytes")
}

// ---------------------------------------------------------------------------
// permfail shim trace (its lines share the trace file with randtrap's)
// ---------------------------------------------------------------------------

struct ShimEvent {
    kind: String,
    attrs: HashMap<String, String>,
}

impl ShimEvent {
    fn get(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).map(String::as_str)
    }
}

fn shim_events(trace: &Path) -> Vec<ShimEvent> {
    fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(|line| {
            let mut it = line.split_whitespace();
            let kind = it.next().unwrap_or("").to_string();
            let attrs = it
                .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
                .collect();
            ShimEvent { kind, attrs }
        })
        .collect()
}

fn shim_event_seen(events: &[ShimEvent], kind: &str, key: &str, value: &str) -> bool {
    events
        .iter()
        .any(|e| e.kind == kind && e.get(key) == Some(value))
}

/// The bytes the key file's write() calls landed, reconstructed from the
/// shim's TRACE_BYTES hex in call order.
fn landed_bytes(events: &[ShimEvent]) -> Vec<u8> {
    events
        .iter()
        .filter(|e| e.kind == "WRITE")
        .filter_map(|e| e.get("hex"))
        .flat_map(|hex| {
            assert_eq!(hex.len() % 2, 0, "odd hex in trace line");
            (0..hex.len() / 2)
                .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
                .collect::<Vec<u8>>()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// filesystem helpers
// ---------------------------------------------------------------------------

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

fn make_sibling(dir: &Path) -> (PathBuf, Vec<u8>, u32) {
    let p = dir.join("sibling");
    let bytes: Vec<u8> = (0..32).map(|i| b'a' + (i % 26) as u8).collect();
    fs::write(&p, &bytes).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
    (p, bytes, 0o640)
}

fn assert_sentinel_untouched(path: &Path, bytes: &[u8], mode: u32) {
    let meta = fs::metadata(path).unwrap();
    assert_eq!(meta.mode() & 0o7777, mode, "sibling permissions changed");
    assert_eq!(fs::read(path).unwrap(), bytes, "sibling contents changed");
}

/// Key material must not surface on stdout/stderr in any encoding: not as
/// raw bytes, and not as a hex rendering.
fn assert_key_not_leaked(out: &[u8], err: &[u8], secret: &[u8]) {
    assert!(
        !contains_slice(out, secret) && !contains_slice(err, secret),
        "the key must never appear on stdout/stderr as raw bytes"
    );
    let hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();
    let out_text = String::from_utf8_lossy(out);
    let err_text = String::from_utf8_lossy(err);
    assert!(
        !out_text.contains(&hex) && !err_text.contains(&hex),
        "the key must never appear on stdout/stderr in hex form"
    );
}

// Every scenario needs the observer to run successfully (rc 0) and reach a
// guest exit. rc 2 means the observer itself could not run in this
// environment (e.g. ptrace forbidden): the caller skips.
fn observer_ran(obs: &Observation) -> bool {
    if obs.rc == 2 {
        eprintln!(
            "skipping: keylife observer could not run here: {}",
            obs.observer_err.trim()
        );
        false
    } else {
        true
    }
}

// ---------------------------------------------------------------------------
// scenarios, parameterized by the guest binary (debug / release)
// ---------------------------------------------------------------------------

fn success_wipes_full_key_and_saves_it_intact(sup: &Support, bin: &Path, profile: &str) {
    let tmp = Tmp::new(&format!("success-{profile}"));
    let key = tmp.child("key");
    let trace = tmp.child("trace.log");
    fs::File::create(&trace).unwrap();
    fs::set_permissions(&trace, fs::Permissions::from_mode(0o644)).unwrap();

    // Short, always-successful reads from the deterministic source: not a
    // failure, and every delivered byte is recognisable in memory/file.
    let obs = observe(
        &sup.observer,
        bin,
        "success",
        &key,
        &[sup.randtrap.clone()],
        &[("WRAPFILE_TEST_GETRANDOM_PARTIAL", "8")],
        &trace,
    );
    if !observer_ran(&obs) {
        return;
    }
    if !trap_armed(&trace) {
        eprintln!("skipping: randtrap seccomp trap did not arm here");
        return;
    }
    assert_eq!(
        obs.rc, 0,
        "[{profile}] observer must report success, got FAIL: {:?}",
        obs.fail.clone()
    );

    // (1) Non-zero secret really was delivered into the one buffer...
    let fill = obs
        .first("FILL_COMPLETE")
        .expect("FILL_COMPLETE event");
    assert_eq!(fill.usize("held"), Some(KEY_LEN));
    let secret = obs.secret();
    assert_eq!(secret.len(), KEY_LEN);
    assert!(
        secret.iter().all(|&b| b != 0),
        "the held key must consist of non-zero delivered bytes"
    );
    // ...and matches what the random source actually handed keygen.
    let delivered = key_fill_bytes(&getrandom_events(&trace));
    assert_eq!(delivered.len(), KEY_LEN);
    assert_eq!(secret, delivered, "observed buffer bytes must equal delivery");

    // (2) The same live bytes were borrowed for the save (never zeroed
    // before/during), and were still present when close returned.
    assert!(!obs.named("WRITE_BORROWS").is_empty(), "key writes must be seen");
    assert!(obs.first("CLOSE_STILL_HELD").is_some());

    // (3) The whole 32-byte buffer was zeroed in a tight local loop before
    // the first stdout/stderr write -- and before the operation returned.
    let wiped = obs.first("WIPED").expect("WIPED event");
    assert_eq!(wiped.usize("held"), Some(KEY_LEN));
    assert_eq!(wiped.usize("sentinel"), Some(0));
    let steps = wiped.i64("steps").expect("WIPED steps=");
    assert!(
        (1..1_000_000).contains(&steps),
        "wipe must be a small bounded local loop, got {steps} steps"
    );
    let i_wiped = obs.index("WIPED").unwrap();
    let i_output = obs
        .index("OUTPUT_AFTER_WIPE")
        .expect("a post-wipe stdout/stderr write must be seen");
    let i_close = obs.index("CLOSE_STILL_HELD").unwrap();
    assert!(i_close < i_wiped, "wipe must follow the save's close");
    assert!(i_wiped < i_output, "wipe must precede the success report");

    let guest_exit = obs
        .first("GUEST_EXIT")
        .and_then(|e| e.i64("rc"))
        .expect("guest exit event");
    assert_eq!(guest_exit, 0);

    // Public behavior is unchanged: exactly the delivered 32 bytes, mode
    // exactly 0600, stdout only the save-location line, stderr empty.
    let on_disk = fs::read(&key).expect("key file must exist");
    assert_eq!(on_disk.len(), KEY_LEN);
    assert_eq!(on_disk, secret, "the saved key must be the delivered bytes");
    assert_eq!(mode_of(&key), MODE_0600, "key file mode must be exactly 0600");

    let (out, err) = guest_captured(&key);
    assert!(err.is_empty(), "stderr must be empty on success: {err:?}");
    assert_eq!(
        out,
        format!("Key saved to {}\n", key.display()).into_bytes()
    );
}

fn partial_random_failure_wipes_fragment_and_entire_buffer(
    sup: &Support,
    bin: &Path,
    profile: &str,
) {
    let tmp = Tmp::new(&format!("randfail-{profile}"));
    let key = tmp.child("key");
    let (sib, sib_bytes, sib_mode) = make_sibling(&tmp.path);
    let parent_before = mode_of(&tmp.path);
    let trace = tmp.child("trace.log");
    fs::File::create(&trace).unwrap();
    fs::set_permissions(&trace, fs::Permissions::from_mode(0o644)).unwrap();

    // Calibrate where keygen's own 32-byte fill starts in the shim's global
    // draw counter. The process runtime draws some bytes of its own first,
    // and that amount must not be hard-coded; it also depends on the fault
    // semantics (a PARTIAL limit truncates the runtime's own call while a
    // FAIL_AFTER cutoff does not), so the probe uses the *same* FAIL_AFTER
    // mechanism with a quota far beyond anything the process will draw: the
    // guest completes normally and its trace records the fill's base. The
    // real fault run then cuts the source a fixed number of bytes into the
    // fill.
    const TARGET_FRAGMENT: usize = 13;
    let cal_dir = Tmp::new(&format!("randfail-cal-{profile}"));
    let cal_key = cal_dir.child("cal");
    let cal_trace = cal_dir.child("trace.log");
    fs::File::create(&cal_trace).unwrap();
    fs::set_permissions(&cal_trace, fs::Permissions::from_mode(0o644)).unwrap();
    let cal = observe(
        &sup.observer,
        bin,
        "success",
        &cal_key,
        &[sup.randtrap.clone()],
        &[("WRAPFILE_TEST_GETRANDOM_FAIL_AFTER", "100000")],
        &cal_trace,
    );
    if !observer_ran(&cal) {
        return;
    }
    if !trap_armed(&cal_trace) {
        eprintln!("skipping: randtrap seccomp trap did not arm here");
        return;
    }
    assert_eq!(cal.rc, 0, "calibration run must succeed");
    let fill_base = key_fill_base(&getrandom_events(&cal_trace));
    let fail_after = fill_base + TARGET_FRAGMENT;

    let fail_after_env = fail_after.to_string();
    let obs = observe(
        &sup.observer,
        bin,
        "randfail",
        &key,
        &[sup.randtrap.clone()],
        &[("WRAPFILE_TEST_GETRANDOM_FAIL_AFTER", fail_after_env.as_str())],
        &trace,
    );
    if !observer_ran(&obs) {
        return;
    }
    assert!(trap_armed(&trace), "fault run must have the trap armed");
    assert_eq!(
        obs.rc, 0,
        "[{profile}] observer must confirm the wipe, got FAIL: {:?}",
        obs.fail.clone()
    );

    // The scenario really delivered a strict prefix of the key and then
    // failed the draw -- corroborated both by the observer and by the
    // shim's own trace.
    let rf = obs.first("RANDOM_FAILED").expect("RANDOM_FAILED event");
    let held = rf.usize("held").expect("held=");
    let sentinel = rf.usize("sentinel").expect("sentinel=");
    assert!(held > 0 && held < KEY_LEN, "fragment must be 1..31 bytes");
    assert_eq!(
        held, TARGET_FRAGMENT,
        "calibrated cutoff must land {TARGET_FRAGMENT} bytes into the fill"
    );
    assert_eq!(sentinel, KEY_LEN - held);

    // The shim trace independently shows a strict partial delivery followed
    // by an unrecoverable error inside the key fill.
    let trace_events = getrandom_events(&trace);
    let fill_start = trace_events
        .iter()
        .rposition(|e| e.req == KEY_LEN)
        .expect("fault trace must contain the 32-byte fill request");
    let fill_calls = &trace_events[fill_start..];
    let traced_held: usize = fill_calls.iter().filter(|e| e.ret > 0).map(|e| e.ret as usize).sum();
    assert!(
        traced_held > 0 && traced_held < KEY_LEN,
        "trace must show a strict partial fill, got {traced_held} bytes"
    );
    assert_eq!(traced_held, held, "observer and trace agree on fragment size");
    assert!(
        fill_calls.iter().any(|e| e.ret < 0),
        "the fill must have ended with an injected failure"
    );

    // The wipe covers the delivered fragment *and* the never-delivered
    // tail (proven by the planted sentinel being erased), before the error
    // report and before the operation returned.
    let wiped = obs.first("WIPED").expect("WIPED event");
    assert_eq!(wiped.usize("held"), Some(held));
    assert_eq!(wiped.usize("sentinel"), Some(sentinel));
    let i_wiped = obs.index("WIPED").unwrap();
    let i_output = obs
        .index("OUTPUT_AFTER_WIPE")
        .expect("the error report must be observed after the wipe");
    assert!(i_wiped < i_output, "wipe must precede the failure report");
    assert_eq!(
        obs.first("GUEST_EXIT").and_then(|e| e.i64("rc")),
        Some(1)
    );

    // Public failure behavior: no key file, no zero-padded stand-in,
    // exit 1, empty stdout, stderr explains the random-source failure and
    // names nothing secret; existing files and the parent are untouched.
    assert!(!key.exists(), "no key file may be created on a partial draw");
    let (out, err) = guest_captured(&key);
    assert!(out.is_empty(), "stdout must be empty on failure");
    let err_text = String::from_utf8_lossy(&err);
    assert!(
        err_text.contains("cryptographic random source unavailable"),
        "stderr must explain the random-source failure: {err_text}"
    );
    // The delivered fragment must not appear in the report, raw or hex.
    let events = getrandom_events(&trace);
    let drawn: Vec<u8> = events
        .iter()
        .rposition(|e| e.req == KEY_LEN)
        .map(|s| {
            events[s..]
                .iter()
                .filter(|e| e.ret > 0)
                .flat_map(|e| (0..e.ret as usize).map(move |i| pattern_byte(e.base + i)))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(drawn.len(), held);
    assert!(
        !contains_slice(&out, &drawn) && !contains_slice(&err, &drawn),
        "the delivered fragment must never appear raw on stdout/stderr"
    );
    let hex: String = drawn.iter().map(|b| format!("{b:02x}")).collect();
    assert!(!err_text.contains(&hex), "fragment must not appear hex-encoded");

    assert_sentinel_untouched(&sib, &sib_bytes, sib_mode);
    assert_eq!(mode_of(&tmp.path), parent_before, "parent mode must survive");
}

fn write_failure_after_full_key_still_wipes_the_full_key(
    sup: &Support,
    bin: &Path,
    profile: &str,
) {
    let tmp = Tmp::new(&format!("writefail-{profile}"));
    let key = tmp.child("key");
    let (sib, sib_bytes, sib_mode) = make_sibling(&tmp.path);
    let parent_before = mode_of(&tmp.path);
    let trace = tmp.child("trace.log");
    fs::File::create(&trace).unwrap();
    fs::set_permissions(&trace, fs::Permissions::from_mode(0o644)).unwrap();

    // Deterministic full key, then the permfail shim accepts only 4 bytes
    // per write and fails with EIO once 10 bytes have landed: part of the
    // key is on disk, the save aborts, the file is unlinked.
    let obs = observe(
        &sup.observer,
        bin,
        "writefail",
        &key,
        &[sup.randtrap.clone(), sup.permfail.clone()],
        &[
            ("WRAPFILE_TEST_GETRANDOM_PARTIAL", "8"),
            ("WRAPFILE_TEST_PARTIAL_WRITE", "4"),
            ("WRAPFILE_TEST_FAIL_WRITE_AFTER", "10"),
            ("WRAPFILE_TEST_TRACE_BYTES", "1"),
        ],
        &trace,
    );
    if !observer_ran(&obs) {
        return;
    }
    if !trap_armed(&trace) {
        eprintln!("skipping: randtrap seccomp trap did not arm here");
        return;
    }
    assert_eq!(
        obs.rc, 0,
        "[{profile}] observer must confirm the wipe, got FAIL: {:?}",
        obs.fail.clone()
    );

    // The full, non-zero key was delivered and still held live across the
    // key writes, the abort's unlink, and the abort's close.
    let secret = obs.secret();
    assert_eq!(secret.len(), KEY_LEN);
    assert!(secret.iter().all(|&b| b != 0));
    assert!(!obs.named("WRITE_BORROWS").is_empty());
    assert!(obs.first("WRITE_FAILED").is_some(), "abort unlink must see the full key");
    assert!(obs.first("ABORT_UNLINK").is_some());
    assert!(obs.first("ABORT_CLOSE_STILL_HELD").is_some());

    // The whole full key was wiped before the error report / return.
    let i_wiped = obs.index("WIPED").expect("WIPED event");
    let wiped = obs.first("WIPED").unwrap();
    assert_eq!(wiped.usize("held"), Some(KEY_LEN));
    assert_eq!(wiped.usize("sentinel"), Some(0));
    assert!(
        obs.index("ABORT_CLOSE_STILL_HELD").unwrap() < i_wiped,
        "wipe must follow the abort close"
    );
    assert!(
        i_wiped < obs.index("OUTPUT_AFTER_WIPE").unwrap(),
        "wipe must precede the failure report"
    );
    assert_eq!(
        obs.first("GUEST_EXIT").and_then(|e| e.i64("rc")),
        Some(1)
    );

    // Public failure behavior: no leftover file, exit 1, empty stdout,
    // stderr names the write failure and the target path.
    assert!(!key.exists(), "the aborted file must be cleaned up");
    let (out, err) = guest_captured(&key);
    assert!(out.is_empty(), "no success message on failure");
    let err_text = String::from_utf8_lossy(&err);
    assert!(
        err_text.contains("could not write key file"),
        "stderr must explain the write failure: {err_text}"
    );
    assert!(
        err_text.contains(&key.display().to_string()),
        "stderr must name the target: {err_text}"
    );
    // The full key must not leak, raw or hex.
    assert!(
        !err.windows(KEY_LEN).any(|w| w == secret.as_slice()),
        "full key must not appear raw on stderr"
    );
    let hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();
    assert!(!err_text.contains(&hex), "full key must not appear hex-encoded");

    assert_sentinel_untouched(&sib, &sib_bytes, sib_mode);
    assert_eq!(mode_of(&tmp.path), parent_before, "parent mode must survive");
}

/// Shared driver for the two permission-failure scenarios: the filesystem
/// refuses to set (fchmod) or confirm (fstat) the required 0600 after the
/// full key has been drawn but before any key byte is written. The observer
/// must see the complete non-zero key still held in the very buffer the
/// random source filled, through the abort cleanup (unlink of this run's
/// file and close of its descriptor), and then wiped before the first
/// failure-report byte -- i.e. before the keygen operation returns.
fn perm_failure_after_full_key_still_wipes_the_full_key(
    sup: &Support,
    bin: &Path,
    profile: &str,
    scenario: &str,
    fault_env: &str,
    stage_stderr: &str,
    other_stage_stderr: &str,
) {
    let tmp = Tmp::new(&format!("{scenario}-{profile}"));
    let key = tmp.child("key");
    let (sib, sib_bytes, sib_mode) = make_sibling(&tmp.path);
    let parent_before = mode_of(&tmp.path);
    let trace = tmp.child("trace.log");
    fs::File::create(&trace).unwrap();
    fs::set_permissions(&trace, fs::Permissions::from_mode(0o644)).unwrap();

    // The key draw succeeds completely (the failure hits only afterwards);
    // the permfail shim then refuses the permission stage in user space,
    // after the target was created but before any key byte could be written.
    let obs = observe(
        &sup.observer,
        bin,
        scenario,
        &key,
        &[sup.randtrap.clone(), sup.permfail.clone()],
        &[
            ("WRAPFILE_TEST_GETRANDOM_PARTIAL", "8"),
            (fault_env, "1"),
            ("WRAPFILE_TEST_TRACE_BYTES", "1"),
        ],
        &trace,
    );
    if !observer_ran(&obs) {
        return;
    }
    if !trap_armed(&trace) {
        eprintln!("skipping: randtrap seccomp trap did not arm here");
        return;
    }
    assert_eq!(
        obs.rc, 0,
        "[{profile}] observer must confirm the wipe, got FAIL: {:?}",
        obs.fail.clone()
    );

    // The scenario really reached the intended permission-failure stage:
    // the shim trace shows the injected refusal (and, for the confirmation
    // stage, that the fchmod itself succeeded first)...
    let shim = shim_events(&trace);
    match scenario {
        "chmodfail" => assert!(
            shim_event_seen(&shim, "FCHMOD", "ret", "-1"),
            "the fchmod failure must have been injected"
        ),
        "statfail" => {
            assert!(
                shim_event_seen(&shim, "FCHMOD", "ret", "0"),
                "the fchmod must have succeeded before the confirmation"
            );
            assert!(
                shim_event_seen(&shim, "FSTAT", "ret", "-1"),
                "the fstat failure must have been injected"
            );
        }
        other => panic!("unknown permission-failure scenario {other}"),
    }
    // ...and no key byte was ever offered to the target file.
    assert!(
        shim.iter().all(|e| e.kind != "WRITE" && e.kind != "FIRST_WRITE"),
        "no key byte may be written when the permissions cannot be guaranteed"
    );

    // The full, non-zero key really was delivered into the buffer the
    // observer watches -- the buffer the save code itself holds.
    let secret = obs.secret();
    assert_eq!(secret.len(), KEY_LEN);
    assert!(
        secret.iter().all(|&b| b != 0),
        "the held key must consist of non-zero delivered bytes"
    );
    let delivered = key_fill_bytes(&getrandom_events(&trace));
    assert_eq!(delivered.len(), KEY_LEN);
    assert_eq!(secret, delivered, "observed buffer bytes must equal delivery");

    // That same buffer still held the full key when the abort cleanup
    // unlinked this run's file and closed its descriptor...
    assert!(
        obs.first("ABORT_UNLINK").is_some(),
        "the cleanup unlink must still see the full key"
    );
    assert!(
        obs.first("ABORT_CLOSE_STILL_HELD").is_some(),
        "the cleanup close must still see the full key"
    );

    // ...and was then wiped in a tight local loop, after the cleanup and
    // before the first failure-report byte.
    let wiped = obs.first("WIPED").expect("WIPED event");
    assert_eq!(wiped.usize("held"), Some(KEY_LEN));
    assert_eq!(wiped.usize("sentinel"), Some(0));
    let steps = wiped.i64("steps").expect("WIPED steps=");
    assert!(
        (1..1_000_000).contains(&steps),
        "wipe must be a small bounded local loop, got {steps} steps"
    );
    let i_unlink = obs.index("ABORT_UNLINK").unwrap();
    let i_close = obs.index("ABORT_CLOSE_STILL_HELD").unwrap();
    let i_wiped = obs.index("WIPED").unwrap();
    let i_output = obs
        .index("OUTPUT_AFTER_WIPE")
        .expect("the failure report must be observed after the wipe");
    assert!(i_unlink < i_close, "the cleanup close must follow the unlink");
    assert!(i_close < i_wiped, "wipe must follow the abort cleanup");
    assert!(i_wiped < i_output, "wipe must precede the failure report");
    assert_eq!(
        obs.first("GUEST_EXIT").and_then(|e| e.i64("rc")),
        Some(1),
        "a permission failure must fail the save"
    );

    // Public failure behavior: exit 1, empty stdout, stderr names the
    // target and identifies *this* permission stage (never the other one);
    // this run's empty file is gone; the key leaks nowhere.
    assert!(!key.exists(), "the file from the failed save must be removed");
    let (out, err) = guest_captured(&key);
    assert!(out.is_empty(), "no success message on failure");
    let err_text = String::from_utf8_lossy(&err);
    assert!(
        err_text.contains(stage_stderr),
        "stderr must name the permission stage that failed: {err_text}"
    );
    assert!(
        !err_text.contains(other_stage_stderr),
        "stderr must not blame the other permission stage: {err_text}"
    );
    assert!(
        err_text.contains(&key.display().to_string()),
        "stderr must name the target: {err_text}"
    );
    assert_key_not_leaked(&out, &err, &secret);

    assert_sentinel_untouched(&sib, &sib_bytes, sib_mode);
    assert_eq!(mode_of(&tmp.path), parent_before, "parent mode must survive");
}

fn chmod_failure_after_full_key_still_wipes_the_full_key(
    sup: &Support,
    bin: &Path,
    profile: &str,
) {
    perm_failure_after_full_key_still_wipes_the_full_key(
        sup,
        bin,
        profile,
        "chmodfail",
        "WRAPFILE_TEST_FAIL_FCHMOD",
        "cannot set key file permissions to 0600",
        "cannot verify key file permissions",
    );
}

fn stat_failure_after_full_key_still_wipes_the_full_key(
    sup: &Support,
    bin: &Path,
    profile: &str,
) {
    perm_failure_after_full_key_still_wipes_the_full_key(
        sup,
        bin,
        profile,
        "statfail",
        "WRAPFILE_TEST_FAIL_FSTAT",
        "cannot verify key file permissions are 0600",
        "cannot set key file permissions",
    );
}

/// Shared observer-side assertions for the two close-failure scenarios:
/// the full non-zero key was delivered and borrowed through the save, the
/// injected close fault really fired after a complete synced write, and the
/// whole buffer was wiped in a tight local loop after the failed close and
/// before the first failure-report byte.
fn assert_closefail_wipe(obs: &Observation, trace: &Path, profile: &str) -> Vec<u8> {
    assert_eq!(
        obs.rc, 0,
        "[{profile}] observer must confirm the wipe, got FAIL: {:?}",
        obs.fail.clone()
    );

    // The full, non-zero key really was delivered and is what the save
    // borrowed (never zeroed before or during the writes).
    let secret = obs.secret();
    assert_eq!(secret.len(), KEY_LEN);
    assert!(secret.iter().all(|&b| b != 0));
    let delivered = key_fill_bytes(&getrandom_events(trace));
    assert_eq!(delivered.len(), KEY_LEN);
    assert_eq!(secret, delivered, "observed buffer bytes must equal delivery");
    assert!(!obs.named("WRITE_BORROWS").is_empty(), "key writes must be seen");

    // The fault really was injected: all 32 bytes landed and were synced,
    // then close reported EIO (the descriptor was really released).
    let shim = shim_events(trace);
    assert!(
        shim_event_seen(&shim, "FSYNC", "ret", "0"),
        "the key must have been fully synced before the close"
    );
    assert!(
        shim_event_seen(&shim, "CLOSE", "errno", "EIO"),
        "the unrecoverable close error must have been injected"
    );
    assert_eq!(
        landed_bytes(&shim),
        secret,
        "the bytes that landed on disk are exactly the delivered key"
    );

    // The key was still held when the close returned; the whole buffer was
    // then wiped before the failure report and before the operation
    // returned.
    assert!(
        obs.first("CLOSE_STILL_HELD").is_some(),
        "the full key must still be held when the failed close returns"
    );
    let wiped = obs.first("WIPED").expect("WIPED event");
    assert_eq!(wiped.usize("held"), Some(KEY_LEN));
    assert_eq!(wiped.usize("sentinel"), Some(0));
    let steps = wiped.i64("steps").expect("WIPED steps=");
    assert!(
        (1..1_000_000).contains(&steps),
        "wipe must be a small bounded local loop, got {steps} steps"
    );
    let i_close = obs.index("CLOSE_STILL_HELD").unwrap();
    let i_wiped = obs.index("WIPED").unwrap();
    let i_output = obs
        .index("OUTPUT_AFTER_WIPE")
        .expect("the failure report must be observed after the wipe");
    assert!(i_close < i_wiped, "wipe must follow the failed close");
    assert!(i_wiped < i_output, "wipe must precede the failure report");
    assert_eq!(
        obs.first("GUEST_EXIT").and_then(|e| e.i64("rc")),
        Some(1),
        "an unrecoverable close error must fail the save"
    );

    secret
}

fn close_failure_after_full_save_still_wipes_the_full_key(
    sup: &Support,
    bin: &Path,
    profile: &str,
) {
    let tmp = Tmp::new(&format!("closefail-{profile}"));
    let key = tmp.child("key");
    let (sib, sib_bytes, sib_mode) = make_sibling(&tmp.path);
    let parent_before = mode_of(&tmp.path);
    let trace = tmp.child("trace.log");
    fs::File::create(&trace).unwrap();
    fs::set_permissions(&trace, fs::Permissions::from_mode(0o644)).unwrap();

    // Deterministic full key; all 32 bytes are written and fsynced, then
    // the permfail shim really releases the descriptor but reports EIO for
    // the close. The file on disk briefly looks complete -- the save must
    // still fail, the file must be removed, and the in-memory key must be
    // wiped before the failure is reported.
    let obs = observe(
        &sup.observer,
        bin,
        "closefail",
        &key,
        &[sup.randtrap.clone(), sup.permfail.clone()],
        &[
            ("WRAPFILE_TEST_GETRANDOM_PARTIAL", "8"),
            ("WRAPFILE_TEST_FAIL_CLOSE", "1"),
            ("WRAPFILE_TEST_TRACE_BYTES", "1"),
        ],
        &trace,
    );
    if !observer_ran(&obs) {
        return;
    }
    if !trap_armed(&trace) {
        eprintln!("skipping: randtrap seccomp trap did not arm here");
        return;
    }
    let secret = assert_closefail_wipe(&obs, &trace, profile);

    // abort_save's cleanup ran while the full key was still held; the wipe
    // came only afterwards.
    assert!(
        obs.first("ABORT_UNLINK").is_some(),
        "the cleanup unlink must still see the full key"
    );
    assert!(
        obs.index("ABORT_UNLINK").unwrap() < obs.index("WIPED").unwrap(),
        "wipe must follow the abort cleanup"
    );

    // Public failure behavior: exit 1, empty stdout, stderr says the
    // failure happened while closing the key file and names the target;
    // this run's file is gone; the key leaks nowhere.
    assert!(!key.exists(), "the file from the failed close must be removed");
    let (out, err) = guest_captured(&key);
    assert!(out.is_empty(), "no success message on failure");
    let err_text = String::from_utf8_lossy(&err);
    assert!(
        err_text.contains("closing the key file"),
        "stderr must say the failure happened at close time: {err_text}"
    );
    assert!(
        err_text.contains(&key.display().to_string()),
        "stderr must name the target: {err_text}"
    );
    assert_key_not_leaked(&out, &err, &secret);

    assert_sentinel_untouched(&sib, &sib_bytes, sib_mode);
    assert_eq!(mode_of(&tmp.path), parent_before, "parent mode must survive");
}

fn close_failure_with_unlink_refused_still_wipes_the_full_key(
    sup: &Support,
    bin: &Path,
    profile: &str,
) {
    let tmp = Tmp::new(&format!("closefail-unlink-refused-{profile}"));
    let key = tmp.child("key");
    let (sib, sib_bytes, sib_mode) = make_sibling(&tmp.path);
    let parent_before = mode_of(&tmp.path);
    let trace = tmp.child("trace.log");
    fs::File::create(&trace).unwrap();
    fs::set_permissions(&trace, fs::Permissions::from_mode(0o644)).unwrap();

    // Same unrecoverable close error, but the system also refuses to remove
    // this run's file (the shim's unlink fails with EACCES without any
    // syscall). The same in-memory wipe contract must hold, and the report
    // must keep the close failure while warning about the residue.
    let obs = observe(
        &sup.observer,
        bin,
        "closefail",
        &key,
        &[sup.randtrap.clone(), sup.permfail.clone()],
        &[
            ("WRAPFILE_TEST_GETRANDOM_PARTIAL", "8"),
            ("WRAPFILE_TEST_FAIL_CLOSE", "1"),
            ("WRAPFILE_TEST_FAIL_UNLINK", "1"),
            ("WRAPFILE_TEST_TRACE_BYTES", "1"),
        ],
        &trace,
    );
    if !observer_ran(&obs) {
        return;
    }
    if !trap_armed(&trace) {
        eprintln!("skipping: randtrap seccomp trap did not arm here");
        return;
    }
    let secret = assert_closefail_wipe(&obs, &trace, profile);

    // The refused cleanup really was injected, in user space: no unlinkat
    // syscall ever reached the kernel, so the observer saw no ABORT_UNLINK.
    let shim = shim_events(&trace);
    assert!(
        shim_event_seen(&shim, "UNLINK", "errno", "EACCES"),
        "the refused cleanup must have been injected"
    );
    assert!(
        obs.first("ABORT_UNLINK").is_none(),
        "a refused unlink performs no syscall the observer could see"
    );

    // Public failure behavior: exit 1, empty stdout; stderr keeps the close
    // failure, reports the unfinished cleanup, and warns that this run's
    // file may remain -- never that it is gone, never a success.
    let (out, err) = guest_captured(&key);
    assert!(out.is_empty(), "no success message on failure");
    let err_text = String::from_utf8_lossy(&err);
    assert!(
        err_text.contains("closing the key file"),
        "stderr must keep the close failure reason: {err_text}"
    );
    assert!(
        err_text.contains(&key.display().to_string()),
        "stderr must name the target: {err_text}"
    );
    assert!(
        err_text.contains("cleanup"),
        "stderr must report the failed cleanup: {err_text}"
    );
    assert!(
        err_text.contains("may still be present"),
        "stderr must warn that this run's file may remain: {err_text}"
    );
    assert!(
        !err_text.contains("Key saved to"),
        "the residue must not be reported as a saved key: {err_text}"
    );
    assert_key_not_leaked(&out, &err, &secret);

    // The residue really is this run's file: 32 bytes, mode 0600 -- it
    // looks complete, yet it is not a saved key (the warning above is what
    // keeps it from being mistaken for one). Nothing was stashed under
    // another name.
    let residue = fs::read(&key).expect("with unlink refused, this run's file remains");
    assert_eq!(residue.len(), KEY_LEN);
    assert_eq!(
        residue, secret,
        "the residue is exactly the key this failed run wrote"
    );
    assert_eq!(mode_of(&key), MODE_0600, "residue keeps mode 0600");

    assert_sentinel_untouched(&sib, &sib_bytes, sib_mode);
    assert_eq!(
        mode_of(&tmp.path),
        parent_before,
        "parent directory permissions must never be loosened to force cleanup"
    );

    // Remove the residue manually (the user is told to); the scratch dir
    // must not keep a key around.
    fs::remove_file(&key).unwrap();
}

fn close_eintr_boundary_saves_and_wipes_before_the_report(
    sup: &Support,
    bin: &Path,
    profile: &str,
) {
    let tmp = Tmp::new(&format!("close-eintr-{profile}"));
    let key = tmp.child("key");
    let trace = tmp.child("trace.log");
    fs::File::create(&trace).unwrap();
    fs::set_permissions(&trace, fs::Permissions::from_mode(0o644)).unwrap();

    // close() releases the descriptor but reports EINTR: an interruption,
    // not a failure. The save succeeds and the temporary key is still wiped
    // before the success report. From the observer's side the close syscall
    // itself succeeds, so this runs under the success scenario.
    let obs = observe(
        &sup.observer,
        bin,
        "success",
        &key,
        &[sup.randtrap.clone(), sup.permfail.clone()],
        &[
            ("WRAPFILE_TEST_GETRANDOM_PARTIAL", "8"),
            ("WRAPFILE_TEST_CLOSE_EINTR", "1"),
            ("WRAPFILE_TEST_TRACE_BYTES", "1"),
        ],
        &trace,
    );
    if !observer_ran(&obs) {
        return;
    }
    if !trap_armed(&trace) {
        eprintln!("skipping: randtrap seccomp trap did not arm here");
        return;
    }
    assert_eq!(
        obs.rc, 0,
        "[{profile}] observer must report success, got FAIL: {:?}",
        obs.fail.clone()
    );

    // The interruption really was injected: the descriptor was released and
    // close only *reported* EINTR.
    let shim = shim_events(&trace);
    assert!(
        shim_event_seen(&shim, "CLOSE", "errno", "EINTR"),
        "the EINTR-on-close must have been injected"
    );

    // The full non-zero key was delivered, borrowed through the save, still
    // held when close returned, and wiped before the success report.
    let secret = obs.secret();
    assert_eq!(secret.len(), KEY_LEN);
    assert!(secret.iter().all(|&b| b != 0));
    assert!(!obs.named("WRITE_BORROWS").is_empty(), "key writes must be seen");
    assert!(obs.first("CLOSE_STILL_HELD").is_some());
    let wiped = obs.first("WIPED").expect("WIPED event");
    assert_eq!(wiped.usize("held"), Some(KEY_LEN));
    assert_eq!(wiped.usize("sentinel"), Some(0));
    let i_close = obs.index("CLOSE_STILL_HELD").unwrap();
    let i_wiped = obs.index("WIPED").unwrap();
    let i_output = obs
        .index("OUTPUT_AFTER_WIPE")
        .expect("a post-wipe stdout write must be seen");
    assert!(i_close < i_wiped, "wipe must follow the close");
    assert!(i_wiped < i_output, "wipe must precede the success report");
    assert_eq!(
        obs.first("GUEST_EXIT").and_then(|e| e.i64("rc")),
        Some(0),
        "a close that only reports EINTR is not a failure"
    );

    // The saved file is exactly the delivered key with mode 0600, and the
    // output is the usual success contract.
    let on_disk = fs::read(&key).expect("key file must exist on success");
    assert_eq!(on_disk.len(), KEY_LEN);
    assert_eq!(on_disk, secret, "the saved key must be the delivered bytes");
    assert_eq!(mode_of(&key), MODE_0600, "key file mode must be exactly 0600");
    let (out, err) = guest_captured(&key);
    assert!(err.is_empty(), "stderr must be empty on success: {err:?}");
    assert_eq!(
        out,
        format!("Key saved to {}\n", key.display()).into_bytes()
    );
}


// ---------------------------------------------------------------------------
// tests: each scenario under both build profiles
// ---------------------------------------------------------------------------

#[test]
fn success_zeroes_the_held_key_before_returning_under_both_profiles() {
    let Some(sup) = support() else {
        eprintln!("skipping: keylife/randtrap/permfail support unavailable");
        return;
    };
    success_wipes_full_key_and_saves_it_intact(&sup, &debug_bin(), "debug");
    if let Some(rel) = release_bin() {
        success_wipes_full_key_and_saves_it_intact(&sup, &rel, "release");
    }
}

#[test]
fn partial_random_failure_zeroes_fragment_and_whole_buffer_under_both_profiles() {
    let Some(sup) = support() else {
        eprintln!("skipping: keylife/randtrap/permfail support unavailable");
        return;
    };
    partial_random_failure_wipes_fragment_and_entire_buffer(&sup, &debug_bin(), "debug");
    if let Some(rel) = release_bin() {
        partial_random_failure_wipes_fragment_and_entire_buffer(&sup, &rel, "release");
    }
}

#[test]
fn write_failure_after_full_key_zeroes_before_returning_under_both_profiles() {
    let Some(sup) = support() else {
        eprintln!("skipping: keylife/randtrap/permfail support unavailable");
        return;
    };
    write_failure_after_full_key_still_wipes_the_full_key(&sup, &debug_bin(), "debug");
    if let Some(rel) = release_bin() {
        write_failure_after_full_key_still_wipes_the_full_key(&sup, &rel, "release");
    }
}

#[test]
fn chmod_failure_after_full_key_zeroes_before_returning_under_both_profiles() {
    let Some(sup) = support() else {
        eprintln!("skipping: keylife/randtrap/permfail support unavailable");
        return;
    };
    chmod_failure_after_full_key_still_wipes_the_full_key(&sup, &debug_bin(), "debug");
    if let Some(rel) = release_bin() {
        chmod_failure_after_full_key_still_wipes_the_full_key(&sup, &rel, "release");
    }
}

#[test]
fn stat_failure_after_full_key_zeroes_before_returning_under_both_profiles() {
    let Some(sup) = support() else {
        eprintln!("skipping: keylife/randtrap/permfail support unavailable");
        return;
    };
    stat_failure_after_full_key_still_wipes_the_full_key(&sup, &debug_bin(), "debug");
    if let Some(rel) = release_bin() {
        stat_failure_after_full_key_still_wipes_the_full_key(&sup, &rel, "release");
    }
}

#[test]
fn close_failure_after_full_save_zeroes_before_reporting_under_both_profiles() {
    let Some(sup) = support() else {
        eprintln!("skipping: keylife/randtrap/permfail support unavailable");
        return;
    };
    close_failure_after_full_save_still_wipes_the_full_key(&sup, &debug_bin(), "debug");
    if let Some(rel) = release_bin() {
        close_failure_after_full_save_still_wipes_the_full_key(&sup, &rel, "release");
    }
}

#[test]
fn close_failure_with_unlink_refused_zeroes_before_reporting_under_both_profiles() {
    let Some(sup) = support() else {
        eprintln!("skipping: keylife/randtrap/permfail support unavailable");
        return;
    };
    close_failure_with_unlink_refused_still_wipes_the_full_key(&sup, &debug_bin(), "debug");
    if let Some(rel) = release_bin() {
        close_failure_with_unlink_refused_still_wipes_the_full_key(&sup, &rel, "release");
    }
}

#[test]
fn close_eintr_boundary_zeroes_before_success_report_under_both_profiles() {
    let Some(sup) = support() else {
        eprintln!("skipping: keylife/randtrap/permfail support unavailable");
        return;
    };
    close_eintr_boundary_saves_and_wipes_before_the_report(&sup, &debug_bin(), "debug");
    if let Some(rel) = release_bin() {
        close_eintr_boundary_saves_and_wipes_before_the_report(&sup, &rel, "release");
    }
}
