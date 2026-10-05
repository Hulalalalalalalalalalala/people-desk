//! Regression coverage for keygen's handling of Unix paths that are not
//! plain UTF-8 text or that begin with '-' (see README "拒绝覆盖与路径要求").
//!
//! The rule protected here: keygen operates on the *raw* path the user
//! handed to the command. Rendering that path for a message (a lossy
//! conversion that may replace undecodable bytes with U+FFFD) is display
//! only -- it must never decide which file is created, refused, or checked
//! for existence.
//!
//! What is protected:
//!
//! * A legal file name containing non-UTF-8 bytes generates normally when
//!   the parent exists and the target does not: exit 0, exactly 32 raw key
//!   bytes at the *raw* name, mode 0600, the usual save-location line on
//!   stdout, empty stderr. The lossy display rendering must not cause a
//!   second file whose name contains the replacement character, and the
//!   program must not crash on the undecodable bytes.
//! * Two names in one directory whose raw bytes differ but whose lossy
//!   display is identical are still distinct targets: existence is judged
//!   by the raw path only. The display-twin is never mistaken for the
//!   target, never rewritten, never re-permissioned. When the raw path
//!   itself already exists, keygen exits 1, explains on stderr, prints no
//!   success line, preserves the existing file byte-for-byte -- and does
//!   not fall back to creating the display-rendered name instead.
//! * `--` (immediately after `keygen`) ends option parsing: the single
//!   remaining argument is a file name even if it starts with '-' or also
//!   carries non-UTF-8 bytes, and the success contract above still holds.
//!   Without `--`, a '-'-led argument stays an unknown-option error: exit
//!   2, usage on stderr, no new file. `--` does not relax the
//!   exactly-one-path requirement.
#![cfg(unix)]

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const KEY_LEN: usize = 32;
const MODE_0600: u32 = 0o600;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_wrapfile"))
}

