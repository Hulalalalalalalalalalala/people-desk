//! Regression coverage for keygen's Unix path handling (see README
//! "拒绝覆盖与路径要求" and "参数与状态码").
//!
//! The rule protected here: keygen operates on the *raw* path the user
//! handed to the command. Rendering that path for a message (a lossy
//! conversion that may replace undecodable bytes) must never decide which
//! file is created or refused -- the display string is not a filesystem
//! name.
//!
//! What is protected here:
//!
//! * A legal file name containing non-UTF-8 bytes generates a key normally
//!   when the parent exists and is writable and the target does not exist:
//!   exit 0, exactly 32 raw key bytes at the *raw* name, mode 0600, the
//!   usual save-location line on stdout (undecodable bytes may appear
//!   replaced there), empty stderr. No extra file whose name contains the
//!   U+FFFD replacement character is created as a side effect of display
//!   conversion, and the command does not crash.
//! * Two names in one directory whose raw bytes differ but whose lossy
//!   display is identical are distinct targets: existence is judged only
//!   by the raw path the user passed. The look-alike sibling is never
//!   treated as the target, overwritten, or re-permissioned. If the raw
//!   target itself exists, the command exits 1, explains on stderr, prints
//!   no success message, preserves the existing file's contents and
//!   permissions, and does not fall back to generating a key under the
//!   displayed name.
//! * `--` immediately after `keygen` ends option parsing: the single
//!   following argument is a file name even if it starts with '-' or also
//!   contains non-UTF-8 bytes, and the success contract above still holds.
//!   Without `--`, a '-'-led argument stays an unknown-option error: exit
//!   2, usage on stderr, no new file. `--` does not change the
//!   exactly-one-path requirement.
//!
//! Existing behaviour (--version, plain-path generation, refusal to
//! overwrite) is covered by keygen_permissions.rs and
//! keygen_write_safety.rs and is unchanged.
#![cfg(unix)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const KEY_LEN: usize = 32;
const MODE_0600: u32 = 0o600;
/// UTF-8 encoding of U+FFFD, which lossy display substitutes for each
/// undecodable byte. A file whose name contains these bytes would be an
/// artifact of display conversion, not of the user's request.
const REPLACEMENT: &[u8] = "\u{FFFD}".as_bytes();

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_wrapfile"))
}

fn os(raw: &[u8]) -> OsString {
    OsString::from_vec(raw.to_vec())
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
            "wrapfile-keygen-path-tests-{}-{}-{}",
            std::process::id(),
            label,
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Tmp { path }
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

/// Run `wrapfile <args>` with `dir` as the working directory, so targets
/// can be passed as bare (relative) file names -- including names that
/// start with '-' or contain non-UTF-8 bytes.
fn run_in(dir: &Path, args: &[&OsStr]) -> Run {
    let o = Command::new(bin())
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    Run {
        rc: o.status.code().unwrap_or(-1),
        out: o.stdout,
        err: o.stderr,
    }
}

fn run_keygen(dir: &Path, rest: &[&OsStr]) -> Run {
    let mut args: Vec<&OsStr> = vec![OsStr::new("keygen")];
    args.extend_from_slice(rest);
    run_in(dir, &args)
}

// ---------------------------------------------------------------------------
// filesystem assertions
// ---------------------------------------------------------------------------

/// Raw bytes of every entry name in `dir`, sorted for comparison.
fn entry_names(dir: &Path) -> Vec<Vec<u8>> {
    let mut names: Vec<Vec<u8>> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_vec())
        .collect();
    names.sort();
    names
}

fn assert_no_display_artifact(dir: &Path) {
    for name in entry_names(dir) {
        assert!(
            !name.windows(REPLACEMENT.len()).any(|w| w == REPLACEMENT),
            "display conversion must not create a file named with U+FFFD: {name:?}"
        );
    }
}

