//! Build the interception shim: `shim/intercept.c` is compiled
//! into a shared library the launcher preloads into the wrapped
//! program. The library path is baked into the crate as the
//! `PIXELFLOW_INTERCEPT_SHIM` environment variable, so the runtime
//! finds it without searching the filesystem.
//!
//! The `cc` crate only produces static archives, so the build script
//! invokes the C compiler directly with `-shared -fPIC`.
//!
//! A second, 32-bit build is attempted with `-m32`: a 64-bit shared
//! object cannot be preloaded into a 32-bit process, and a 32-bit
//! target such as zsnes needs the 32-bit build. A toolchain without
//! 32-bit support records an empty
//! `PIXELFLOW_INTERCEPT_SHIM32` instead of failing the build; the
//! runtime reports a clear error when asked to wrap a 32-bit target
//! with no 32-bit shim.

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
    let output32 = PathBuf::from(&out_dir).join("libpixelflow_intercept32.so");
    let probe = std::process::Command::new(&compiler)
        .args(["-m32", "-O2", "-shared", "-fPIC"])
        .arg("shim/intercept.c")
        .arg("-o")
        .arg(&output32)
        .output();
    match probe {
        Ok(output) if output.status.success() => {
            println!(
                "cargo:rustc-env=PIXELFLOW_INTERCEPT_SHIM32={}",
                output32.display()
            );
        }
        _ => {
            println!(
                "cargo:warning=pixelflow-intercept: no 32-bit C toolchain; 32-bit targets cannot be intercepted"
            );
            println!("cargo:rustc-env=PIXELFLOW_INTERCEPT_SHIM32=");
        }
    }
}
