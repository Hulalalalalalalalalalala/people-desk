//! Unix regression tests for `wrapfile keygen`, centered on the key file
//! permission contract:
//!
//!  * success under umask 0022 / 0000 / 0777 yields exactly 32 bytes with
//!    mode exactly 0600 (owner rw only, no group/other/special bits), exit
//!    code 0, and only a save-location notice on stdout;
//!  * the file carries 0600 *before* any key byte is written (verified with
//!    an LD_PRELOAD shim that records the mode of the key file at every
//!    write(2));
//!  * if the filesystem refuses the required mode, or the mode cannot be
//!    confirmed, the command exits 1, reports the permission problem on
//!    stderr, prints no success notice, and removes the file it created;
//!  * pre-existing files, directories and symlinks are never modified, and
//!    failures leave no partial file behind;
//!  * existing CLI behaviour (--version, usage errors, `--` separator) is
//!    preserved.

#![cfg(unix)]

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

const BIN: &str = env!("CARGO_BIN_EXE_wrapfile");

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

/// A unique temporary directory, removed on drop (best effort).
struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "wrapfile-test-{label}-{}-{n}",
            std::process::id()
        ));
        fs::create_dir(&dir).unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
        TestDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        // Best effort: a test may have tightened permissions inside.
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Run `wrapfile keygen <path>` in a child process whose umask is set to
/// `umask` just before exec, so the result does not depend on the umask the
/// test harness itself happens to run under.
fn run_keygen(path: &OsStr, umask: libc::mode_t, envs: &[(&str, &Path)]) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.arg("keygen").arg(path);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    // pre_exec runs after fork, before exec: async-signal-safe umask(2) only.
    unsafe {
        cmd.pre_exec(move || {
            libc::umask(umask);
            Ok(())
        });
    }
    cmd.output().unwrap_or_else(|e| panic!("failed to spawn {BIN}: {e}"))
}

fn run_wrapfile(args: &[&OsStr]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {BIN}: {e}"))
}

