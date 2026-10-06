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
            // render_path already adds the surrounding double quotes when
            // the option text needs escaping, so no extra quotes go here.
            eprintln!("wrapfile keygen: unknown option {}", render_path(p));
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
            println!("Key saved to {}", render_path(path));
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

// Render a path for messages only.
//
// A plain name -- valid UTF-8 with no ASCII control byte, no double quote,
// and no backslash -- is shown verbatim, exactly as before. Anything else is
// wrapped in double quotes and rendered with escapes:
//
//   \n \r \t   newline, carriage return, tab (so a name can never break a
//              message across lines or smuggle terminal control bytes in)
//   \"  \\     double quote and backslash
//   \xhh       every other ASCII control byte, and every raw byte that is
//              not part of valid UTF-8, as two lowercase hex digits
//
// Decodable non-ASCII text (e.g. Chinese characters) is kept as-is inside
// the quotes. The result therefore never contains a raw ASCII control byte
// (the line terminator printed after the message is the only one), and two
// different raw names always render differently: a real 0xff byte shows as
// `\xff` while a name literally containing those four characters shows as
// `\\xff`, and a real newline shows as `\n` while a backslash followed by
// `n` shows as `\\n`.
//
// This string is purely informational -- it is never used to create, look
// up, or remove anything. The original OsStr is used for every filesystem
// operation, so non-UTF-8 names are still operated on byte for byte.
#[cfg(unix)]
fn render_path(path: &OsStr) -> String {
    const QUOTE: u8 = b'"';
    const BACKSLASH: u8 = b'\\';

    let bytes = path.as_encoded_bytes();

    // Valid UTF-8 with nothing that needs an escape: keep the historical
    // bare display (and the bare "Key saved to ..." wording).
    if std::str::from_utf8(bytes).is_ok()
        && bytes
            .iter()
            .all(|&b| b >= 0x20 && b != QUOTE && b != BACKSLASH && b != 0x7f)
    {
        return String::from_utf8(bytes.to_vec()).unwrap();
    }

    let mut out = String::with_capacity(bytes.len() + 2);
    out.push('"');

    // Walk the raw bytes, stepping over whole UTF-8 sequences so a valid
    // multi-byte character is shown intact while an undecodable byte is
    // escaped on its own.
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];

        if b < 0x80 {
            match b {
                b'\n' => out.push_str("\\n"),
                b'\r' => out.push_str("\\r"),
                b'\t' => out.push_str("\\t"),
                QUOTE => out.push_str("\\\""),
                BACKSLASH => out.push_str("\\\\"),
                // Printable ASCII (0x20..=0x7e except the two handled above)
                // is shown as itself.
                0x20..=0x7e => out.push(b as char),
                // Every remaining ASCII control byte (including NUL and
                // DEL) is rendered explicitly.
                _ => out.push_str(&format!("\\x{b:02x}")),
            }
            i += 1;
            continue;
        }

        // Length of the UTF-8 sequence starting here.
        let seq_len = if b & 0xe0 == 0xc0 {
            2
        } else if b & 0xf0 == 0xe0 {
            3
        } else if b & 0xf8 == 0xf0 {
            4
        } else {
            // 0x80..0xbf continuation byte, or 0xf8..0xff lead: never legal
            // here.
            1
        };

        if seq_len > 1
            && i + seq_len <= bytes.len()
            && std::str::from_utf8(&bytes[i..i + seq_len]).is_ok()
        {
            // from_utf8 on a maximal, well-formed sequence succeeds; push the
            // character as text so e.g. Chinese names keep their glyphs.
            out.push_str(std::str::from_utf8(&bytes[i..i + seq_len]).unwrap());
            i += seq_len;
        } else {
            out.push_str(&format!("\\x{b:02x}"));
            i += 1;
        }
    }

    out.push('"');
    out
}

