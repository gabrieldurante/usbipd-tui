use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=sidecar/Cargo.toml");
    println!("cargo:rerun-if-changed=sidecar/src/main.rs");
    println!("cargo:rerun-if-env-changed=USBIPD_TUI_ELEVATE_SIDECAR_EXE");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let embedded = out_dir.join("usbipd-tui-elevate.exe");

    if let Some(path) = env::var_os("USBIPD_TUI_ELEVATE_SIDECAR_EXE") {
        fs::copy(&path, &embedded).unwrap_or_else(|error| {
            panic!("could not copy USBIPD_TUI_ELEVATE_SIDECAR_EXE: {error}")
        });
        println!(
            "cargo:rustc-env=USBIPD_TUI_SIDECAR_PATH={}",
            embedded.display()
        );
        return;
    }

    let target_dir = out_dir.join("sidecar-target");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"))
        .join("sidecar/Cargo.toml");
    let status = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--release")
        .arg("--target")
        .arg("x86_64-pc-windows-gnu")
        .arg("--target-dir")
        .arg(&target_dir)
        .status()
        .unwrap_or_else(|error| panic!("could not start sidecar build: {error}"));
    if !status.success() {
        panic!(
            "could not build the embedded Windows UAC helper. Run `./build-docker.sh`, or install the Rust target `x86_64-pc-windows-gnu` and a MinGW-w64 linker locally."
        );
    }

    let helper = target_dir.join("x86_64-pc-windows-gnu/release/usbipd-tui-elevate.exe");
    fs::copy(&helper, &embedded)
        .unwrap_or_else(|error| panic!("could not embed Windows UAC helper: {error}"));
    println!(
        "cargo:rustc-env=USBIPD_TUI_SIDECAR_PATH={}",
        embedded.display()
    );
}