fn exit_code(out: &Output) -> i32 {
    out.status.code().expect("wrapfile terminated by signal")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Assert the full success-side contract on the generated key file.
fn assert_valid_key_file(path: &Path) {
    let md = fs::metadata(path)
        .unwrap_or_else(|e| panic!("key file {} missing: {e}", path.display()));
    assert!(
        md.file_type().is_file(),
        "key file must be a regular file, not a symlink or other object"
    );
    assert_eq!(
        md.len(),
        32,
        "key file must contain exactly 32 raw key bytes, got {}",
        md.len()
    );
    let mode = md.mode() & 0o7777;
    assert_eq!(
        mode, 0o600,
        "key file mode must be exactly 0600 (owner rw, no group/other/special \
         bits), got {mode:04o}"
    );
    // Owner read AND write must both be present; 0400/0200/0000 are failures.
    assert_eq!(mode & 0o600, 0o600, "owner read+write bits must both be set");
    // Group, other and special bits must all be clear.
    assert_eq!(mode & 0o7177, 0, "group/other/special bits must all be clear");
}

/// Assert the full failure-side contract: exit 1, a permission/problem
/// explanation on stderr, and no success notice or key material on stdout.
fn assert_keygen_failed(out: &Output) {
    assert_eq!(exit_code(&out), 1, "expected exit code 1, got {out:?}");
    assert!(
        !stderr(&out).is_empty(),
        "a failure must be explained on stderr"
    );
    assert!(
        !stdout(&out).contains("Key saved"),
        "no success notice may be printed on failure, got: {}",
        stdout(&out)
    );
}

// ---------------------------------------------------------------------------
// Success path: exactly 0600 regardless of umask
// ---------------------------------------------------------------------------

#[test]
fn keygen_succeeds_with_exact_0600_under_common_and_extreme_umasks() {
    // 0022: everyday default. 0000: nothing masked (permissive terminal).
    // 0777: everything masked, including the owner bits.
    for umask in [0o022, 0o000, 0o777] {
        let dir = TestDir::new("umask");
        let key = dir.path().join("backup.key");
        let out = run_keygen(key.as_os_str(), umask, &[]);

        assert_eq!(
            exit_code(&out),
            0,
            "umask {umask:04o}: expected exit code 0, got {out:?}"
        );
        assert_eq!(
            stdout(&out),
            format!("Key saved to {}\n", key.display()),
            "umask {umask:04o}: stdout must only announce the save location"
        );
        assert_eq!(
            stderr(&out),
            "",
            "umask {umask:04o}: stderr must be empty on success"
        );
        assert_valid_key_file(&key);
    }
}

#[test]
fn keygen_stdout_never_contains_key_bytes() {
    let dir = TestDir::new("no-leak");
    let key = dir.path().join("backup.key");
    let out = run_keygen(key.as_os_str(), 0o022, &[]);
    assert_eq!(exit_code(&out), 0);

    let key_bytes = fs::read(&key).unwrap();
    assert_eq!(key_bytes.len(), 32);
    // The exact-stdout assertion in the umask test already pins the success
    // message; here we additionally check neither stream carries the secret.
    assert!(!out.stdout.windows(32).any(|w| w == key_bytes.as_slice()));
    assert!(!out.stderr.windows(32).any(|w| w == key_bytes.as_slice()));
}

// ---------------------------------------------------------------------------
// Permission-before-write: verified via an LD_PRELOAD shim
// ---------------------------------------------------------------------------

/// C source of the interposition shim. Compiled once per test-binary run.
///
///  * Always: logs the mode of every regular file at each write(2), one
///    `%04o` line per write, to $WRAPFILE_SHIM_LOG.
///  * $WRAPFILE_SHIM_FAIL_FCHMOD set: fchmod(2) fails with EPERM, simulating
///    a filesystem that refuses to store the required mode.
///  * $WRAPFILE_SHIM_LIE_MODE=<octal>: fstat(2) reports that mode for regular
///    files, simulating a filesystem whose stored mode cannot be confirmed
///    to be 0600.
const SHIM_C: &str = r#"
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>

static ssize_t (*real_write)(int, const void *, size_t);
static int (*real_fchmod)(int, mode_t);
static int (*real_fstat)(int, struct stat *);
static int (*real_open)(const char *, int, ...);

static void resolve(void) {
    if (!real_write)  real_write  = dlsym(RTLD_NEXT, "write");
    if (!real_fchmod) real_fchmod = dlsym(RTLD_NEXT, "fchmod");
    if (!real_fstat)  real_fstat  = dlsym(RTLD_NEXT, "fstat");
    if (!real_open)   real_open   = dlsym(RTLD_NEXT, "open");
}

static void log_mode(mode_t m) {
    const char *log = getenv("WRAPFILE_SHIM_LOG");
    if (!log) return;
    int fd = real_open(log, O_WRONLY | O_CREAT | O_APPEND, 0600);
    if (fd < 0) return;
    char buf[16];
    int n = snprintf(buf, sizeof buf, "%04o\n", (unsigned)(m & 07777));
    if (n > 0) (void)real_write(fd, buf, (size_t)n);
    close(fd);
}

ssize_t write(int fd, const void *buf, size_t n) {
    resolve();
    struct stat st;
    if (real_fstat(fd, &st) == 0 && S_ISREG(st.st_mode))
        log_mode(st.st_mode);
    return real_write(fd, buf, n);
}

int fchmod(int fd, mode_t mode) {
    resolve();
    if (getenv("WRAPFILE_SHIM_FAIL_FCHMOD")) {
        errno = EPERM;
        return -1;
    }
    return real_fchmod(fd, mode);
}

int fstat(int fd, struct stat *st) {
    resolve();
    int r = real_fstat(fd, st);
    const char *lie = getenv("WRAPFILE_SHIM_LIE_MODE");
    if (r == 0 && lie && S_ISREG(st->st_mode)) {
        unsigned long m = strtoul(lie, NULL, 8);
        st->st_mode = (st->st_mode & ~(mode_t)07777) | (mode_t)(m & 07777);
    }
    return r;
}
"#;

/// Path to the compiled shim, or None if no C compiler is available.
/// Compiled once; the build directory is intentionally leaked so the .so
/// outlives all tests in this process.
fn shim() -> Option<&'static Path> {
    static SHIM: OnceLock<Option<PathBuf>> = OnceLock::new();
    SHIM.get_or_init(|| {
        let dir = TestDir::new("shim");
        let src = dir.path().join("shim.c");
        let so = dir.path().join("shim.so");
        fs::write(&src, SHIM_C).unwrap();
        let out = match Command::new("cc")
            .args(["-shared", "-fPIC", "-O2", "-Wall", "-Werror", "-o"])
            .arg(&so)
            .arg(&src)
            .arg("-ldl")
            .output()
        {
            Ok(out) => out,
            Err(_) => return None, // no C compiler: shim tests skip
        };
        assert!(
            out.status.success(),
            "shim compilation failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let so_path = so.clone();
        std::mem::forget(dir); // keep the .so alive for the whole test run
        Some(so_path)
    })
    .as_deref()
}

#[test]
fn key_bytes_are_never_written_before_mode_is_confirmed_0600() {
    let Some(shim) = shim() else {
        eprintln!("skipping: no C compiler available to build the LD_PRELOAD shim");
        return;
    };
    // 0000 is the permissive-terminal case; 0777 forces the open()-time mode
    // to 0000 so the fchmod-before-write path is exercised. In both, every
    // write(2) to the key file must already see exactly 0600.
    for umask in [0o000, 0o777] {
        let dir = TestDir::new("mode-before-write");
        let key = dir.path().join("backup.key");
        let log = dir.path().join("writes.log");
        let out = run_keygen(
            key.as_os_str(),
            umask,
            &[("LD_PRELOAD", shim), ("WRAPFILE_SHIM_LOG", &log)],
        );
        assert_eq!(exit_code(&out), 0, "umask {umask:04o}: {out:?}");

        // The child created the log under its own umask (possibly 0777, i.e.
        // mode 0000); we own it, so restore readability before reading.
        let _ = fs::set_permissions(&log, fs::Permissions::from_mode(0o600));
        let log = fs::read_to_string(&log)
            .expect("shim must observe at least the key-file write");
        let modes: Vec<&str> = log.lines().collect();
        assert!(
            !modes.is_empty(),
            "umask {umask:04o}: no write to the key file was observed"
        );
        for mode in modes {
            assert_eq!(
                mode, "0600",
                "umask {umask:04o}: a key byte was written while the file \
                 mode was {mode}, not 0600"
            );
        }
        assert_valid_key_file(&key);
    }
}

#[test]
fn filesystem_refusing_0600_fails_and_cleans_up() {
    let Some(shim) = shim() else {
        eprintln!("skipping: no C compiler available to build the LD_PRELOAD shim");
        return;
    };
    let dir = TestDir::new("fchmod-refused");
    let key = dir.path().join("backup.key");
    // A sibling that must survive the failed run untouched.
    let sibling = dir.path().join("keep.txt");
    fs::write(&sibling, b"keep").unwrap();
    fs::set_permissions(&sibling, fs::Permissions::from_mode(0o644)).unwrap();

    let out = run_keygen(
        key.as_os_str(),
        0o022,
        &[("LD_PRELOAD", shim), ("WRAPFILE_SHIM_FAIL_FCHMOD", shim)],
    );

    assert_keygen_failed(&out);
    assert!(
        stderr(&out).contains("permission"),
        "stderr must explain the permission problem, got: {}",
        stderr(&out)
    );
    assert!(
        key.symlink_metadata().is_err(),
        "the file created by the failed run must be removed"
    );
    // Parent directory holds only the untouched sibling.
    let entries: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(entries.len(), 1, "no leftover files may remain");
    assert_eq!(fs::read(&sibling).unwrap(), b"keep");
    assert_eq!(fs::metadata(&sibling).unwrap().mode() & 0o7777, 0o644);
}

#[test]
fn unconfirmable_0600_fails_and_cleans_up() {
    let Some(shim) = shim() else {
        eprintln!("skipping: no C compiler available to build the LD_PRELOAD shim");
        return;
    };
    let dir = TestDir::new("fstat-lies");
    let key = dir.path().join("backup.key");
    let lie = "0644"; // fchmod "succeeds" but the stored mode reads back wrong

    let out = run_keygen(
        key.as_os_str(),
        0o022,
        &[
            ("LD_PRELOAD", shim),
            ("WRAPFILE_SHIM_LIE_MODE", Path::new(lie)),
        ],
    );

    assert_keygen_failed(&out);
    assert!(
        stderr(&out).contains("permission"),
        "stderr must explain the permission problem, got: {}",
        stderr(&out)
    );
    assert!(
        key.symlink_metadata().is_err(),
        "the file created by the failed run must be removed"
    );
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        0,
        "no leftover files may remain"
    );
}

