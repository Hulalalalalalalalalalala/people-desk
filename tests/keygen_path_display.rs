//! Regression coverage for keygen's path *display* rules (see README
//! "提示中的路径表示").
//!
//! Paths are shown to the user in exactly one escaped form, everywhere the
//! command names a path (success line, create/save errors, cleanup
//! warnings, unknown-option errors):
//!
//! * A path that is valid UTF-8 and contains no ASCII control byte, double
//!   quote, or backslash is shown verbatim.
//! * Any other path is shown between double quotes: newline, carriage
//!   return and tab appear as the two-character sequences \n \r \t, double
//!   quote and backslash as \" and \\, every other ASCII control byte and
//!   every raw byte that is not part of a valid UTF-8 sequence as \xhh
//!   (two lowercase hex digits). Non-ASCII text such as Chinese is kept
//!   as-is.
//!
//! The display is read-only text: no raw ASCII control byte may survive in
//! the output (so a name cannot break a message into extra lines, drive
//! the terminal, or imitate another message), two names share a display
//! only if their raw bytes are identical, and file operations always use
//! the raw path the user passed -- never the escaped text.
#![cfg(unix)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
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
            "wrapfile-keygen-display-tests-{}-{}-{}",
            std::process::id(),
            label,
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Tmp { path }
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
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
/// can be passed as bare (relative) file names of any byte content.
fn run_in(dir: &Path, args: &[&OsStr]) -> Run {
    run_with_env(dir, args, &[])
}

