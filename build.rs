fn main() {
    println!("cargo:rerun-if-changed=src/loader/liblog_shim.c");
    println!("cargo:rerun-if-changed=src/loader/bionic_syscall_shim.c");
    println!("cargo:rerun-if-changed=src/loader/native_load_shim.cpp");
    println!("cargo:rerun-if-changed=src/loader/stdio_shim.c");
    println!("cargo:rerun-if-changed=src/loader/sigaltstack_shim.c");

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
    build_preload_library(
        "src/client_settings_path_shim.c",
        "libeclipse_client_settings_path.so",
        "ECLIPSE_CLIENT_SETTINGS_PATH_SHIM_SO",
    );
    build_preload_library(
        "tests/fixtures/next_interposer.c",
        "libeclipse_next_interposer_fixture.so",
        "ECLIPSE_NEXT_INTERPOSER_FIXTURE_SO",
    );
}

fn build_preload_library(source: &str, library: &str, env_var: &str) {
    use std::path::Path;
    use std::process::Command;

    println!("cargo:rerun-if-changed={source}");
    let out_dir = std::env::var_os("OUT_DIR").expect("OUT_DIR set by cargo");
    let output = Path::new(&out_dir).join(library);
    let compiler = cc::Build::new().get_compiler();
    let mut command = Command::new(compiler.path());
    command
        .args(compiler.args())
        .args(["-shared", "-fPIC", "-O2"])
        .arg(source)
        .arg("-ldl")
        .arg("-Wl,-z,relro,-z,now")
        .arg("-o")
        .arg(&output);
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to spawn the C compiler for {source}: {error}"));
    assert!(status.success(), "building {source} failed");
    println!("cargo:rustc-env={env_var}={}", output.display());
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

    reject_tls_in_libm_shim(&so);

    println!("cargo:rustc-env=ECLIPSE_LIBM_SHIM_SO={}", so.display());
}

fn reject_tls_in_libm_shim(so: &std::path::Path) {
    let Some(program_headers) = readelf("-lW", so) else {
        println!("cargo:warning=readelf not found; skipped the libm-shim TLS guard");
        return;
    };
    assert!(
        !program_headers
            .lines()
            .any(|line| line.split_whitespace().next() == Some("TLS")),
        "libm shim regressed: it now has a TLS segment, which the apkenv linker cannot set up. \
         The shim must stay no_std and free of thread-locals."
    );

    let relocations = readelf("-rW", so).expect("readelf ran for the program headers");
    let tls_relocation = relocations
        .lines()
        .filter_map(|line| line.split_whitespace().nth(2))
        .find(|kind| {
            ["TPOFF", "DTPMOD", "TLS"]
                .iter()
                .any(|tls| kind.contains(tls))
        });
    if let Some(kind) = tls_relocation {
        panic!(
            "libm shim regressed: it now has {kind} relocations, which the apkenv linker cannot \
             apply. The shim must stay no_std and free of thread-locals."
        );
    }
}

fn readelf(flag: &str, file: &std::path::Path) -> Option<String> {
    use std::process::Command;

    let output = match Command::new("readelf").arg(flag).arg(file).output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => panic!("failed to run readelf {flag} {}: {error}", file.display()),
    };
    assert!(
        output.status.success(),
        "readelf {flag} {} failed: {}",
        file.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8(output.stdout).expect("readelf prints UTF-8"))
}