/// View raw bytes as a path/name without any decoding or replacement.
fn os(bytes: &[u8]) -> &OsStr {
    OsStr::from_bytes(bytes)
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

    fn child(&self, name: &OsStr) -> PathBuf {
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

/// Run `wrapfile <args...>` with `cwd` as the working directory. Arguments
/// are passed as raw OS strings, so non-UTF-8 and '-'-led names reach the
/// program exactly as a shell would deliver them.
fn run_os(args: &[&OsStr], cwd: &Path) -> Run {
    let o = Command::new(bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    Run {
        rc: o.status.code().unwrap_or(-1),
        out: o.stdout,
        err: o.stderr,
    }
}

/// `wrapfile keygen <path>` with the path given as raw bytes.
fn run_keygen(path: &Path, cwd: &Path) -> Run {
    run_os(&[OsStr::new("keygen"), path.as_os_str()], cwd)
}

// ---------------------------------------------------------------------------
// filesystem assertions
// ---------------------------------------------------------------------------

/// The raw byte names of every entry in `dir`, sorted. Comparing these
/// (not their lossy renderings) is what proves no replacement-character
/// file appeared and no raw name was rewritten.
fn dir_entries(dir: &Path) -> Vec<Vec<u8>> {
    let mut names: Vec<Vec<u8>> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().as_bytes().to_vec())
        .collect();
    names.sort();
    names
}

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

/// The full success contract. `passed` is the argument exactly as handed to
/// the command (this is what the save-location line must render); `on_disk`
/// is where the key must actually land.
fn assert_key_ok(passed: &OsStr, on_disk: &Path, run: &Run) -> Vec<u8> {
    assert_eq!(
        run.rc, 0,
        "keygen must succeed (rc 0, no crash), stderr={}",
        String::from_utf8_lossy(&run.err)
    );
    assert!(
        run.err.is_empty(),
        "stderr must be empty on success, got {}",
        String::from_utf8_lossy(&run.err)
    );
    // The existing save-location prompt, rendered from the passed path.
    // Lossy display of undecodable bytes is fine here -- it is output only.
    assert_eq!(
        run.out,
        format!("Key saved to {}\n", Path::new(passed).display()).into_bytes(),
        "stdout must be exactly the save-location message"
    );

    let meta = fs::metadata(on_disk).expect("key file must exist at the raw path");
    assert_eq!(meta.len() as usize, KEY_LEN, "key must be exactly 32 bytes");
    assert_eq!(
        meta.mode() & 0o7777,
        MODE_0600,
        "key file mode must be exactly 0600"
    );

    let bytes = fs::read(on_disk).unwrap();
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
    fn file(dir: &Path, name: &OsStr, bytes: &[u8], mode: u32) -> Sentinel {
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
        assert_eq!(fs::read(&self.path).unwrap(), self.bytes, "sentinel contents changed");
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[test]
fn non_utf8_names_generate_at_the_raw_path_only() {
    // Legal-on-Unix names whose bytes are not valid UTF-8: a lone high byte,
    // a truncated multi-byte sequence, and invalid bytes around ASCII.
    let names: [&[u8]; 3] = [b"key-\xff", b"k\x80y", b"\xf0\x28\x8c\x28.key"];

    for name in names {
        let tmp = Tmp::new("non-utf8");
        let key = tmp.child(os(name));

        // The display rendering of this name genuinely differs from the raw
        // name -- otherwise the test would prove nothing.
        let display_name = String::from_utf8_lossy(name).into_owned();
        assert!(
            display_name.contains('\u{FFFD}'),
            "test name must actually be non-UTF-8: {name:?}"
        );
        assert_ne!(display_name.as_bytes(), name);

        let run = run_keygen(&key, &tmp.path);
        assert_key_ok(key.as_os_str(), &key, &run);

        // The directory holds exactly one new entry, and it is the raw name.
        // In particular the lossy display form (with U+FFFD substituted) was
        // not used to create anything.
        assert_eq!(
            dir_entries(&tmp.path),
            vec![name.to_vec()],
            "only the raw-named key file may exist; display conversion must not create a file"
        );
        assert!(
            !tmp.child(OsStr::new(&display_name)).exists(),
            "no file may appear under the replacement-character display name"
        );
    }
}

#[test]
fn display_identical_names_are_distinct_targets() {
    let tmp = Tmp::new("display-collision");
    // Different raw bytes, identical lossy display ("a\u{FFFD}z").
    let sentinel_name: &[u8] = b"a\xffz";
    let target_name: &[u8] = b"a\xfez";
    assert_eq!(
        String::from_utf8_lossy(sentinel_name),
        String::from_utf8_lossy(target_name),
        "the two names must render identically for display"
    );

    let sentinel = Sentinel::file(&tmp.path, os(sentinel_name), b"pre-existing-material", 0o640);
    let target = tmp.child(os(target_name));

    // The display-twin already existing must not make the raw target count
    // as "already exists": existence is judged by the raw path only.
    let run = run_keygen(&target, &tmp.path);
    assert_key_ok(target.as_os_str(), &target, &run);
    sentinel.assert_untouched();

    let mut want: Vec<Vec<u8>> = vec![sentinel_name.to_vec(), target_name.to_vec()];
    want.sort();
    assert_eq!(
        dir_entries(&tmp.path),
        want,
        "the display-twin must be untouched and no display-named file may appear"
    );

    // The reverse direction: when the user's raw path itself exists, keygen
    // refuses -- and must not fall back to the display-rendered name.
    let run = run_keygen(&sentinel.path, &tmp.path);
    assert_eq!(run.rc, 1, "an existing raw target must be refused with exit 1");
    assert!(run.out.is_empty(), "no success message on refusal");
    assert!(!run.err.is_empty(), "stderr must explain the refusal");
    sentinel.assert_untouched();
    assert_eq!(
        dir_entries(&tmp.path),
        want,
        "refusal must not create a key under the display-rendered name"
    );
}

#[test]
fn double_dash_allows_dash_led_and_non_utf8_names() {
    let tmp = Tmp::new("dash-ok");

    // A '-'-led name after `--` is a plain file name.
    let run = run_os(
        &[OsStr::new("keygen"), OsStr::new("--"), os(b"-plain")],
        &tmp.path,
    );
    assert_key_ok(os(b"-plain"), &tmp.child(os(b"-plain")), &run);

    // A name that both starts with '-' and contains non-UTF-8 bytes.
    let weird: &[u8] = b"-\xffk";
    let run = run_os(
        &[OsStr::new("keygen"), OsStr::new("--"), os(weird)],
        &tmp.path,
    );
    assert_key_ok(os(weird), &tmp.child(os(weird)), &run);

    // Both keys landed under their raw names; nothing display-rendered.
    let mut want: Vec<Vec<u8>> = vec![b"-plain".to_vec(), weird.to_vec()];
    want.sort();
    assert_eq!(dir_entries(&tmp.path), want);
}

#[test]
fn without_double_dash_a_dash_led_argument_is_an_unknown_option() {
    let tmp = Tmp::new("dash-err");

    // Both a plain '-'-led argument and one that also carries non-UTF-8
    // bytes keep the existing unknown-option behaviour.
    for arg in [&b"-plain"[..], b"-\xffk"] {
        let before = dir_entries(&tmp.path);
        let run = run_os(&[OsStr::new("keygen"), os(arg)], &tmp.path);
        assert_eq!(run.rc, 2, "unknown option must exit 2 (arg {arg:?})");
        assert!(run.out.is_empty(), "no output on stdout for arg {arg:?}");
        assert!(
            String::from_utf8_lossy(&run.err).contains("Usage"),
            "usage must be printed on stderr for arg {arg:?}"
        );
        assert_eq!(
            dir_entries(&tmp.path),
            before,
            "an argument error must not create any file (arg {arg:?})"
        );
        assert!(
            !tmp.child(os(arg)).exists(),
            "the rejected '-'-led name must not appear on disk"
        );
    }
}

#[test]
fn double_dash_still_requires_exactly_one_path() {
    let tmp = Tmp::new("dash-arity");

    // `--` alone: the path is still missing.
    let run = run_os(&[OsStr::new("keygen"), OsStr::new("--")], &tmp.path);
    assert_eq!(run.rc, 2);
    assert!(String::from_utf8_lossy(&run.err).contains("Usage"));

    // `--` followed by two paths: still an arity error, and neither name --
    // not even the '-'-led one -- is created.
    let run = run_os(
        &[
            OsStr::new("keygen"),
            OsStr::new("--"),
            os(b"-a"),
            os(b"-b"),
        ],
        &tmp.path,
    );
    assert_eq!(run.rc, 2);
    assert!(String::from_utf8_lossy(&run.err).contains("Usage"));

    assert!(
        dir_entries(&tmp.path).is_empty(),
        "argument errors must leave the directory empty"
    );
}

#[test]
fn existing_behaviour_for_plain_paths_is_unchanged() {
    // --version still works exactly as before.
    let tmp = Tmp::new("plain");
    let run = run_os(&[OsStr::new("--version")], &tmp.path);
    assert_eq!(run.rc, 0);
    assert_eq!(run.out, b"wrapfile 0.1.0\n");
    assert!(run.err.is_empty());

    // A plain UTF-8 path generates normally...
    let key = tmp.child(OsStr::new("plain.key"));
    let run = run_keygen(&key, &tmp.path);
    assert_key_ok(key.as_os_str(), &key, &run);

    // ...and a second generation at the same path is still refused without
    // touching the first key.
    let first = fs::read(&key).unwrap();
    let run = run_keygen(&key, &tmp.path);
    assert_eq!(run.rc, 1);
    assert!(run.out.is_empty());
    assert!(!run.err.is_empty());
    assert_eq!(fs::read(&key).unwrap(), first, "the existing key must survive");
    assert_eq!(mode_of(&key), MODE_0600, "the existing key's mode must survive");
}