fn run_with_env(dir: &Path, args: &[&OsStr], env: &[(&str, &str)]) -> Run {
    let mut cmd = Command::new(bin());
    cmd.args(args).current_dir(dir);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let o = cmd.output().unwrap();
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
// shared assertions
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

/// Output meant for the user must not carry raw ASCII control bytes: the
/// only one allowed is the '\n' that terminates each output line itself.
/// In particular no raw newline/CR/tab/ESC/DEL from a file name may leak
/// into a message.
fn assert_no_raw_control_bytes(what: &str, bytes: &[u8]) {
    for (i, &b) in bytes.iter().enumerate() {
        assert!(
            b == b'\n' || (0x20..0x7f).contains(&b) || b >= 0x80,
            "{what}: raw control byte 0x{b:02x} at offset {i} must have been escaped: {}",
            String::from_utf8_lossy(bytes)
        );
    }
}

/// Assert the full success contract for generating `raw_name` in `dir`,
/// where `expected` is the exact display form the path must be shown in.
fn assert_generated_with_display(dir: &Path, raw_name: &[u8], expected: &str, run: &Run) {
    assert_eq!(run.rc, 0, "stderr={}", String::from_utf8_lossy(&run.err));
    assert!(
        run.err.is_empty(),
        "stderr must be empty on success, got {}",
        String::from_utf8_lossy(&run.err)
    );
    let expected_out = format!("Key saved to {expected}\n").into_bytes();
    assert_eq!(
        run.out, expected_out,
        "stdout must be exactly the save-location line with the documented display form"
    );
    assert_no_raw_control_bytes("stdout", &run.out);

    let key = dir.join(os(raw_name));
    let meta = fs::metadata(&key).expect("key file must exist at the raw path");
    assert_eq!(meta.len() as usize, KEY_LEN, "key must be exactly 32 bytes");
    assert_eq!(
        meta.mode() & 0o7777,
        MODE_0600,
        "key file mode must be exactly 0600"
    );
}

// ---------------------------------------------------------------------------
// the display rules themselves
// ---------------------------------------------------------------------------

#[test]
fn plain_utf8_paths_are_shown_verbatim() {
    // No ASCII control byte, no '"', no '\': the display is the path
    // itself, unquoted -- including non-ASCII text such as Chinese.
    for (label, raw_name) in [
        ("plain", &b"backup.key"[..]),
        ("spaces", b"my backup key.bin"),
        ("chinese", "密钥-备份.key".as_bytes()),
        ("mixed", "backup 密钥 #2 (final).key".as_bytes()),
    ] {
        let tmp = Tmp::new(label);
        let run = run_keygen(&tmp.path, &[&os(raw_name)]);
        let expected = std::str::from_utf8(raw_name).unwrap();
        assert_generated_with_display(&tmp.path, raw_name, expected, &run);
        assert_eq!(entry_names(&tmp.path), vec![raw_name.to_vec()]);
    }
}

#[test]
fn special_bytes_are_escaped_inside_double_quotes() {
    let cases: &[(&str, &[u8], &str)] = &[
        ("newline", b"a\nb.key", "\"a\\nb.key\""),
        ("carriage-return", b"a\rb.key", "\"a\\rb.key\""),
        ("tab", b"a\tb.key", "\"a\\tb.key\""),
        ("quote", b"say \"hi\".key", "\"say \\\"hi\\\".key\""),
        ("backslash", b"back\\slash.key", "\"back\\\\slash.key\""),
        ("escape-byte", b"a\x1b[0m.key", "\"a\\x1b[0m.key\""),
        ("ctrl-byte", b"a\x01b.key", "\"a\\x01b.key\""),
        ("del-byte", b"a\x7fb.key", "\"a\\x7fb.key\""),
        ("non-utf8", b"raw \xff\xfe.key", "\"raw \\xff\\xfe.key\""),
        ("truncated-utf8", b"cut \xe5\xaf.key", "\"cut \\xe5\\xaf.key\""),
        ("chinese-newline", "密钥\n备份.key".as_bytes(), "\"密钥\\n备份.key\""),
    ];
    for (label, raw_name, expected) in cases {
        let tmp = Tmp::new(label);
        let run = run_keygen(&tmp.path, &[&os(raw_name)]);
        assert_generated_with_display(&tmp.path, raw_name, expected, &run);
        // The only file created is the one at the raw name: the escaped
        // text shown to the user is never used as a filesystem name.
        assert_eq!(
            entry_names(&tmp.path),
            vec![raw_name.to_vec()],
            "{label}: only the raw name may be created"
        );
    }
}

#[test]
fn escaped_display_distinguishes_escape_text_from_real_bytes() {
    // A name containing the actual byte 0xff and a name literally spelling
    // '\xff' (backslash, x, f, f) must not share a visible form -- and the
    // same for a real newline vs. a literal backslash followed by 'n'.
    let tmp = Tmp::new("escape-vs-real");
    let pairs: &[(&[u8], &[u8], &str, &str)] = &[
        (b"key-\xff", b"key-\\xff", "\"key-\\xff\"", "\"key-\\\\xff\""),
        (b"key-\nend", b"key-\\nend", "\"key-\\nend\"", "\"key-\\\\nend\""),
    ];
    for (real, literal, real_display, literal_display) in pairs {
        assert_ne!(
            real_display, literal_display,
            "test premise: the two displays must differ"
        );

        let run = run_keygen(&tmp.path, &[&os(real)]);
        assert_generated_with_display(&tmp.path, real, real_display, &run);

        let run = run_keygen(&tmp.path, &[&os(literal)]);
        assert_generated_with_display(&tmp.path, literal, literal_display, &run);
    }

    // All four raw names exist side by side; nothing was conflated.
    let mut want: Vec<Vec<u8>> = pairs
        .iter()
        .flat_map(|(a, b, _, _)| [a.to_vec(), b.to_vec()])
        .collect();
    want.sort();
    assert_eq!(entry_names(&tmp.path), want);
}

#[test]
fn the_escaped_text_is_not_a_filesystem_name() {
    let tmp = Tmp::new("display-not-a-name");
    let raw_name: &[u8] = b"a\nb.key";

    let run = run_keygen(&tmp.path, &[&os(raw_name)]);
    assert_generated_with_display(&tmp.path, raw_name, "\"a\\nb.key\"", &run);

    // No file appears under the displayed text: not the quoted form, not
    // the backslash-n form, not any unescaped variant.
    assert_eq!(entry_names(&tmp.path), vec![raw_name.to_vec()]);

    // Passing the escaped text back as an argument names a *different*
    // file (one whose name literally contains a backslash), which does not
    // yet exist -- so it is generated as its own target, shown with the
    // backslash itself escaped.
    let literal: &[u8] = b"a\\nb.key";
    let run = run_keygen(&tmp.path, &[&os(literal)]);
    assert_generated_with_display(&tmp.path, literal, "\"a\\\\nb.key\"", &run);

    let mut want = vec![raw_name.to_vec(), literal.to_vec()];
    want.sort();
    assert_eq!(entry_names(&tmp.path), want);
}

// ---------------------------------------------------------------------------
// one display form across every message
// ---------------------------------------------------------------------------

#[test]
fn create_error_uses_the_same_display() {
    let tmp = Tmp::new("create-error");
    let raw_name: &[u8] = b"dup\nkey.bin";

    let first = run_keygen(&tmp.path, &[&os(raw_name)]);
    assert_eq!(first.rc, 0, "setup generation must succeed");

    let run = run_keygen(&tmp.path, &[&os(raw_name)]);
    assert_eq!(run.rc, 1, "an existing target must be refused");
    assert!(run.out.is_empty(), "no success message on failure");
    let stderr = String::from_utf8_lossy(&run.err);
    assert!(
        stderr.contains("\"dup\\nkey.bin\""),
        "stderr must name the target in the same escaped display form, got: {stderr}"
    );
    assert!(
        !stderr.contains("dup\nkey"),
        "the raw newline must not reach stderr: {stderr}"
    );
    assert_no_raw_control_bytes("stderr", &run.err);

    // The pre-existing key is untouched and nothing else appeared.
    let key = fs::read(tmp.path.join(os(raw_name))).unwrap();
    assert_eq!(key.len(), KEY_LEN);
    assert_eq!(entry_names(&tmp.path), vec![raw_name.to_vec()]);
}

#[test]
fn unknown_option_uses_the_same_display() {
    // A '-'-led argument without `--` is an unknown option; if its bytes
    // need escaping, the error shows the same escaped form and carries no
    // raw control bytes.
    let tmp = Tmp::new("unknown-option");
    let raw_name: &[u8] = b"-x\ny\x1b\xff";
    let run = run_keygen(&tmp.path, &[&os(raw_name)]);
    assert_eq!(run.rc, 2, "a '-'-led argument without -- must exit 2");
    assert!(run.out.is_empty());
    let stderr = String::from_utf8_lossy(&run.err);
    assert!(
        stderr.contains("\"-x\\ny\\x1b\\xff\""),
        "the unknown option must be shown in the escaped display form, got: {stderr}"
    );
    assert!(stderr.contains("Usage"), "usage must be printed, got: {stderr}");
    assert_no_raw_control_bytes("stderr", &run.err);
    assert!(
        entry_names(&tmp.path).is_empty(),
        "an argument error must not create a file"
    );
}

#[test]
fn cleanup_warning_uses_the_same_display() {
    // Force the save to fail (fchmod EPERM) and the cleanup unlink to be
    // refused (EACCES): the warning about the possibly remaining file must
    // name the target in the same escaped display form.
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not built");
        return;
    };
    let tmp = Tmp::new("cleanup-warning");
    let raw_name: &[u8] =b"half\nsaved.key";

    let so_str = so.to_str().unwrap().to_string();
    let run = run_with_env(
        &tmp.path,
        &[OsStr::new("keygen"), &os(raw_name)],
        &[
            ("LD_PRELOAD", so_str.as_str()),
            ("WRAPFILE_TEST_FAIL_FCHMOD", "1"),
            ("WRAPFILE_TEST_FAIL_UNLINK", "1"),
        ],
    );
    assert_eq!(run.rc, 1, "the failed save must exit 1");
    assert!(run.out.is_empty(), "no success message on failure");
    let stderr = String::from_utf8_lossy(&run.err);
    assert!(
        stderr.contains("\"half\\nsaved.key\""),
        "the cleanup warning must name the target in the escaped display form, got: {stderr}"
    );
    assert!(
        stderr.contains("cleanup") || stderr.contains("清理"),
        "stderr must report that cleanup failed, got: {stderr}"
    );
    assert_no_raw_control_bytes("stderr", &run.err);

    // The leftover, if any, sits at the raw name -- never at a name built
    // from the displayed text.
    for name in entry_names(&tmp.path) {
        assert_eq!(name, raw_name.to_vec(), "only the raw target may remain");
    }
}