// On non-Unix targets keygen never reaches the filesystem; keep the message
// rendering lossy-but-inert there (control bytes are still escaped) rather
// than duplicating the byte-level renderer.
#[cfg(not(unix))]
fn render_path(path: &OsStr) -> String {
    use std::fmt::Write as _;
    let text = path.to_string_lossy();
    if text
        .chars()
        .all(|c| c as u32 >= 0x20 && c != '"' && c != '\\' && c as u32 != 0x7f)
    {
        return text.into_owned();
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(all(test, unix))]
mod path_render_tests {
    use super::render_path;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    fn r(raw: &[u8]) -> String {
        render_path(OsStr::from_bytes(raw))
    }

    #[test]
    fn plain_ascii_and_ordinary_utf8_stay_bare() {
        assert_eq!(r(b"path/to/backup.key"), "path/to/backup.key");
        assert_eq!(r(b"-weird-name"), "-weird-name");
        // Decodable non-ASCII text is shown as itself, unquoted.
        assert_eq!(r("密钥/备份.key".as_bytes()), "密钥/备份.key");
    }

    #[test]
    fn named_escapes_render_as_literal_two_char_sequences() {
        assert_eq!(r(b"a\nb"), r#""a\nb""#);
        assert_eq!(r(b"a\rb"), r#""a\rb""#);
        assert_eq!(r(b"a\tb"), r#""a\tb""#);
    }

    #[test]
    fn quote_and_backslash_are_escaped_inside_quotes() {
        assert_eq!(r(b"a\"b"), r#""a\"b""#);
        assert_eq!(r(b"a\\b"), r#""a\\b""#);
    }

    #[test]
    fn other_control_bytes_use_hex_and_split_no_lines() {
        assert_eq!(r(b"a\x1bb"), r#""a\x1bb""#); // ESC
        assert_eq!(r(b"\x00"), r#""\x00""#);
        assert_eq!(r(b"\x7f"), r#""\x7f""#);
        let rendered = r(b"a\x07\x1bc");
        assert!(
            !rendered.bytes().any(|b| b < 0x20 || b == 0x7f),
            "rendering must not retain raw control bytes: {rendered:?}"
        );
    }

    #[test]
    fn invalid_utf8_bytes_use_hex_but_valid_text_around_remains() {
        assert_eq!(r(b"key-\xff"), r#""key-\xff""#);
        assert_eq!(r(b"key-\xff raw \xfe.bin"), r#""key-\xff raw \xfe.bin""#);
        assert_eq!(r(b"-\xffkey"), r#""-\xffkey""#);
        // A valid multibyte run next to a broken byte keeps its glyphs.
        let mut mixed = "密钥".as_bytes().to_vec();
        mixed.push(0xff);
        assert_eq!(r(&mixed), r#""密钥\xff""#);
        // Truncated multibyte lead/continuation bytes are each escaped.
        assert_eq!(r(b"\xe4\xbd"), r#""\xe4\xbd""#);
        assert_eq!(r(b"a\x80b"), r#""a\x80b""#);
    }

    #[test]
    fn real_illegal_byte_differs_from_literal_backslash_text() {
        // A real 0xff byte ...
        assert_eq!(r(b"\xff"), r#""\xff""#);
        // ... is not the same visible target as a name containing the four
        // literal characters \, x, f, f.
        assert_eq!(r(b"\\xff"), r#""\\xff""#);
        assert_ne!(r(b"\xff"), r(b"\\xff"));

        // Real newline vs the two characters backslash-n.
        assert_eq!(r(b"a\nb"), r#""a\nb""#);
        assert_eq!(r(b"a\\nb"), r#""a\\nb""#);
        assert_ne!(r(b"a\nb"), r(b"a\\nb"));
    }

    #[test]
    fn distinct_illegal_bytes_do_not_share_a_rendering() {
        assert_eq!(r(b"key-\xff"), r#""key-\xff""#);
        assert_eq!(r(b"key-\xfe"), r#""key-\xfe""#);
        assert_ne!(r(b"key-\xff"), r(b"key-\xfe"));
    }

    #[test]
    fn rendering_contains_no_raw_control_bytes_except_as_appended_newline() {
        for name in [
            &b"a\nb\x1bc\r\t"[..],
            &b"\x00\x01\x02\x1b\x7f"[..],
            &b"ok\xff\n"[..],
        ] {
            let rendered = r(name);
            assert!(
                !rendered.bytes().any(|b| b < 0x20 || b == 0x7f),
                "raw control byte survived in {rendered:?} for {name:?}"
            );
        }
    }
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

    // This stack array is the only buffer this command directly holds that
    // contains the generated key material: getrandom draws straight into it
    // (the crate forwards the buffer to the getrandom(2) syscall without
    // copying it) and the save code only ever borrows it. Whatever happens
    // below -- success, a random-source failure after zero, some, or all 32
    // bytes, or a failure creating/permissioning/writing/closing the file --
    // the array is wiped before this operation returns, and the wipe must
    // survive an optimizing compiler. It therefore lives here in this
    // wrapper rather than at the individual return sites, so no error branch
    // inside the save logic can bypass it.
    let mut key = [0u8; KEY_LEN];
    let outcome = save_key_file(&c_path, path, &mut key);
    secure_zero(&mut key);
    outcome
}

#[cfg(unix)]
fn save_key_file(
    c_path: &CString,
    path: &OsStr,
    key: &mut [u8; KEY_LEN],
) -> std::io::Result<()> {
    // Draw the key from the OS CSPRNG first. If the random source is
    // unavailable nothing is ever created at the target path. Bytes already
    // delivered on a mid-draw failure are secret material even though no
    // complete key exists: they are wiped by the caller on the way out, and
    // they are neither zero-padded into a stand-in key nor reported.
    if let Err(e) = getrandom::fill(key) {
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
        return Err(std::io::Error::new(
            e.kind(),
            format!("cannot create key file {}: {e}", render_path(path)),
        ));
    }
    let fd = fd as RawFd;

    // Enforce and confirm exactly 0600 before writing, then write and fsync.
    // On any failure the inode we just created is unlinked by abort_save; an
    // object predating this invocation is never touched. The key buffer is
    // wiped by generate_key_file on the way out of every arm below --
    // including the close-failure arm -- so it is not zeroed here.
    let outcome = ensure_owner_only_mode(fd).and_then(|()| write_key(fd, key));

    match outcome {
        // The descriptor is still open: abort_save unlinks while it is (only
        // the link to the inode we just created is removed) and closes it
        // afterwards.
        Err(stage) => Err(abort_save(&c_path, path, Some(fd), stage)),
        Ok(()) => {
            // All 32 bytes were written and fsynced, but the save is only
            // finished once the close succeeds.
            if unsafe { libc::close(fd) } == 0 {
                return Ok(());
            }
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                // close() released the descriptor despite EINTR.
                return Ok(());
            }
            // An unrecoverable close error: even if the file happens to hold
            // 32 bytes with mode 0600, the save did not complete normally, so
            // this invocation's file must not stay behind to be mistaken for
            // a successfully saved key. On Linux the descriptor is released
            // even when close() reports an error, so close is not retried;
            // the file is removed exactly as for a write or fsync failure.
            let stage = std::io::Error::new(
                e.kind(),
                format!("closing the key file failed: {e}"),
            );
            Err(abort_save(&c_path, path, None, stage))
        }
    }
}

/// Overwrite a secret-holding buffer with zeroes in a way the compiler must
/// keep.
///
/// A plain `buf.fill(0)` at the end of a key's lifetime is not enough: once
/// the buffer is never read again, an optimizing compiler is free to delete
/// those stores as dead code, so under an optimized build the secret bytes
/// would stay on the stack. Every byte here is written through
/// `write_volatile`, which the optimizer must emit regardless of later
/// reads -- the same guarantee libc's `explicit_bzero(3)` gives -- so the
/// erasure does not depend on the optimization level. It is implemented
/// directly rather than calling a libc symbol so the protection also holds
/// on Unix targets whose libc does not provide `explicit_bzero`.
///
/// Call this for every buffer that directly holds (any prefix of) the
/// generated key, on both success and every error path.
#[cfg(unix)]
fn secure_zero(buf: &mut [u8]) {
    let base = buf.as_mut_ptr();
    for offset in 0..buf.len() {
        // SAFETY: `offset` is in bounds of `buf`, a live allocation
        // exclusively borrowed for this whole call; `u8` imposes no
        // initialization invariant and has no drop glue.
        unsafe {
            std::ptr::write_volatile(base.add(offset), 0u8);
        }
    }
}

/// Abandon a save that could not be completed: remove the file this
/// invocation created at the target and build the error to report.
///
/// `stage` is the original failure (permission setup or confirmation, write,
/// sync, or close); its kind and message are kept as the reported error, so a
/// cleanup problem can never mask why the save failed. If `fd` is `Some`, the
/// descriptor is still open: the unlink runs first and its result is captured
/// before the close, which may clobber errno (on Linux close releases the
/// descriptor even when it reports an error, so it is never retried).
///
/// The unlink only ever targets the link this invocation created (the caller
/// opened it O_CREAT | O_EXCL), never an object that predates this run.
///
/// * Removal succeeds: nothing of this run remains at the target -- no empty
///   file, no partial key, no unsynced 32-byte file.
/// * The system refuses the removal: whatever the file holds (an empty file,
///   a partial key, or 32 bytes that were never synced), it is not a saved
///   key. The report keeps the original failure, adds the cleanup failure
///   and its reason, and says plainly that this run's file may still sit at
///   the target so the user can check and remove it. It never claims the
///   file is gone, and the refusal is never worked around -- the parent
///   directory's permissions are not loosened and the key is not stashed
///   under another name.
#[cfg(unix)]
fn abort_save(
    c_path: &CString,
    path: &OsStr,
    fd: Option<RawFd>,
    stage: std::io::Error,
) -> std::io::Error {
    let unlink_err = if unsafe { libc::unlink(c_path.as_ptr()) } == 0 {
        None
    } else {
        Some(std::io::Error::last_os_error())
    };
    if let Some(fd) = fd {
        unsafe { libc::close(fd) };
    }
    let kind = stage.kind();
    match unlink_err {
        None => std::io::Error::new(
            kind,
            format!(
                "key file {} was not saved: {stage}; the file created by \
                 this invocation has been removed",
                render_path(path)
            ),
        ),
        Some(ue) => std::io::Error::new(
            kind,
            format!(
                "key file {} was not saved: {stage}; cleanup of the \
                 incomplete file also failed ({ue}): a file created by this \
                 failed run may still be present at the target -- check and \
                 remove it yourself",
                render_path(path)
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
