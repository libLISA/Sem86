use std::fs::canonicalize;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    let llvm_build_dir = out_dir.join("llvm-build");
    let llvm_source_dir = canonicalize("llvm-source").unwrap().join("llvm");
    println!("cargo::rerun-if-changed=llvm-source");

    std::fs::create_dir_all(&llvm_build_dir).unwrap();

    let target_triple = std::env::var("TARGET").unwrap();
    let target_triple = target_triple.replace("-linux-", "-unknown-linux-");

    if target_os == "android" {
        println!("cargo:libdir={}", llvm_build_dir.join("lib").display()); // DEP_LLVM_LIBDIR

        let abi = match target_arch.as_str() {
            "aarch64" => "arm64-v8a",
            "x86_64" => "x86_64",
            _ => panic!("unsupported architecture: {target_arch}"),
        };

        let ndk_home = std::env::var("ANDROID_NDK_HOME").unwrap();
        let toolchain = format!("{ndk_home}/29.0.14206865/build/cmake/android.toolchain.cmake");
        let ok = Command::new("cmake")
            .arg(llvm_source_dir)
            .args([
                &format!("-DCMAKE_TOOLCHAIN_FILE={toolchain}"),
                &format!("-DANDROID_ABI={abi}"),
                "-DANDROID_PLATFOlRM=android-21",
                &format!("-DLLVM_HOST_TRIPLE={target_triple}"),
                "-DCROSS_TOOLCHAIN_FLAGS_NATIVE='-DCMAKE_C_COMPILER=cc;-DCMAKE_CXX_COMPILER=c++'",
                "-DCMAKE_BUILD_TYPE=Release",
                "-DLLVM_TARGETS_TO_BUILD=X86;AArch64",
                "-DLLVM_ENABLE_PROJECTS=llvm",
                "-DLLVM_ENABLE_BINDINGS=ON",
                "-DLLVM_ENABLE_TERMINFO=OFF",
                "-DLLVM_ENABLE_THREADS=OFF",
                "-DLLVM_ENABLE_LIBEDIT=OFF",
                "-DLLVM_INCLUDE_TESTS=OFF",
                "-DLLVM_BUILD_TESTS=OFF",
                "-DLLVM_INCLUDE_EXAMPLES=OFF",
                "-DLLVM_BUILD_EXAMPLES=OFF",
                "-DLLVM_INCLUDE_BENCHMARKS=OFF",
                "-DLLVM_BUILD_TOOLS=OFF",
                "-DLLVM_ENABLE_C_API=ON",
                "-DDLLVM_ENABLE_ZLIB=OFF",
                "-DLLVM_ENABLE_EH=OFF",
                "-DLLVM_ENABLE_RTTI=OFF",
                "-DLLVM_BUILD_LLVM_DYLIB=ON",
                "-DLLVM_LINK_LLVM_DYLIB=ON",
                "-DCMAKE_INTERPROCEDURAL_OPTIMIZATION=ON",
                "-DCMAKE_C_FLAGS_RELEASE=-O3 -DNDEBUG -g0",
                "-DCMAKE_CXX_FLAGS_RELEASE=-O3 -DNDEBUG -g0 -fno-exceptions -fno-rtti",
            ])
            .current_dir(&llvm_build_dir)
            .spawn()
            .unwrap()
            .wait()
            .unwrap()
            .success();
        assert!(ok);

        let ok = Command::new("cmake")
            .args(["--build", ".", "--parallel", "8"])
            .current_dir(&llvm_build_dir)
            .spawn()
            .unwrap()
            .wait()
            .unwrap()
            .success();
        assert!(ok);
    } else {
        println!(
            "cargo:warning=Unable to build LLVM for non-android OS: target_os={target_os}, target_arch={target_arch}, triple={target_triple}"
        );
    }
}
