//! Regression coverage for keygen's Unix path handling (see README
//! "拒绝覆盖与路径要求"、"路径在提示中的显示" and "参数与状态码").
//!
//! The rule protected here: keygen operates on the *raw* path the user
//! handed to the command. Rendering that path for a message must never decide
//! which file is created or refused -- the display string is not a filesystem
//! name -- yet the rendering must let the user tell the actual target apart:
//!
//! * plain UTF-8 without ASCII control bytes, `"` or `\\` is shown bare;
//! * every other name is wrapped in double quotes, with newline/CR/tab shown
//!   as the two characters `\\n`/`\\r`/`\\t`, `"` and `\\` escaped, and any
//!   other ASCII control byte or raw non-UTF-8 byte shown as `\\xhh`
//!   (lowercase hex). No raw control byte (hence no injected line break or
//!   terminal escape) can survive into the message;
//! * the rendering is unambiguous: a real 0xff byte (`\\xff` inside quotes)
//!   differs from a name literally containing the four characters `\\xff`
//!   (`\\\\xff`), and a real newline differs from a literal backslash-n;
//! * success, create/save errors, unfinished-cleanup warnings, and the
//!   unknown-option error all use the very same rendering.
//!
//! What is protected here:
//!
//! * A legal file name containing non-UTF-8 bytes generates a key normally
//!   when the parent exists and is writable and the target does not exist:
//!   exit 0, exactly 32 raw key bytes at the *raw* name, mode 0600, the
//!   usual save-location line on stdout (with the bytes shown as `\\xhh`
//!   escapes inside quotes), empty stderr. No extra file whose name contains
//!   the U+FFFD replacement character is created as a side effect of display
//!   conversion, and the command does not crash.
//! * Two names in one directory whose raw bytes differ are distinct targets
//!   and now also render distinctly: existence is judged only by the raw
//!   path the user passed. The look-alike sibling is never treated as the
//!   target, overwritten, or re-permissioned. If the raw target itself
//!   exists, the command exits 1, explains on stderr, prints no success
//!   message, preserves the existing file's contents and permissions, and
//!   does not fall back to generating a key under the displayed name.
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

mod common;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const KEY_LEN: usize = 32;
const MODE_0600: u32 = 0o600;
/// UTF-8 encoding of U+FFFD, which a lossy display substitutes for each
/// undecodable byte. A file whose name contains these bytes would be an
/// artifact of display conversion, not of the user's request.
const REPLACEMENT: &[u8] = "\u{FFFD}".as_bytes();

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_wrapfile"))
}

fn os(raw: &[u8]) -> OsString {
    OsString::from_vec(raw.to_vec())
}

