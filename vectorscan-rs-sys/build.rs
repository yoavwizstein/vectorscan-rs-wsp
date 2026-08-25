use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("`{}` should be set in the environment", name))
}

fn rename_library(dst: &Path) {
    for lib_folder in &[dst.join("lib"), dst.join("lib64")] {
        // GNU/Unix toolchains produce libhs.a; MSVC/clang-cl produces hs.lib.
        for (from, to) in &[("libhs.a", "libvs.a"), ("hs.lib", "vs.lib")] {
            let src = lib_folder.join(from);
            let dest = lib_folder.join(to);
            if src.exists() {
                fs::rename(&src, &dest).unwrap_or_else(|e| {
                    panic!("Failed to rename {:?} to {:?}: {}", src, dest, e)
                });
            }
        }
    }
}

fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap_or_else(|e| {
        panic!("Failed to create directory {}: {e}", dst.display())
    });
    for entry in fs::read_dir(src).unwrap_or_else(|e| {
        panic!("Failed to read directory {}: {e}", src.display())
    }) {
        let entry = entry.expect("Failed to read directory entry");
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type().expect("Failed to get file type").is_dir() {
            copy_dir_all(&src_path, &dst_path);
        } else {
            fs::copy(&src_path, &dst_path).unwrap_or_else(|e| {
                panic!("Failed to copy {} -> {}: {e}", src_path.display(), dst_path.display());
            });
        }
    }
}

/// Try to resolve the submodule's git HEAD file for Cargo change detection.
/// For a git submodule, `<submodule>/.git` is a file containing `gitdir: <path>`.
fn resolve_submodule_head(submodule_dir: &Path) -> Option<PathBuf> {
    let dot_git = submodule_dir.join(".git");
    let content = fs::read_to_string(&dot_git).ok()?;
    let gitdir_ref = content.strip_prefix("gitdir: ")?.trim();
    let resolved = submodule_dir.join(gitdir_ref);
    let head = resolved.join("HEAD");
    head.exists().then_some(head)
}

