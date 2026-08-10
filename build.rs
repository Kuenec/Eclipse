fn main() {
    println!("cargo:rerun-if-changed=src/loader/liblog_shim.c");
    println!("cargo:rerun-if-changed=src/loader/bionic_syscall_shim.c");
    println!("cargo:rerun-if-changed=src/loader/native_load_shim.cpp");
    println!("cargo:rerun-if-changed=src/loader/stdio_shim.c");
    println!("cargo:rerun-if-changed=src/loader/sigaltstack_shim.c");
    println!("cargo:rerun-if-changed=src/client_settings_path_shim.c");

    cc::Build::new()
        .file("src/loader/liblog_shim.c")
        .compile("eclipse_liblog_shim");

    cc::Build::new()
        .file("src/loader/bionic_syscall_shim.c")
        .compile("eclipse_bionic_syscall_shim");

    cc::Build::new()
        .file("src/loader/stdio_shim.c")
        .compile("eclipse_stdio_shim");

    cc::Build::new()
        .file("src/loader/sigaltstack_shim.c")
        .compile("eclipse_sigaltstack_shim");

    cc::Build::new()
        .cpp(true)
        .file("src/loader/native_load_shim.cpp")
        .compile("eclipse_native_load_shim");

    build_libm_shim();
    build_client_settings_path_shim();
}

fn build_client_settings_path_shim() {
    use std::path::Path;
    use std::process::Command;

    let out_dir = std::env::var_os("OUT_DIR").expect("OUT_DIR set by cargo");
    let output = Path::new(&out_dir).join("libeclipse_client_settings_path.so");
    let compiler = cc::Build::new().get_compiler();
    let mut command = Command::new(compiler.path());
    command
        .args(compiler.args())
        .args(["-shared", "-fPIC", "-O2"])
        .arg("src/client_settings_path_shim.c")
        .arg("-Wl,-z,relro,-z,now")
        .arg("-o")
        .arg(&output);
    let status = command
        .status()
        .expect("failed to spawn the C compiler for the client-settings path shim");
    assert!(
        status.success(),
        "building src/client_settings_path_shim.c failed"
    );
    println!(
        "cargo:rustc-env=ECLIPSE_CLIENT_SETTINGS_PATH_SHIM_SO={}",
        output.display()
    );
}

fn build_libm_shim() {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    let manifest = "crates/libm-shim/Cargo.toml";

    println!("cargo:rerun-if-changed=crates/libm-shim/src/lib.rs");
    println!("cargo:rerun-if-changed=crates/libm-shim/Cargo.toml");

    let out_dir = std::env::var_os("OUT_DIR").expect("OUT_DIR set by cargo");

    let shim_target = Path::new(&out_dir).join("libm-shim-target");

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(&cargo)
        .args(["build", "--release", "--manifest-path", manifest])
        .arg("--target-dir")
        .arg(&shim_target)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .status()
        .expect("failed to spawn cargo to build the libm shim");
    assert!(status.success(), "building crates/libm-shim failed");

    let so: PathBuf = shim_target.join("release").join("libeclipse_libm_shim.so");
    assert!(
        so.exists(),
        "libm shim build did not produce {} — check crates/libm-shim",
        so.display()
    );

    if let Ok(out) = Command::new("readelf").arg("-rW").arg(&so).output() {
        if out.status.success() {
            let relocs = String::from_utf8_lossy(&out.stdout);
            assert!(
                !relocs.contains("R_X86_64_TPOFF64"),
                "libm shim regressed: it now has R_X86_64_TPOFF64 (the apkenv linker cannot apply \
                 it). The shim must stay no_std/no-TLS."
            );
        }
    } else {
        println!("cargo:warning=readelf not found; skipped the libm-shim modern-reloc guard");
    }

    println!("cargo:rustc-env=ECLIPSE_LIBM_SHIM_SO={}", so.display());
}
