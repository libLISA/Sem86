use std::path::PathBuf;

// If we do not explicitly link llvm IN THIS VERY CRATE, the symbols will be missing in the final *.so file.
fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();

    if target_os == "android" {
        let libdir = PathBuf::from(std::env::var("DEP_LLVM_LIBDIR").unwrap());
        let libdir = std::fs::canonicalize(libdir).unwrap();

        println!("cargo:rustc-link-search=native={}", libdir.display());
        println!("cargo:rustc-link-lib=dylib=LLVM");

        let so = libdir.join("libLLVM.so");

        let output_path = PathBuf::from(std::env::var("CARGO_NDK_OUTPUT_PATH").unwrap());
        let abi_dir = output_path.join(std::env::var("ANDROID_ABI").unwrap());

        std::fs::create_dir_all(&abi_dir).unwrap();

        let llvm_so_target = abi_dir.join("libLLVM.so");

        std::fs::copy(so, llvm_so_target).unwrap();
    }
}
