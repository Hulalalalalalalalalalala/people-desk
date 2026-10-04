// Build script: compile the tiny LD_PRELOAD shim used by the keygen
// permission tests (Unix only). Intentionally invokes the system C compiler
// directly instead of pulling in the `cc` crate, so the offline dependency
// set (getrandom + libc) is unchanged.
fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    // The shim relies on glibc's open/open64/fstat/fstat64 interposition
    // semantics; build it on Linux. On other Unix targets the shim-backed
    // tests detect the missing env var and skip themselves, while the
    // shim-free tests still run.
    if target_os != "linux" {
        println!("cargo:warning=permfail shim not built for target OS {target_os}; shim-backed tests will skip");
        return;
    }

    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let so_path = out_dir.join("libwrapfile_permfail.so");
    let src = "tests/support/permfail/permfail.c";

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let status = std::process::Command::new(&cc)
        .args(["-O0", "-g", "-fPIC", "-shared", "-Wall", "-Wextra"])
        .arg("-o")
        .arg(&so_path)
        .arg(src)
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => panic!("permfail shim compile failed with status {s}"),
        // A machine without a C compiler can still build/use wrapfile
        // itself; skip exporting the shim path so the shim-backed tests
        // print a skip notice. A compiler that ran but rejected the source
        // is a real error and must fail loudly (the Ok arm above).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("cargo:warning=C compiler '{cc}' not found; permfail shim not built, shim-backed tests will skip");
            return;
        }
        Err(e) => panic!("failed to run C compiler while building permfail shim: {e}"),
    }

    println!("cargo:rustc-env=WRAPFILE_PERMFAIL_SO={}", so_path.display());
    println!("cargo:rerun-if-changed={src}");
}
