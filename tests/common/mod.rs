//! Shared helpers for the keygen regression tests: build the LD_PRELOAD
//! fault-injection shims (tests/support/permfail, tests/support/randtrap,
//! tests/support/keywipe) on demand, at test time.
//!
//! The shims are compiled here rather than by a Cargo build script so that
//! `cargo build` / `cargo build --release` never invoke a C compiler: a
//! machine that can compile and link wrapfile itself always gets a working
//! binary, regardless of whether the test-only C toolchain is present or
//! functional. The price is that the shim-backed tests must cope with a
//! missing compiler themselves. The policy, matching the old build script:
//!
//! * Platform without shim support (non-Linux for permfail; non-Linux or
//!   non-x86_64/aarch64 for randtrap): return `None` after printing the
//!   reason; the dependent tests skip, everything else still runs.
//! * C compiler not found: same -- skip with the reason printed.
//! * The compiler *ran* but rejected the shim source: this is a real
//!   problem (broken headers, broken source, test-only compiler settings),
//!   never "platform unsupported" -- panic, failing the test run with the
//!   compile stage and the compiler's own diagnostics.
//!
//! Compilation intentionally invokes the system C compiler directly instead
//! of pulling in the `cc` crate, so the offline dependency set
//! (getrandom + libc) is unchanged and `cargo test --offline` keeps working.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Path to the permfail shim, building it on first use. `None` (with the
/// reason on stderr) when this platform or machine cannot provide it.
pub fn permfail_so() -> Option<PathBuf> {
    // The permfail shim relies on glibc's open/open64/fstat/fstat64
    // interposition semantics; build it on Linux only.
    static SO: OnceLock<Option<PathBuf>> = OnceLock::new();
    SO.get_or_init(|| {
        if std::env::consts::OS != "linux" {
            eprintln!(
                "permfail shim unavailable: only supported on Linux/glibc, \
                 this target is {}",
                std::env::consts::OS
            );
            return None;
        }
        build_shim(
            "permfail",
            "libwrapfile_permfail.so",
            "tests/support/permfail/permfail.c",
        )
    })
    .clone()
}

/// Path to the randtrap shim, building it on first use. `None` (with the
/// reason on stderr) when this platform or machine cannot provide it.
pub fn randtrap_so() -> Option<PathBuf> {
    // The randtrap shim relies on seccomp SECCOMP_RET_TRAP plus
    // signal-frame register access (x86_64 and aarch64 only).
    static SO: OnceLock<Option<PathBuf>> = OnceLock::new();
    SO.get_or_init(|| {
        if std::env::consts::OS != "linux" {
            eprintln!(
                "randtrap shim unavailable: only supported on Linux, \
                 this target is {}",
                std::env::consts::OS
            );
            return None;
        }
        if !matches!(std::env::consts::ARCH, "x86_64" | "aarch64") {
            eprintln!(
                "randtrap shim unavailable: only supported on x86_64/aarch64, \
                 this target is {}",
                std::env::consts::ARCH
            );
            return None;
        }
        build_shim(
            "randtrap",
            "libwrapfile_randtrap.so",
            "tests/support/randtrap/randtrap.c",
        )
    })
    .clone()
}

/// Path to the keywipe shim, building it on first use. `None` (with the
/// reason on stderr) when this platform or machine cannot provide it.
pub fn keywipe_so() -> Option<PathBuf> {
    // The keywipe shim relies on seccomp SECCOMP_RET_TRAP plus
    // signal-frame register access and single-stepping (x86_64 and
    // aarch64 only).
    static SO: OnceLock<Option<PathBuf>> = OnceLock::new();
    SO.get_or_init(|| {
        if std::env::consts::OS != "linux" {
            eprintln!(
                "keywipe shim unavailable: only supported on Linux, \
                 this target is {}",
                std::env::consts::OS
            );
            return None;
        }
        if !matches!(std::env::consts::ARCH, "x86_64" | "aarch64") {
            eprintln!(
                "keywipe shim unavailable: only supported on x86_64/aarch64, \
                 this target is {}",
                std::env::consts::ARCH
            );
            return None;
        }
        build_shim(
            "keywipe",
            "libwrapfile_keywipe.so",
            "tests/support/keywipe/keywipe.c",
        )
    })
    .clone()
}

/// Compile one shim with the system C compiler. Returns the built path, or
/// `None` (reason printed) when no C compiler exists on this machine. A
/// compiler that starts but fails to build the shim is a hard error.
fn build_shim(label: &str, so_name: &str, src_rel: &str) -> Option<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join(src_rel);

    // Build into a per-process directory so concurrently running test
    // binaries never write the same .so. Prefer Cargo's own scratch area
    // (cleaned up with `target/`); fall back to the system temp dir.
    let dir = option_env!("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("wrapfile-test-shims-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| {
        panic!("cannot create test shim build directory {}: {e}", dir.display())
    });
    let so = dir.join(so_name);

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let output = Command::new(&cc)
        .args(["-O0", "-g", "-fPIC", "-shared", "-Wall", "-Wextra"])
        .arg("-o")
        .arg(&so)
        .arg(&src)
        .output();

    match output {
        Ok(o) if o.status.success() => Some(so),
        // The compiler started but the shim did not build: fail loudly,
        // naming the compile stage and relaying the compiler's diagnostics.
        // This must never be mistaken for an unsupported platform.
        Ok(o) => panic!(
            "test shim '{label}' failed to compile: `{cc}` exited with {} \
             while building {src_rel}\ncompiler stderr:\n{}",
            o.status,
            String::from_utf8_lossy(&o.stderr),
        ),
        // A machine without a C compiler can still build and use wrapfile
        // itself; the shim-backed tests skip with the reason stated.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "{label} shim not built: C compiler '{cc}' not found; \
                 install a C compiler to run the shim-backed tests"
            );
            None
        }
        Err(e) => panic!("failed to run C compiler '{cc}' while building test shim '{label}': {e}"),
    }
}
