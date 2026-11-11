//! Build the interception shim: `shim/intercept.c` is compiled
//! into a shared library the launcher preloads into the wrapped
//! program. The library path is baked into the crate as the
//! `PIXELFLOW_INTERCEPT_SHIM` environment variable, so the runtime
//! finds it without searching the filesystem.
//!
//! The `cc` crate only produces static archives, so the build script
//! invokes the C compiler directly with `-shared -fPIC`.

use std::path::PathBuf;

fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap_or_default();
    if out_dir.is_empty() {
        return;
    }
    println!("cargo:rerun-if-changed=shim/intercept.c");
    let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let output = PathBuf::from(&out_dir).join("libpixelflow_intercept.so");
    let status = std::process::Command::new(&compiler)
        .args(["-O2", "-shared", "-fPIC"])
        .arg("shim/intercept.c")
        .arg("-o")
        .arg(&output)
        .status();
    match status {
        Ok(status) => {
            assert!(
                status.success(),
                "failed to compile the interception shim with {compiler}"
            );
        }
        Err(e) => {
            eprintln!("pixelflow-intercept: failed to run the C compiler {compiler}: {e}");
            std::process::exit(1);
        }
    }
    println!(
        "cargo:rustc-env=PIXELFLOW_INTERCEPT_SHIM={}",
        output.display()
    );
}
