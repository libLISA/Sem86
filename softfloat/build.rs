use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let target = std::env::var("TARGET").unwrap();
    let clang;
    let (build_dir, cc, ar) = if target.contains("aarch64-linux-android") {
        let sysroot = PathBuf::from(std::env::var("CARGO_NDK_SYSROOT_PATH").unwrap());
        let path = sysroot.parent().unwrap();
        let join = path.join("bin");
        let bin = join.to_string_lossy();
        clang = format!("{bin}/aarch64-linux-android26-clang");
        (
            "libsources/build/Android-aarch64-clang",
            clang.as_str(),
            "aarch64-linux-android-ar",
        )
    } else {
        // default to host build
        ("libsources/build/Linux-x86_64-GCC", "gcc", "ar")
    };

    let status = Command::new("make")
        .current_dir(build_dir)
        .env("CC", cc)
        .env("AR", ar)
        .status()
        .expect("Failed to run make");

    if !status.success() {
        panic!("Make failed with status: {}", status);
    }

    let lib_path = Path::new(build_dir).join("softfloat.a");
    let lib_copy_path = Path::new(build_dir).join("libsoftfloat.a");
    std::fs::copy(&lib_path, &lib_copy_path).unwrap();

    let lib_dir = Path::new(build_dir).canonicalize().unwrap();

    // Tell cargo where to find the built library
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=softfloat");
}