/// The success contract for a target passed as the bare name `raw_name`
/// inside `dir`: exit 0, empty stderr, only the save-location line on
/// stdout (lossy display allowed there), and exactly 32 raw key bytes in a
/// 0600 file at the *raw* name.
fn assert_key_generated(dir: &Path, raw_name: &[u8], run: &Run) -> Vec<u8> {
    assert_eq!(run.rc, 0, "stderr={}", String::from_utf8_lossy(&run.err));
    assert!(
        run.err.is_empty(),
        "stderr must be empty on success, got {}",
        String::from_utf8_lossy(&run.err)
    );

    let raw = os(raw_name);
    let expected_out = format!("Key saved to {}\n", Path::new(&raw).display()).into_bytes();
    assert_eq!(
        run.out, expected_out,
        "stdout must be exactly the save-location message for the raw path"
    );

    let key = dir.join(&raw);
    let meta = fs::metadata(&key).expect("key file must exist at the raw path");
    assert_eq!(meta.len() as usize, KEY_LEN, "key must be exactly 32 bytes");
    assert_eq!(
        meta.mode() & 0o7777,
        MODE_0600,
        "key file mode must be exactly 0600"
    );

    let bytes = fs::read(&key).unwrap();
    assert_eq!(bytes.len(), KEY_LEN);
    assert!(bytes.iter().any(|&b| b != 0), "key must not be all zeroes");
    bytes
}

// A pre-existing object that must survive keygen byte-for-byte.
struct Sentinel {
    path: PathBuf,
    bytes: Vec<u8>,
    mode: u32,
    inode: u64,
}