// ---------------------------------------------------------------------------
// Pre-existing targets are never touched
// ---------------------------------------------------------------------------

#[test]
fn existing_regular_file_is_not_overwritten_or_repermissioned() {
    let dir = TestDir::new("exists-file");
    let key = dir.path().join("backup.key");
    fs::write(&key, b"pre-existing content").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();

    // Even a permissive umask must not become a reason to alter the file.
    let out = run_keygen(key.as_os_str(), 0o000, &[]);
    assert_keygen_failed(&out);

    assert_eq!(fs::read(&key).unwrap(), b"pre-existing content");
    assert_eq!(fs::metadata(&key).unwrap().mode() & 0o7777, 0o644);
}

#[test]
fn existing_directory_is_not_replaced() {
    let dir = TestDir::new("exists-dir");
    let key = dir.path().join("backup.key");
    fs::create_dir(&key).unwrap();

    let out = run_keygen(key.as_os_str(), 0o022, &[]);
    assert_keygen_failed(&out);
    assert!(fs::metadata(&key).unwrap().is_dir());
}

#[test]
fn existing_dangling_symlink_is_not_followed_or_removed() {
    let dir = TestDir::new("exists-symlink");
    let target = dir.path().join("nowhere");
    let key = dir.path().join("backup.key");
    std::os::unix::fs::symlink(&target, &key).unwrap();

    let out = run_keygen(key.as_os_str(), 0o022, &[]);
    assert_keygen_failed(&out);

    // The symlink itself survives, still dangling; nothing was created at
    // its target.
    let md = fs::symlink_metadata(&key).unwrap();
    assert!(md.file_type().is_symlink());
    assert!(!target.exists());
}

