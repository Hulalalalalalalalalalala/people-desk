use std::env;
use std::ffi::{OsStr, OsString};
use std::process::ExitCode;

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::io::RawFd;

const VERSION_STRING: &str = "wrapfile 0.1.0";
const KEY_LEN: usize = 32;

fn main() -> ExitCode {
    // args_os (not args): on Unix a key file path may contain bytes that are
    // not valid UTF-8, and those are legitimate on-disk names we must accept
    // verbatim rather than panic on.
    let args: Vec<OsString> = env::args_os().skip(1).collect();

    match args.first().map(OsString::as_os_str) {
        Some(a) if a == "--version" && args.len() == 1 => {
            println!("{VERSION_STRING}");
            ExitCode::SUCCESS
        }
        Some(a) if a == "keygen" => run_keygen(&args[1..]),
        _ => {
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn print_usage() {
    eprintln!("Usage: wrapfile --version");
    eprintln!("       wrapfile keygen <KEY_FILE>");
}

fn run_keygen(args: &[OsString]) -> ExitCode {
    // A leading "--" marks the end of options, allowing a path that starts
    // with '-' to be passed (e.g. `wrapfile keygen -- -weird-name`).
    let (args, options_ended): (&[OsString], bool) = match args {
        [first, rest @ ..] if first == "--" => (rest, true),
        _ => (args, false),
    };
    let path: &OsStr = match args {
        [] => {
            eprintln!("wrapfile keygen: missing key file path");
            print_usage();
            return ExitCode::from(2);
        }
        [p] if options_ended || !starts_with_dash(p) => p,
        [p] => {
            eprintln!(
                "wrapfile keygen: unknown option '{}'",
                display_path(p)
            );
            print_usage();
            return ExitCode::from(2);
        }
        _ => {
            eprintln!("wrapfile keygen: expected exactly one argument (key file path)");
            print_usage();
            return ExitCode::from(2);
        }
    };

    match generate_key_file(path) {
        Ok(()) => {
            println!("Key saved to {}", display_path(path));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wrapfile keygen: {e}");
            ExitCode::from(1)
        }
    }
}

// True if the argument begins with '-'. Checked on the raw bytes so a path
// with non-UTF-8 bytes is classified the same as any other name.
fn starts_with_dash(arg: &OsStr) -> bool {
    arg.as_encoded_bytes().first() == Some(&b'-')
}

// Human-readable rendering of a path for messages only. Lossy conversion may
// replace non-UTF-8 bytes for display, but this string is never used to
// touch the filesystem -- the original OsStr is always used for that.
fn display_path(path: &OsStr) -> std::path::Display<'_> {
    std::path::Path::new(path).display()
}

#[cfg(unix)]
fn generate_key_file(path: &OsStr) -> std::io::Result<()> {
    // Use the raw OS bytes so a path containing non-UTF-8 bytes refers to
    // exactly the name the user passed, with no lossy substitution.
    let c_path = CString::new(path.as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target path contains a NUL byte",
        )
    })?;

    // Draw the key from the OS CSPRNG first. If the random source is
    // unavailable nothing is ever created at the target path.
    let mut key = [0u8; KEY_LEN];
    if let Err(e) = getrandom::fill(&mut key) {
        key.fill(0);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("cryptographic random source unavailable: {e}"),
        ));
    }

    // O_CREAT | O_EXCL: fail if anything already exists at the path (file,
    // directory, or symlink -- including a dangling one, which O_NOFOLLOW
    // also rejects), so a pre-existing object is never overwritten or
    // followed. mode 0600 contains no group/other bits, and the umask can
    // only strip bits, never add them: group/other therefore never gain
    // access, not even for an instant. A strict umask (e.g. 0777) can also
    // strip the owner bits, leaving 0000; that is repaired with fchmod and
    // verified before any key byte is written (see ensure_owner_only_mode).
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_WRONLY
                | libc::O_CREAT
                | libc::O_EXCL
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        key.fill(0);
        return Err(std::io::Error::new(
            e.kind(),
            format!("cannot create key file '{}': {e}", display_path(path)),
        ));
    }
    let fd = fd as RawFd;

    // Enforce and confirm exactly 0600 before writing, then write and fsync.
    // Once this invocation's inode exists, every later failure follows the
    // single cleanup convention in abort_created_file: unlink only the link
    // to the inode we created, keep the original failure, and report any
    // refused removal on top of it. An object predating this invocation is
    // never touched. Failures before the open (the random draw, the open
    // itself) returned above without entering that cleanup, because no file
    // from this run exists yet.
    if let Err(cause) = ensure_owner_only_mode(fd).and_then(|()| write_key(fd, &key)) {
        key.fill(0);
        // The fd is still ours: unlink first, then close.
        return Err(abort_created_file(Some(fd), &c_path, path, cause));
    }
    key.fill(0);

    // All 32 bytes were written and fsynced, but the save is only finished
    // once the close succeeds. On Linux a close error other than EINTR has
    // still released the descriptor, so the fd is not ours to close again.
    match close_key_file(fd) {
        Ok(()) => Ok(()),
        Err(cause) => Err(abort_created_file(None, &c_path, path, cause)),
    }
}