impl Sentinel {
    fn file(dir: &Path, raw_name: &[u8], bytes: &[u8], mode: u32) -> Sentinel {
        let path = dir.join(os(raw_name));
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
        assert_eq!(fs::read(&self.path).unwrap(), self.bytes, "sentinel contents changed");
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[test]
fn non_utf8_name_generates_key_at_the_raw_path() {
    let tmp = Tmp::new("non-utf8");
    // Legal on-disk bytes, not decodable as UTF-8.
    let raw_name: &[u8] = b"key-\xff raw \xfe.bin";

    let run = run_keygen(&tmp.path, &[&os(raw_name)]);
    assert_key_generated(&tmp.path, raw_name, &run);

    // The only thing created is the key at the raw name -- in particular no
    // second file named after the lossy display string (with U+FFFD).
    assert_eq!(
        entry_names(&tmp.path),
        vec![raw_name.to_vec()],
        "exactly one file, at the raw name, must be created"
    );
    assert_no_display_artifact(&tmp.path);
}

#[test]
fn confusable_display_names_are_distinct_targets() {
    let tmp = Tmp::new("confusable");
    // Different raw bytes, identical lossy display ("key-�"): the premise
    // of the confusion this test guards against.
    let existing: &[u8] = b"key-\xff";
    let target: &[u8] = b"key-\xfe";
    assert_eq!(
        Path::new(&os(existing)).display().to_string(),
        Path::new(&os(target)).display().to_string(),
        "test premise: both names must render identically"
    );

    let lookalike = Sentinel::file(&tmp.path, existing, b"not-the-target", 0o640);

    // The target does not exist -- only its display-twin does. Existence is
    // judged by the raw path, so generation must succeed at the raw name.
    let run = run_keygen(&tmp.path, &[&os(target)]);
    assert_key_generated(&tmp.path, target, &run);

    // The look-alike sibling was not treated as the target, overwritten,
    // or re-permissioned.
    lookalike.assert_untouched();
    let mut want: Vec<Vec<u8>> = vec![existing.to_vec(), target.to_vec()];
    want.sort();
    assert_eq!(entry_names(&tmp.path), want, "only the raw target was added");
    assert_no_display_artifact(&tmp.path);
}

#[test]
fn existing_raw_target_fails_despite_confusable_sibling() {
    let tmp = Tmp::new("confusable-exists");
    let existing: &[u8] = b"key-\xff";
    let lookalike: &[u8] = b"key-\xfe";
    assert_eq!(
        Path::new(&os(existing)).display().to_string(),
        Path::new(&os(lookalike)).display().to_string(),
        "test premise: both names must render identically"
    );

    // Both the raw target and its display-twin exist on disk.
    let target_file = Sentinel::file(&tmp.path, existing, b"pre-existing-key-material", 0o640);
    let other = Sentinel::file(&tmp.path, lookalike, b"unrelated", 0o644);

    let run = run_keygen(&tmp.path, &[&os(existing)]);
    assert_eq!(run.rc, 1, "an existing raw target must be refused");
    assert!(
        run.out.is_empty(),
        "no success message on failure, got {}",
        String::from_utf8_lossy(&run.out)
    );
    assert!(!run.err.is_empty(), "stderr must explain the failure");
    let stderr = String::from_utf8_lossy(&run.err);
    assert!(
        stderr.contains(&Path::new(&os(existing)).display().to_string()),
        "stderr should name the refused target, got: {stderr}"
    );

    // The existing file keeps its contents and permissions, the sibling is
    // untouched, and no key is generated under the displayed name instead.
    target_file.assert_untouched();
    other.assert_untouched();
    let mut want: Vec<Vec<u8>> = vec![existing.to_vec(), lookalike.to_vec()];
    want.sort();
    assert_eq!(entry_names(&tmp.path), want, "no new file may appear");
    assert_no_display_artifact(&tmp.path);
}

#[test]
fn double_dash_makes_dash_led_names_plain_paths() {
    // --- with `--`: a '-'-led name is a file name and succeeds ---
    let tmp = Tmp::new("dash-ok");
    let run = run_keygen(&tmp.path, &[OsStr::new("--"), OsStr::new("-weird-name")]);
    assert_key_generated(&tmp.path, b"-weird-name", &run);
    assert_eq!(entry_names(&tmp.path), vec![b"-weird-name".to_vec()]);

    // --- with `--`: '-'-led *and* non-UTF-8, still a plain file name ---
    let tmp = Tmp::new("dash-non-utf8-ok");
    let raw_name: &[u8] = b"-\xffkey";
    let run = run_keygen(&tmp.path, &[OsStr::new("--"), &os(raw_name)]);
    assert_key_generated(&tmp.path, raw_name, &run);
    assert_eq!(entry_names(&tmp.path), vec![raw_name.to_vec()]);
    assert_no_display_artifact(&tmp.path);
}

#[test]
fn dash_led_argument_without_double_dash_is_an_unknown_option() {
    for raw_name in [&b"-weird-name"[..], b"-\xffkey"] {
        let tmp = Tmp::new("dash-rejected");
        let run = run_keygen(&tmp.path, &[&os(raw_name)]);
        assert_eq!(run.rc, 2, "a '-'-led argument without -- must exit 2");
        assert!(
            String::from_utf8_lossy(&run.err).contains("Usage"),
            "usage must be printed on stderr, got {}",
            String::from_utf8_lossy(&run.err)
        );
        assert!(
            run.out.is_empty(),
            "no success message for an argument error"
        );
        assert!(
            entry_names(&tmp.path).is_empty(),
            "an argument error must not leave a new file behind"
        );
    }
}

#[test]
fn double_dash_keeps_the_exactly_one_path_requirement() {
    // `--` alone: the path is still missing.
    let tmp = Tmp::new("dash-missing");
    let run = run_keygen(&tmp.path, &[OsStr::new("--")]);
    assert_eq!(run.rc, 2);
    assert!(String::from_utf8_lossy(&run.err).contains("Usage"));
    assert!(entry_names(&tmp.path).is_empty());

    // `--` followed by two names: still exactly one path is accepted.
    let tmp = Tmp::new("dash-two-args");
    let run = run_keygen(
        &tmp.path,
        &[OsStr::new("--"), OsStr::new("-a"), OsStr::new("-b")],
    );
    assert_eq!(run.rc, 2);
    assert!(String::from_utf8_lossy(&run.err).contains("Usage"));
    assert!(entry_names(&tmp.path).is_empty());
}