// ---------------------------------------------------------------------------
// Other failure paths leave no partial state
// ---------------------------------------------------------------------------

#[test]
fn missing_parent_directory_fails_without_creating_anything() {
    let dir = TestDir::new("no-parent");
    let key = dir.path().join("missing").join("backup.key");

    let out = run_keygen(key.as_os_str(), 0o022, &[]);
    assert_keygen_failed(&out);
    assert!(
        !dir.path().join("missing").exists(),
        "keygen must not create parent directories"
    );
}

#[test]
fn unwritable_parent_directory_fails_without_leftovers() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: root can write to any directory");
        return;
    }
    let dir = TestDir::new("ro-parent");
    let parent = dir.path().join("locked");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
    let key = parent.join("backup.key");

    let out = run_keygen(key.as_os_str(), 0o022, &[]);
    assert_keygen_failed(&out);
    assert!(!key.exists());
    assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);

    // Restore writability so the cleanup in Drop can remove the tree.
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
}

// ---------------------------------------------------------------------------
// Existing CLI behaviour is preserved
// ---------------------------------------------------------------------------

#[test]
fn version_output_is_unchanged() {
    let out = run_wrapfile(&[OsStr::new("--version")]);
    assert_eq!(exit_code(&out), 0);
    assert_eq!(stdout(&out), "wrapfile 0.1.0\n");
    assert_eq!(stderr(&out), "");
}

#[test]
fn usage_errors_exit_2_and_create_nothing() {
    let dir = TestDir::new("usage");
    let key = dir.path().join("backup.key");
    let cases: Vec<Vec<&OsStr>> = vec![
        vec![],                                          // no arguments
        vec![OsStr::new("bogus")],                       // unknown subcommand
        vec![OsStr::new("keygen")],                      // missing path
        vec![OsStr::new("keygen"), OsStr::new("--weird")], // unknown option
        vec![                                            // too many arguments
            OsStr::new("keygen"),
            key.as_os_str(),
            OsStr::new("extra"),
        ],
    ];
    for args in cases {
        let out = run_wrapfile(&args);
        assert_eq!(exit_code(&out), 2, "args {args:?} must exit 2");
        assert!(
            stderr(&out).contains("Usage:"),
            "args {args:?} must print usage on stderr, got: {}",
            stderr(&out)
        );
        assert!(!key.exists(), "args {args:?} must not create a file");
    }
}

#[test]
fn double_dash_allows_dash_prefixed_path() {
    let dir = TestDir::new("dash-path");
    let out = Command::new(BIN)
        .args(["keygen", "--", "-secret.key"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(exit_code(&out), 0, "{out:?}");
    assert_valid_key_file(&dir.path().join("-secret.key"));
}
