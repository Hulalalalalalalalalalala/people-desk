//! On-demand compilation of the fault-injection shims used by the keygen
//! regression tests.
//!
//! The shims (tests/support/permfail, tests/support/randtrap) are test-only
//! artifacts: key generation itself never loads them. They are therefore NOT
//! part of the product build (there is no build.rs compiling them, so an
//! ordinary `cargo build` succeeds even on a machine whose C compiler or
//! headers cannot compile them). Instead each test binary compiles the shim
//! it needs on its first use, at test runtime.
//!
//! Skip-vs-fail contract:
//!
//! * Platforms the shim does not support (anything but Linux, and for
//!   randtrap anything but x86_64/aarch64), or a machine with no C compiler
//!   at all, return `None` after printing an explicit reason; the caller
//!   skips only the shim-backed cases, while shim-free tests still run.
//! * A C compiler that is found but rejects the shim source is a real
//!   environment/regression failure: the helper panics, naming the compile
//!   stage and the compiler's diagnostics, instead of pretending the
//!   platform is unsupported.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Absolute path to the shim source `rel` under tests/support.
fn support_src(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("support")
        .join(rel)
}

/// Shared cache directory for the compiled shims. The several integration
/// test binaries run as separate processes (and cargo runs them in parallel),
/// so each shim is stored under a content-derived name (see
/// [`shim_path`]): identical source maps to the same path, changed source
/// gets a fresh name, and the file is published with an atomic rename, so
/// concurrent test binaries never observe or corrupt a half-written .so.
fn cache_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("wrapfile-test-shims");
    std::fs::create_dir_all(&dir).expect("create shim cache directory");
    dir
}

/// Content-addressed path for a shim: the name changes whenever the source
/// (or the crate version) changes, so an edit to the C source can never be
/// masked by a stale artifact.
fn shim_path(src_path: &Path, base_name: &str) -> PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let bytes = std::fs::read(src_path)
        .unwrap_or_else(|e| panic!("read test shim source {}: {e}", src_path.display()));
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    env!("CARGO_PKG_VERSION").hash(&mut hasher);
    let suffix = if cfg!(target_os = "windows") { ".dll" } else { ".so" };
    cache_dir().join(format!("{base_name}-{:016x}{suffix}", hasher.finish()))
}

/// Compile `src` into `so_name` once for this test binary and return its
/// path. Returns `None` (with an explicit skip reason) when the shim cannot
/// be used on this platform or no C compiler is installed; panics when the
/// compiler runs but fails to build the shim.
fn build_shim(src: &str, so_name: &str, arch_gated: bool) -> Option<PathBuf> {
    // The permfail shim relies on glibc's open/fstat interposition
    // semantics; randtrap relies on seccomp SECCOMP_RET_TRAP plus
    // signal-frame register access. These are Linux mechanisms; on other
    // targets the shim-backed tests have always skipped.
    if cfg!(not(target_os = "linux")) {
        eprintln!(
            "skipping: {so_name} is Linux/glibc-only; this target is not Linux"
        );
        return None;
    }
    if arch_gated && cfg!(not(any(target_arch = "x86_64", target_arch = "aarch64"))) {
        eprintln!(
            "skipping: {so_name} supports only x86_64/aarch64 (seccomp trap \
             signal-frame access); this arch is not supported"
        );
        return None;
    }

    let src_path = support_src(src);
    let so_path = shim_path(&src_path, so_name);
    // Another concurrently running test binary may have just compiled the
    // identical shim; reuse it instead of recompiling.
    if so_path.is_file() {
        return Some(so_path);
    }

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());

    // Compile to a unique temporary file in the same directory, then publish
    // with an atomic rename: no reader ever sees a partially linked .so. The
    // PID + thread id make the name unique across cargo's parallel test
    // processes and threads.
    let tid = format!("{:?}", std::thread::current().id())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>();
    let tmp_so = cache_dir().join(format!(".{so_name}.tmp-{}-{tid}", std::process::id()));

    let output = match Command::new(&cc)
        .args(["-O0", "-g", "-fPIC", "-shared", "-Wall", "-Wextra"])
        .arg("-o")
        .arg(&tmp_so)
        .arg(&src_path)
        .output()
    {
        Ok(output) => output,
        // A machine without a C compiler can still build/use wrapfile; skip
        // only the shim-backed tests. A compiler that ran but rejected the
        // source is handled below and must fail loudly.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "skipping: C compiler '{cc}' not found; {so_name} not built, \
                 shim-backed tests skipped (wrapfile itself builds without a C \
                 compiler)"
            );
            return None;
        }
        Err(e) => panic!("test setup: could not start C compiler '{cc}' to compile {src}: {e}"),
    };

    if !output.status.success() {
        let _ = std::fs::remove_file(&tmp_so);
        panic!(
            "test setup: compiling test shim {src} with '{cc}' FAILED \
             (status {}). The compiler was found but rejected the shim \
             source; this is a compile failure, not an unsupported platform.\n\
             --- compiler stdout ---\n{}\n--- compiler stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    if let Err(e) = std::fs::rename(&tmp_so, &so_path) {
        // Rename fails only if another process published the same name first
        // (same content hash) or for a genuine I/O error; reuse when present.
        let _ = std::fs::remove_file(&tmp_so);
        if !so_path.is_file() {
            panic!("test setup: publishing compiled shim {} failed: {e}", so_path.display());
        }
    }

    Some(so_path)
}

/// Path to the permission/write fault-injection shim, compiling it first.
#[allow(dead_code)] // each test binary imports only the shim it needs
pub fn permfail_so() -> Option<PathBuf> {
    static SO: OnceLock<Option<PathBuf>> = OnceLock::new();
    SO.get_or_init(|| build_shim("permfail/permfail.c", "libwrapfile_permfail", false))
        .clone()
}

/// Path to the random-source seccomp shim, compiling it first.
#[allow(dead_code)] // each test binary imports only the shim it needs
pub fn randtrap_so() -> Option<PathBuf> {
    static SO: OnceLock<Option<PathBuf>> = OnceLock::new();
    SO.get_or_init(|| build_shim("randtrap/randtrap.c", "libwrapfile_randtrap", true))
        .clone()
}
