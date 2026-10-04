use std::env;
use std::process::ExitCode;

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::unix::io::RawFd;

const VERSION_STRING: &str = "wrapfile 0.1.0";
const KEY_LEN: usize = 32;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("--version") if args.len() == 1 => {
            println!("{VERSION_STRING}");
            ExitCode::SUCCESS
        }
        Some("keygen") => run_keygen(&args[1..]),
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

fn run_keygen(args: &[String]) -> ExitCode {
    // A leading "--" marks the end of options, allowing a path that starts
    // with '-' to be passed (e.g. `wrapfile keygen -- -weird-name`).
    let (args, options_ended): (&[String], bool) = match args {
        [first, rest @ ..] if first == "--" => (rest, true),
        _ => (args, false),
    };
    let path = match args {
        [] => {
            eprintln!("wrapfile keygen: missing key file path");
            print_usage();
            return ExitCode::from(2);
        }
        [p] if options_ended || !p.starts_with('-') => p,
        [p] => {
            eprintln!("wrapfile keygen: unknown option '{p}'");
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
            println!("Key saved to {path}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wrapfile keygen: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(unix)]
fn generate_key_file(path: &str) -> std::io::Result<()> {
    let c_path = CString::new(path).map_err(|_| {
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
    // followed. Requesting mode 0600 means group/other never gain access at
    // creation: umask can only strip bits, never add them. A strict umask
    // (e.g. 0777) can also strip the owner bits; write_key() restores the
    // full mode with fchmod before any key byte is written.
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
            format!("cannot create key file '{path}': {e}"),
        ));
    }
    let fd = fd as RawFd;

    let result = write_key(fd, &key);
    key.fill(0);

    match result {
        Err(e) => {
            // Unlink while our fd is still open: only the link to the inode
            // we just created is removed, never an object that predates this
            // invocation. Then close (on Linux the fd is closed regardless
            // of the returned error, so it must not be retried).
            unsafe {
                libc::unlink(c_path.as_ptr());
                libc::close(fd);
            }
            Err(std::io::Error::new(
                e.kind(),
                format!("key file '{path}': {e}"),
            ))
        }
        Ok(()) => {
            // All 32 bytes were written and fsynced, so the file is complete.
            if unsafe { libc::close(fd) } != 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    // close() released the descriptor despite EINTR.
                    return Ok(());
                }
                return Err(std::io::Error::new(
                    e.kind(),
                    format!("key file '{path}' written but close failed: {e}"),
                ));
            }
            Ok(())
        }
    }
}

#[cfg(unix)]
fn write_key(fd: RawFd, key: &[u8; KEY_LEN]) -> std::io::Result<()> {
    // Ensure exactly 0600 (owner read/write, nothing for group/other)
    // before any secret byte is written.
    //
    // The umask applied at open() may also have stripped the owner bits
    // (e.g. umask 0777 yields mode 0000), and some filesystems fail to
    // record the requested mode. fchmod on our fresh fd restores the full
    // mode (it is not affected by umask and can never grant group/other
    // access), then fstat confirms the on-disk mode is exactly 0600 --
    // fchmod can appear to succeed on filesystems that silently ignore it
    // (e.g. certain FAT mounts), so the result must be verified. If either
    // step fails, no key byte is ever written and the caller removes the
    // empty file.
    if unsafe { libc::fchmod(fd, 0o600) } != 0 {
        let e = std::io::Error::last_os_error();
        return Err(std::io::Error::new(
            e.kind(),
            format!("cannot set key file permissions to 0600: {e}"),
        ));
    }

    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if st.st_mode & 0o7777 != 0o600 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "key file permissions are {:04o}, cannot guarantee owner-only (0600) permissions",
                st.st_mode & 0o7777
            ),
        ));
    }

    write_all(fd, key)?;

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
fn generate_key_file(_path: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "keygen is only supported on Unix systems",
    ))
}
