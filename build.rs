use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/rust-screen.ico");
    println!("cargo:rerun-if-env-changed=WINDRES");
    println!("cargo:rerun-if-env-changed=RC");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let output = out_dir.join(if msvc { "app.res" } else { "app.o" });
    let compiler = |program: std::ffi::OsString| {
        let mut command = Command::new(program);
        if msvc {
            command.arg("/nologo").arg("/fo").arg(&output).arg("app.rc");
        } else {
            command.args(["--input", "app.rc", "--output-format=coff", "--output"]);
            command.arg(&output);
        }
        command.current_dir("assets");
        command
    };
    let configured = env::var_os(if msvc { "RC" } else { "WINDRES" });
    let program = configured
        .clone()
        .unwrap_or_else(|| if msvc { "rc.exe" } else { "windres.exe" }.into());
    let mut status = compiler(program).status();
    // IDEs and plain `cargo check` often run without run.ps1's MinGW PATH. Fall back
    // to the default MSYS2 location; windres also needs that directory's gcc on PATH
    // to preprocess the .rc file.
    if !msvc
        && configured.is_none()
        && status
            .as_ref()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        let directory = PathBuf::from(r"C:\msys64\mingw64\bin");
        let fallback = directory.join("windres.exe");
        if fallback.is_file() {
            let path = env::join_paths(
                std::iter::once(directory)
                    .chain(env::var_os("PATH").iter().flat_map(env::split_paths)),
            )
            .expect("PATH entries are valid");
            status = compiler(fallback.into_os_string())
                .env("PATH", path)
                .status();
        }
    }
    let status = status.expect(
        "Windows icon compilation requires windres (GNU, e.g. MSYS2 MinGW-w64) or rc.exe \
         (Windows SDK) on PATH, or the WINDRES / RC environment variable",
    );
    assert!(status.success(), "Windows icon resource compilation failed");
    println!("cargo:rustc-link-arg-bins={}", output.display());
}