#[test]
fn dash_led_path_with_special_bytes_after_double_dash() {
    // `keygen --` still makes a '-'-led name a plain path when the name
    // also needs escaping; the success line shows the escaped form.
    let tmp = Tmp::new("dash-special");
    let raw_name: &[u8] = b"-n\nkey";
    let run = run_keygen(&tmp.path, &[OsStr::new("--"), &os(raw_name)]);
    assert_generated_with_display(&tmp.path, raw_name, "\"-n\\nkey\"", &run);
    assert_eq!(entry_names(&tmp.path), vec![raw_name.to_vec()]);
}

#[test]
fn output_lines_stay_single_lines() {
    // A name crafted to imitate a second message (its own "Key saved to"
    // line, plus terminal colour reset) must arrive as one quoted,
    // single-line display -- the real success line stays the only one.
    let tmp = Tmp::new("no-fake-lines");
    let raw_name: &[u8] = b"x\nKey saved to forged\n\x1b[32mOK.key";
    let run = run_keygen(&tmp.path, &[&os(raw_name)]);
    assert_eq!(run.rc, 0, "stderr={}", String::from_utf8_lossy(&run.err));

    // Exactly one line of output, and it is the genuine success line.
    let text = String::from_utf8(run.out.clone()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "output must be a single line, got: {text:?}");
    assert_eq!(
        lines[0],
        "Key saved to \"x\\nKey saved to forged\\n\\x1b[32mOK.key\""
    );
    assert_no_raw_control_bytes("stdout", &run.out);
    assert_eq!(entry_names(&tmp.path), vec![raw_name.to_vec()]);
}