fn build_vectorscan(manifest_dir: &Path, out_dir: &Path, is_windows_msvc: bool) {
    let include_dir = out_dir
        .join("include")
        .into_os_string()
        .into_string()
        .unwrap();

    let submodule_dir = manifest_dir.join("vectorscan");
    let vectorscan_src_dir = out_dir.join("vectorscan-src");

    assert!(
        submodule_dir.join("CMakeLists.txt").exists(),
        "Vectorscan submodule not found at {}. Run: git submodule update --init",
        submodule_dir.display()
    );

    match fs::remove_dir_all(&vectorscan_src_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("Failed to clean vectorscan source directory: {e}"),
    }
    copy_dir_all(&submodule_dir, &vectorscan_src_dir);

    let patches_dir = manifest_dir.join("patches");
    if patches_dir.is_dir() {
        let mut patches: Vec<_> = fs::read_dir(&patches_dir)
            .expect("Failed to read patches directory")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "patch"))
            .map(|e| e.path())
            .collect();
        patches.sort();
        for patch_path in &patches {
            let patchfile = File::open(patch_path)
                .unwrap_or_else(|e| panic!("Failed to open {}: {e}", patch_path.display()));
            let output = Command::new("patch")
                .args(["-p1", "--forward"])
                .current_dir(&vectorscan_src_dir)
                .stdin(patchfile)
                .output()
                .unwrap_or_else(|e| panic!("Failed to run patch for {}: {e}", patch_path.display()));
            assert!(
                output.status.success(),
                "Failed to apply {}:\nstdout: {}\nstderr: {}",
                patch_path.display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            eprintln!("Applied {}", patch_path.file_name().unwrap().to_string_lossy());
        }
    }
    eprintln!("Vectorscan source prepared at {}", vectorscan_src_dir.display());

    let mut cfg = cmake::Config::new(&vectorscan_src_dir);
    cfg.out_dir(out_dir);

    macro_rules! cfg_define_feature {
        ($cmake_feature: tt, $cargo_feature: tt) => {
            cfg.define(
                $cmake_feature,
                if cfg!(feature = $cargo_feature) {
                    "ON"
                } else {
                    "OFF"
                },
            )
        };
    }

    // On MSVC build RelWithDebInfo so the static lib carries CodeView debug info
    // (optimized + PDB-able). The consuming binary keeps debug info for
    // stacktraces, and a plain Release vectorscan would contribute no symbols.
    // RelWithDebInfo still maps to RELEASE_BUILD=TRUE in vectorscan (fat runtime
    // gate passes) and the CRT stays /MT via CMAKE_MSVC_RUNTIME_LIBRARY below.
    cfg.profile(if is_windows_msvc { "RelWithDebInfo" } else { "Release" })
        .define("CMAKE_INSTALL_INCLUDEDIR", &include_dir)
        .define("CMAKE_VERBOSE_MAKEFILE", "ON")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("BUILD_STATIC_LIBS", "ON")
        .define("BUILD_EXAMPLES", "OFF")
        .define("BUILD_BENCHMARKS", "OFF")
        .define("BUILD_DOC", "OFF");

    cfg_define_feature!("BUILD_UNIT", "unit_hyperscan");
    cfg_define_feature!("USE_CPU_NATIVE", "cpu_native");

    if cfg!(feature = "asan") {
        cfg.define("SANITIZE", "address");
    }

    if cfg!(feature = "fat_runtime") {
        cfg.define("FAT_RUNTIME", "ON");
    } else {
        cfg.define("FAT_RUNTIME", "OFF");
    }

    if cfg!(feature = "simd_specialization") {
        macro_rules! x86_64_feature {
            () => {{
                #[cfg(target_arch = "x86_64")]
                {
                    "ON"
                }
                #[cfg(not(target_arch = "x86_64"))]
                {
                    "OFF"
                }
            }};
        }

        macro_rules! aarch64_feature {
            () => {{
                #[cfg(target_arch = "aarch64")]
                {
                    "ON"
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
                    "OFF"
                }
            }};
        }

        cfg.define("BUILD_AVX2", x86_64_feature!());
        cfg.define("BUILD_AVX512", x86_64_feature!());
        cfg.define("BUILD_AVX512VBMI", x86_64_feature!());

        cfg.define("BUILD_SVE", aarch64_feature!());
        cfg.define("BUILD_SVE2", aarch64_feature!());
        cfg.define("BUILD_SVE2_BITPERM", aarch64_feature!());
    } else {
        cfg.define("BUILD_AVX2", "OFF")
            .define("BUILD_AVX512", "OFF")
            .define("BUILD_AVX512VBMI", "OFF")
            .define("BUILD_SVE", "OFF")
            .define("BUILD_SVE2", "OFF")
            .define("BUILD_SVE2_BITPERM", "OFF");
    }

    if is_windows_msvc {
        // Build with clang-cl so the objects are MSVC-ABI and the resulting
        // static lib links into any MSVC binary (no DLL boundary needed).
        cfg.generator("Ninja");
        cfg.define("CMAKE_C_COMPILER", "clang-cl");
        cfg.define("CMAKE_CXX_COMPILER", "clang-cl");
        // clang-cl rejects a few GNU-style flags the build emits; silence those
        // and pass the MSVC-style language / exception-handling flags it wants.
        for f in ["-Wno-unknown-argument", "-Wno-unused-command-line-argument", "/std:c17"] {
            cfg.cflag(f);
        }
        for f in ["-Wno-unknown-argument", "-Wno-unused-command-line-argument", "/std:c++17", "/EHsc"] {
            cfg.cxxflag(f);
        }
        // Boost headers in isolation: pointing CMake at e.g. an MSYS2 include
        // dir would drag mingw's libc/intrinsic headers onto clang-cl's system
        // include path and break clang's <mmintrin.h>. Junction *only* boost
        // into the source tree's include/ dir (which vectorscan's boost.cmake
        // probes first) and force module-mode FindBoost to resolve there.
        let boost_include = env("VECTORSCAN_BOOST_INCLUDE");
        let boost_target = Path::new(&boost_include).join("boost");
        assert!(
            boost_target.join("version.hpp").exists(),
            "VECTORSCAN_BOOST_INCLUDE ({}) must contain boost/version.hpp",
            boost_include
        );
        // Junction only boost/ into a private include dir OUTSIDE the source tree
        // (build.rs wipes vectorscan-src each run) and point BOOST_ROOT at it.
        let boost_root = out_dir.join("boost-root");
        let boost_link = boost_root.join("include").join("boost");
        if !boost_link.exists() {
            fs::create_dir_all(boost_root.join("include"))
                .expect("Failed to create boost-root/include");
            let status = Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&boost_link)
                .arg(&boost_target)
                .status()
                .expect("Failed to run mklink for boost junction");
            assert!(status.success(), "mklink /J for boost junction failed");
        }
        cfg.define("BOOST_ROOT", &boost_root);
        cfg.define("CMAKE_POLICY_DEFAULT_CMP0167", "OLD");
        cfg.define("Boost_NO_BOOST_CMAKE", "ON");
        cfg.define("Boost_NO_SYSTEM_PATHS", "ON");
        // Match the consuming crate's CRT linkage: when the target is built with
        // +crt-static (e.g. via .cargo/config rustflags) link the static MSVC
        // runtime (/MT), otherwise the dynamic one (/MD). Mixing them makes the
        // final link fail with unresolved __imp_* CRT externals (_aligned_malloc,
        // _W_Getmonths, ...). build.rs always builds the Release config, so this
        // is the complete choice (CMP0091 NEW; CMake >= 3.15).
        let crt_static = std::env::var("CARGO_CFG_TARGET_FEATURE")
            .unwrap_or_default()
            .split(',')
            .any(|f| f == "crt-static");
        cfg.define(
            "CMAKE_MSVC_RUNTIME_LIBRARY",
            if crt_static { "MultiThreaded" } else { "MultiThreadedDLL" },
        );
        // The MSVC fat-runtime CMake path (msvc-support.patch) invokes
        // cmake/fat_rename.ps1 (a COFF whole-variant symbol renamer via
        // PowerShell, so no Python is required); place it where
        // ${PROJECT_SOURCE_DIR}/cmake expects it. Harmless for non-fat builds
        // (the patched CMake only references it inside the MSVC fat branch).
        fs::copy(
            manifest_dir.join("fat_rename.ps1"),
            vectorscan_src_dir.join("cmake").join("fat_rename.ps1"),
        )
        .expect("Failed to copy fat_rename.ps1 into vectorscan source tree");
        // Build only the `hs` static lib, not the default `all`/`install`
        // target. That avoids compiling the unit tests and util test-helpers
        // (e.g. util/expressions.cpp needs POSIX dirent.h), which don't build
        // on MSVC and aren't needed to link the library.
        cfg.build_target("hs");
    }

    if cfg!(feature = "fat_runtime") {
        if is_windows_msvc {
            // MSVC fat runtime renames symbols via cmake/fat_rename.ps1, a
            // self-contained COFF pass that needs no libc symbol list.
        } else {
            let libc_path = String::from_utf8(
                Command::new("cc")
                    .args(["--print-file-name=libc.so.6"])
                    .output()
                    .expect("Failed to get libc.so.6 path from cc")
                    .stdout,
            )
            .expect("Invalid UTF-8 in cc output")
            .trim()
            .to_string();
            std::env::set_var("VECTORSCAN_LIBC_SO", &libc_path);
            eprintln!("VECTORSCAN_LIBC_SO={libc_path}");
        }
    }

    cfg.build();

    rename_library(out_dir);

    println!("cargo:rustc-link-search={}", out_dir.join("lib").display());
    println!(
        "cargo:rustc-link-search={}",
        out_dir.join("lib64").display()
    );

    if is_windows_msvc {
        // With `build_target("hs")` (no install step) the archive stays in the
        // CMake binary dir; rename hs.lib -> vs.lib there and link from it.
        let build_root = out_dir.join("build");
        rename_library(&build_root);
        println!(
            "cargo:rustc-link-search={}",
            build_root.join("lib").display()
        );
    }
}