// The final save step. close() reporting EINTR after releasing the
// descriptor is an interruption, not an unrecoverable failure: the save is
// complete and is reported as usual. Any other close error is labelled with
// its stage so the caller's cleanup report can tell it apart from a
// permission, write, or sync failure; on Linux the descriptor is released
// even when close() reports an error, so close is never retried here.
#[cfg(unix)]
fn close_key_file(fd: RawFd) -> std::io::Result<()> {
    if unsafe { libc::close(fd) } == 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EINTR) {
        return Ok(());
    }
    Err(std::io::Error::new(
        e.kind(),
        format!("closing the key file failed: {e}"),
    ))
}

// The one cleanup-and-report convention for every failure that happens once
// this invocation has created its target file -- an unconfirmable 0600 mode,
// an unfinished write, a failed sync, or an unrecoverable close error. It
// removes only the link to the inode this run created (an object that
// predates the invocation is never touched, and the unlink deliberately
// happens while our fd is still open whenever one remains), then closes any
// fd we still own. The original failure is always preserved: the cleanup
// result is reported alongside it, never in its place.
//
// `open_fd` is Some while the descriptor is still ours (permission/write/
// sync failures) and None when the close stage has already released it.
#[cfg(unix)]
fn abort_created_file(
    open_fd: Option<RawFd>,
    c_path: &CString,
    path: &OsStr,
    cause: std::io::Error,
) -> std::io::Error {
    // Capture the unlink result before close, which may clobber errno.
    let unlink_error = if unsafe { libc::unlink(c_path.as_ptr()) } == 0 {
        None
    } else {
        Some(std::io::Error::last_os_error())
    };
    if let Some(fd) = open_fd {
        // On Linux the fd is closed regardless of the returned error, so a
        // close here must not be retried; its result cannot change the
        // outcome already being reported.
        unsafe { libc::close(fd) };
    }
    failed_save_error(path, cause, unlink_error)
}

// Build the user-facing save-failure error from the original stage failure
// and the cleanup result. The stage failure always leads, so a cleanup error
// can never overwrite the first reason the save failed. When the system
// refused to remove the file, the message says plainly that a file created
// by this run may still sit at the target -- an empty file, a partial key,
// or 32 bytes that were not saved -- for the user to check and remove. It
// never claims the residue is gone or is a usable key, and it never works
// around the refusal (no loosening the parent directory, no alternate name).
#[cfg(unix)]
fn failed_save_error(
    path: &OsStr,
    cause: std::io::Error,
    unlink_error: Option<std::io::Error>,
) -> std::io::Error {
    let target = display_path(path);
    match unlink_error {
        None => std::io::Error::new(
            cause.kind(),
            format!(
                "key file '{target}' was not saved: {cause}; the file created \
                 by this invocation has been removed"
            ),
        ),
        Some(ue) => std::io::Error::new(
            cause.kind(),
            format!(
                "key file '{target}' was not saved: {cause}; cleanup of the \
                 incomplete file also failed ({ue}): a file created by this \
                 failed run may still be present at the target -- check and \
                 remove it yourself"
            ),
        ),
    }
}

#[cfg(unix)]
fn ensure_owner_only_mode(fd: RawFd) -> std::io::Result<()> {
    // The mode passed to open() is masked by the process umask, so a strict
    // umask (e.g. 0777) can strip the owner read/write bits, and some
    // filesystems may adjust or refuse the requested mode. fchmod on the
    // open fd is not affected by the umask: force exactly 0600 and then
    // fstat to confirm the filesystem actually stores those bits. This
    // runs before any key byte is written, so a file that cannot meet the
    // requirement never receives secret data.
    if unsafe { libc::fchmod(fd, 0o600) } != 0 {
        let e = std::io::Error::last_os_error();
        return Err(std::io::Error::new(
            e.kind(),
            format!(
                "cannot set key file permissions to 0600: {e} \
                 (filesystem does not support the required permissions)"
            ),
        ));
    }

    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        // Without a fresh stat result the actual on-disk mode cannot be
        // confirmed, so the requirement cannot be met -- say so explicitly
        // rather than surface a bare errno string.
        let e = std::io::Error::last_os_error();
        return Err(std::io::Error::new(
            e.kind(),
            format!("cannot verify key file permissions are 0600: {e}"),
        ));
    }

    // Exactly owner read/write, no group or other permissions of any kind.
    // This rejects 0400/0200/0000 (owner bits missing under a strict umask)
    // as well as any mode carrying group/other bits.
    if st.st_mode & 0o7777 != 0o600 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "cannot guarantee key file permissions 0600: \
                 filesystem reports mode 0{:04o}",
                st.st_mode & 0o7777
            ),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn write_key(fd: RawFd, key: &[u8; KEY_LEN]) -> std::io::Result<()> {
    // Caller has already run ensure_owner_only_mode: the inode carries
    // exactly 0600 before the first secret byte reaches it. Failures are
    // labelled with the stage so the user can tell an unfinished write
    // apart from a permission or sync problem.
    write_all(fd, key).map_err(|e| {
        std::io::Error::new(e.kind(), format!("could not write key file: {e}"))
    })?;

    if unsafe { libc::fsync(fd) } != 0 {
        let e = std::io::Error::last_os_error();
        return Err(std::io::Error::new(
            e.kind(),
            format!("could not flush key file: {e}"),
        ));
    }

    Ok(())
}

#[cfg(unix)]
fn write_all(fd: RawFd, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        let n = unsafe {
            libc::write(fd, data.as_ptr().cast::<libc::c_void>(), data.len())
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "incomplete write to key file",
            ));
        }
        data = &data[n as usize..];
    }
    Ok(())
}

#[cfg(not(unix))]
fn generate_key_file(_path: &OsStr) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "keygen is only supported on Unix systems",
    ))
}
