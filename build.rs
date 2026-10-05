// Build script: compile the tiny LD_PRELOAD shims used by the keygen
// regression tests (Unix only). Intentionally invokes the system C compiler
// directly instead of pulling in the `cc` crate, so the offline dependency
// set (getrandom + libc) is unchanged.
use std::path::Path;

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    // The permfail shim relies on glibc's open/open64/fstat/fstat64
    // interposition semantics; the randtrap shim relies on seccomp
    // SECCOMP_RET_TRAP plus signal-frame register access (x86_64 and
    // aarch64 only). Build them on Linux. On other targets the shim-backed
    // tests detect the missing env var and skip themselves, while the
    // shim-free tests still run.
    if target_os != "linux" {
        println!("cargo:warning=test shims not built for target OS {target_os}; shim-backed tests will skip");
        return;
    }

    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());

    build_shim(
        &cc,
        "tests/support/permfail/permfail.c",
        &out_dir.join("libwrapfile_permfail.so"),
        "WRAPFILE_PERMFAIL_SO",
    );

    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if matches!(target_arch.as_str(), "x86_64" | "aarch64") {
        build_shim(
            &cc,
            "tests/support/randtrap/randtrap.c",
            &out_dir.join("libwrapfile_randtrap.so"),
            "WRAPFILE_RANDTRAP_SO",
        );
    } else {
        println!("cargo:warning=randtrap shim not built for target arch {target_arch}; random-source tests will skip");
    }
}

fn build_shim(cc: &str, src: &str, so_path: &Path, env_var: &str) {
    let status = std::process::Command::new(cc)
        .args(["-O0", "-g", "-fPIC", "-shared", "-Wall", "-Wextra"])
        .arg("-o")
        .arg(so_path)
        .arg(src)
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => panic!("{src} compile failed with status {s}"),
        // A machine without a C compiler can still build/use wrapfile
        // itself; skip exporting the shim path so the shim-backed tests
        // print a skip notice. A compiler that ran but rejected the source
        // is a real error and must fail loudly (the Ok arm above).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("cargo:warning=C compiler '{cc}' not found; {src} not built, dependent tests will skip");
            return;
        }
        Err(e) => panic!("failed to run C compiler while building {src}: {e}"),
    }

    println!("cargo:rustc-env={env_var}={}", so_path.display());
    println!("cargo:rerun-if-changed={src}");
}