/// Independent reference implementation of the program's path rendering,
/// written for the tests from the README rules, so the assertions below
/// cross-check the real output rather than reuse the program's code.
///
/// Plain UTF-8 without ASCII control bytes, `"` or `\\` stays bare; any
/// other byte string is wrapped in double quotes, newline/CR/tab become the
/// two-character sequences `\\n`/`\\r`/`\\t`, `"` and `\\` are escaped, and
/// any remaining ASCII control byte or undecodable raw byte becomes `\\xhh`
/// with lowercase hex. Valid multibyte characters keep their bytes.
fn rendered(raw: &[u8]) -> Vec<u8> {
    fn plain_byte(b: u8) -> bool {
        b >= 0x20 && b != b'"' && b != b'\\' && b != 0x7f
    }

    let plain = std::str::from_utf8(raw).is_ok() && raw.iter().all(|&b| plain_byte(b));
    if plain {
        return raw.to_vec();
    }

    let mut out: Vec<u8> = vec![b'"'];
    let mut i = 0;
    while i < raw.len() {
        let b = raw[i];
        if b < 0x80 {
            match b {
                b'\n' => out.extend_from_slice(br"\n"),
                b'\r' => out.extend_from_slice(br"\r"),
                b'\t' => out.extend_from_slice(br"\t"),
                b'"' => out.extend_from_slice(br#"\""#),
                b'\\' => out.extend_from_slice(br"\\"),
                0x20..=0x7e => out.push(b),
                _ => out.extend_from_slice(format!("\\x{b:02x}").as_bytes()),
            }
            i += 1;
            continue;
        }
        let seq_len = match b {
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 1,
        };
        if seq_len > 1
            && i + seq_len <= raw.len()
            && std::str::from_utf8(&raw[i..i + seq_len]).is_ok()
        {
            out.extend_from_slice(&raw[i..i + seq_len]);
            i += seq_len;
        } else {
            out.extend_from_slice(format!("\\x{b:02x}").as_bytes());
            i += 1;
        }
    }
    out.push(b'"');
    out
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

/// Like [run_keygen], but with extra environment variables -- used to load
/// the permfail shim when a failure-plus-cleanup message must be inspected.
fn run_keygen_env(dir: &Path, rest: &[&OsStr], extra_env: &[(&str, &str)]) -> Run {
    let mut args: Vec<&OsStr> = vec![OsStr::new("keygen")];
    args.extend_from_slice(rest);
    let mut cmd = Command::new(bin());
    cmd.args(&args).current_dir(dir);
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

fn permfail_so() -> Option<PathBuf> {
    common::permfail_so()
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
/// stdout (with the path rendered per the escaping rules), and exactly 32
/// raw key bytes in a 0600 file at the *raw* name.
fn assert_key_generated(dir: &Path, raw_name: &[u8], run: &Run) -> Vec<u8> {
    assert_eq!(run.rc, 0, "stderr={}", String::from_utf8_lossy(&run.err));
    assert!(
        run.err.is_empty(),
        "stderr must be empty on success, got {}",
        String::from_utf8_lossy(&run.err)
    );

    let mut expected_out = b"Key saved to ".to_vec();
    expected_out.extend_from_slice(&rendered(raw_name));
    expected_out.push(b'\n');
    assert_eq!(
        run.out, expected_out,
        "stdout must be exactly the save-location message for the raw path"
    );

    let key = dir.join(os(raw_name));
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
    // Different raw bytes that an unescaped lossy display would both show
    // as "key-�" -- the confusion this test guards against. The new
    // rendering keeps them apart ("key-\xff" vs "key-\xfe").
    let existing: &[u8] = b"key-\xff";
    let target: &[u8] = b"key-\xfe";
    assert_ne!(
        rendered(existing),
        rendered(target),
        "the escaping rules must render the two names distinctly"
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
    assert_ne!(
        rendered(existing),
        rendered(lookalike),
        "the escaping rules must render the two names distinctly"
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
        run.err
            .windows(rendered(existing).len())
            .any(|w| w == rendered(existing)),
        "stderr should show the refused target via its escaped rendering, got: {stderr}"
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
    // (raw option text, label) -- including names that carry a newline or a
    // non-UTF-8 byte, which must reach the option error escaped rather than
    // raw.
    let cases: &[(&[u8], &str)] = &[
        (b"-weird-name", "plain"),
        (b"-\xffkey", "non-utf8"),
        (b"-x\ny", "newline"),
        (b"-\x1bz", "escape"),
    ];
    for (raw_name, label) in cases {
        let tmp = Tmp::new("dash-rejected");
        let run = run_keygen(&tmp.path, &[&os(raw_name)]);
        assert_eq!(run.rc, 2, "a '-'-led argument without -- must exit 2 ({label})");
        let stderr = String::from_utf8_lossy(&run.err);
        assert!(
            stderr.contains("Usage"),
            "usage must be printed on stderr, got {stderr}"
        );
        // The option is shown with the same escaped rendering as every path
        // message: exact first line, no raw control byte surviving.
        let mut want_first = b"wrapfile keygen: unknown option ".to_vec();
        want_first.extend_from_slice(&rendered(raw_name));
        want_first.push(b'\n');
        assert!(
            run.err.starts_with(&want_first),
            "unknown-option line must use the escaped rendering ({label}), got: {:?}",
            String::from_utf8_lossy(&run.err)
        );
        // The rendered option itself occupies exactly one line: only the
        // line terminator after it may appear before the usage text.
        let first_line_end = run.err.iter().position(|&b| b == b'\n').unwrap();
        let first_line = &run.err[..first_line_end];
        assert!(
            !first_line.iter().any(|&b| b < 0x20 || b == 0x7f),
            "no raw control byte may survive in the option rendering ({label})"
        );
        assert!(
            !first_line.contains(&0x1b),
            "an ESC byte must not reach the terminal raw ({label})"
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

// ---------------------------------------------------------------------------
// message rendering: escaping of control bytes, quotes, backslashes
// ---------------------------------------------------------------------------

/// Assert the success line is exactly `Key saved to <rendered(raw)>\n`,
/// stderr is empty, and no raw control byte survives into stdout.
fn assert_rendered_success_line(dir: &Path, raw: &[u8]) {
    let run = run_keygen(dir, &[&os(raw)]);
    assert_eq!(run.rc, 0, "stderr={}", String::from_utf8_lossy(&run.err));
    assert!(run.err.is_empty());
    let mut want = b"Key saved to ".to_vec();
    want.extend_from_slice(&rendered(raw));
    want.push(b'\n');
    assert_eq!(run.out, want, "success line must use the escaped rendering");
    assert!(
        !run.out[..run.out.len() - 1].iter().any(|&b| b < 0x20 || b == 0x7f),
        "no raw control byte may remain in the message apart from its final newline"
    );
}

#[test]
fn control_bytes_in_a_name_are_escaped_on_success_and_split_no_lines() {
    let tmp = Tmp::new("ctrl-success");
    let raw: &[u8] = b"key\nalign\x1b\t\r.key";
    assert_rendered_success_line(&tmp.path, raw);

    // The filesystem operation used the raw name, not the rendered one.
    let meta = fs::metadata(tmp.path.join(os(raw))).expect("file at the raw name");
    assert_eq!(meta.len() as usize, KEY_LEN);
    assert_eq!(meta.mode() & 0o7777, MODE_0600);
    // No file was created under the escaped spelling.
    assert!(fs::metadata(tmp.path.join(os(br#"key\nalign\x1b\t\r.key"#))).is_err());
    assert_eq!(entry_names(&tmp.path), vec![raw.to_vec()]);
}

#[test]
fn quote_and_backslash_names_stay_single_targets() {
    let tmp = Tmp::new("quote-backslash");
    let raw: &[u8] = br#"a"b\c.key"#;
    assert_rendered_success_line(&tmp.path, raw);
    assert_eq!(entry_names(&tmp.path), vec![raw.to_vec()]);
}

#[test]
fn literal_backslash_spelling_is_not_confused_with_a_real_byte() {
    let tmp = Tmp::new("literal-vs-real");

    // A file whose name literally contains backslash-x-f-f.
    let literal: &[u8] = br#"key-\xff"#;
    assert_rendered_success_line(&tmp.path, literal);

    // A genuinely non-UTF-8 name.
    let raw = {
        let mut v = b"key-".to_vec();
        v.push(0xff);
        v
    };
    assert_rendered_success_line(&tmp.path, &raw);

    // Both files coexist: the two visible spellings name two different
    // targets.
    let mut names = vec![literal.to_vec(), raw.clone()];
    names.sort();
    assert_eq!(entry_names(&tmp.path), names);

    // And the two renderings visibly differ.
    assert_eq!(rendered(literal), br#""key-\\xff""#);
    assert_eq!(rendered(&raw), br#""key-\xff""#);
    assert_ne!(rendered(literal), rendered(&raw));
}

#[test]
fn real_newline_is_distinct_from_literal_backslash_n() {
    let tmp = Tmp::new("newline-vs-literal");
    let with_newline: &[u8] = b"a\nb.key";
    let literal: &[u8] = br#"a\nb.key"#;
    assert_rendered_success_line(&tmp.path, with_newline);
    assert_rendered_success_line(&tmp.path, literal);

    assert_eq!(rendered(with_newline), br#""a\nb.key""#);
    assert_eq!(rendered(literal), br#""a\\nb.key""#);
    assert_ne!(rendered(with_newline), rendered(literal));
    let mut names = vec![with_newline.to_vec(), literal.to_vec()];
    names.sort();
    assert_eq!(entry_names(&tmp.path), names);
}

#[test]
fn create_error_escapes_the_target_and_carries_no_raw_controls() {
    let tmp = Tmp::new("ctrl-create-error");
    // A name with a newline whose parent does not exist: open() fails before
    // anything is created, and the error message must render the name safely.
    let raw: &[u8] = b"missing-dir/key\nname\x1b";
    let run = run_keygen(&tmp.path, &[&os(raw)]);
    assert_eq!(run.rc, 1);
    assert!(run.out.is_empty(), "no success line on failure");
    let want = rendered(raw);
    assert!(
        run.err.windows(want.len()).any(|w| w == want),
        "stderr must show the escaped target, got: {:?}",
        String::from_utf8_lossy(&run.err)
    );
    assert!(
        !run.err[..run.err.len() - 1].iter().any(|&b| b < 0x20 || b == 0x7f),
        "no raw control byte besides the terminating newline may reach stderr"
    );
    // Nothing created, and nothing under the escaped spelling either.
    assert!(fs::metadata(tmp.path.join(os(raw))).is_err());
    assert!(entry_names(&tmp.path).is_empty());
}

#[test]
fn cleanup_message_escapes_a_non_utf8_target_when_unlink_is_refused() {
    let Some(so) = permfail_so() else {
        eprintln!("skipping: permfail shim not available on this target");
        return;
    };

    let tmp = Tmp::new("ctrl-cleanup-error");
    // Newline + ESC + a non-UTF-8 byte: the unfinished-cleanup warning must
    // render all of them and stay on one line for the path portion.
    let raw: &[u8] = b"unfinished\nkey\x1b-\xff";
    let run = run_keygen_env(
        &tmp.path,
        &[&os(raw)],
        &[
            ("LD_PRELOAD", so.to_str().unwrap()),
            ("WRAPFILE_TEST_FAIL_FCHMOD", "1"),
            ("WRAPFILE_TEST_FAIL_UNLINK", "1"),
        ],
    );
    assert_eq!(run.rc, 1, "a failed save with refused cleanup must exit 1");
    assert!(run.out.is_empty());

    let stderr = String::from_utf8_lossy(&run.err);
    let want = rendered(raw);
    assert!(
        run.err.windows(want.len()).any(|w| w == want),
        "cleanup warning must show the escaped target, got: {stderr}"
    );
    // The original failure and the cleanup failure are both still reported.
    assert!(
        stderr.contains("cleanup"),
        "stderr must report the failed cleanup, got: {stderr}"
    );
    assert!(
        stderr.contains("may still be present"),
        "stderr must warn that this run's file may remain, got: {stderr}"
    );
    // The refused unlink really leaves the file at the raw name only.
    assert!(
        fs::metadata(tmp.path.join(os(raw))).is_ok(),
        "the refused cleanup leaves the file at the raw path"
    );
    assert_no_display_artifact(&tmp.path);
    assert_eq!(entry_names(&tmp.path), vec![raw.to_vec()]);

    // Remove the residue (the test harness runs with full rights) so Drop
    // can wipe the scratch dir.
    let _ = fs::remove_file(tmp.path.join(os(raw)));
}