/// Detect the host C++ compiler (via `c++ -v`) and emit the matching C++
/// standard library to link against: `stdc++` for GCC, `c++` for Clang.
/// Panics if neither toolchain is detected.
fn link_cpp_stdlib() {
    let compiler_version_out = String::from_utf8(
        Command::new("c++")
            .args(["-v"])
            .output()
            .expect("Failed to get C++ compiler version")
            .stderr,
    )
    .unwrap();

    if compiler_version_out.contains("gcc") {
        println!("cargo:rustc-link-lib=stdc++");
    } else if compiler_version_out.contains("clang") {
        println!("cargo:rustc-link-lib=c++");
    } else {
        panic!("No compatible compiler found: either clang or gcc is needed");
    }
}

fn main() {
    let target_os = env("CARGO_CFG_TARGET_OS");
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let is_windows_msvc = target_os == "windows" && target_env == "msvc";

    println!("cargo:rerun-if-env-changed=VECTORSCAN_LIB_DIR");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=patches");
    println!("cargo:rerun-if-changed=fat_rename.ps1");

    // CARGO_FEATURE_* env vars are set by cargo when features are enabled.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_FAT_RUNTIME");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SIMD_SPECIALIZATION");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_CPU_NATIVE");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_UNIT_HYPERSCAN");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_ASAN");

    let manifest_dir = PathBuf::from(env("CARGO_MANIFEST_DIR"));

    // Fingerprint the submodule's git HEAD so that `git submodule update`
    // automatically triggers a rebuild without scanning thousands of files.
    if let Some(head_path) = resolve_submodule_head(&manifest_dir.join("vectorscan")) {
        println!("cargo:rerun-if-changed={}", head_path.display());
    }

    let out_dir = PathBuf::from(env("OUT_DIR"));

    if is_windows_msvc {
        // Build vectorscan from source as a static, MSVC-ABI library (clang-cl)
        // and link it statically -- no DLL, links into any MSVC binary.
        if let Some(lib_dir) = std::env::var_os("VECTORSCAN_LIB_DIR") {
            println!("cargo:rustc-link-search={}", lib_dir.to_string_lossy());
        } else {
            build_vectorscan(&manifest_dir, &out_dir, is_windows_msvc);
        }

        println!("cargo:rustc-link-lib=static=vs");
        // The MSVC C++ runtime (libcpmt/vcruntime/ucrt) is auto-linked via the
        // #pragma comment(lib) directives clang-cl embeds in the objects, so no
        // explicit C++ standard library needs to be added here.
    } else {
        link_cpp_stdlib();

        if let Some(lib_dir) = std::env::var_os("VECTORSCAN_LIB_DIR") {
            println!("cargo:rustc-link-search={}", lib_dir.display());
        } else {
            build_vectorscan(&manifest_dir, &out_dir, is_windows_msvc);
        }

        println!("cargo:rustc-link-lib=static=vs");

        #[cfg(feature = "unit_hyperscan")]
        {
            let unittests = out_dir.join("build").join("bin").join("unit-hyperscan");
            match Command::new(unittests).status() {
                Ok(rc) if rc.success() => {}
                Ok(rc) => panic!("Failed to run unit tests: exit with code {rc}"),
                Err(e) => panic!("Failed to run unit tests: {e}"),
            }
        }
    }

    #[cfg(feature = "bindgen")]
    {
        // Headers are installed to OUT_DIR/include by build_vectorscan
        // (via CMAKE_INSTALL_INCLUDEDIR); point bindgen's clang there.
        let include_dir = out_dir.join("include");
        let config = bindgen::Builder::default()
            .allowlist_function("hs_.*")
            .allowlist_type("hs_.*")
            .allowlist_var("HS_.*")
            .header("wrapper.h")
            .clang_arg(format!("-I{}", include_dir.display()));
        config
            .generate()
            .expect("Unable to generate bindings")
            .write_to_file(out_dir.join("bindings.rs"))
            .expect("Failed to write Rust bindings to Vectorscan");
    }
    #[cfg(not(feature = "bindgen"))]
    {
        fs::copy("src/bindings.rs", out_dir.join("bindings.rs"))
            .expect("Failed to write Rust bindings to Vectorscan");
    }
}
