use std::{env, path::PathBuf};

include!("../proto/proto_files.rs");

fn vcpkg_installed_root() -> Option<PathBuf> {
    let vcpkg_root = env::var("VCPKG_ROOT").ok().map(PathBuf::from)?;
    let triplet = env::var("VCPKG_TARGET_TRIPLET")
        .or_else(|_| env::var("VCPKG_DEFAULT_TRIPLET"))
        .unwrap_or_else(|_| "x64-windows-static".to_string());

    Some(vcpkg_root.join("installed").join(triplet))
}

fn main() {
    let build_version = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    println!("cargo:rustc-env=VP_CLIENT_BUILD_VERSION={build_version}");

    println!("cargo:rerun-if-env-changed=VCPKG_ROOT");
    println!("cargo:rerun-if-env-changed=DAV1D_ROOT");
    println!("cargo:rerun-if-env-changed=VPX_ROOT");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_LIBDIR");
    println!("cargo:rerun-if-env-changed=VCPKG_DEFAULT_TRIPLET");
    println!("cargo:rerun-if-env-changed=VCPKG_TARGET_TRIPLET");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());

    // ── libvpx (VP9 encode/decode) ────────────────────────────────────────────
    if target_os == "windows" && target_env == "msvc" {
        // On Windows/MSVC, resolve libvpx from VPX_ROOT or VCPKG_ROOT.
        let vpx_root = env::var("VPX_ROOT")
            .map(PathBuf::from)
            .ok()
            .or_else(vcpkg_installed_root)
            .unwrap_or_else(|| PathBuf::from(r"C:\src\vcpkg\installed\x64-windows-static"));

        let lib_dir = if profile == "release" {
            vpx_root.join("lib")
        } else {
            vpx_root.join("debug").join("lib")
        };

        let vpx_lib = lib_dir.join("vpx.lib");
        if !vpx_lib.exists() {
            println!(
                "cargo:warning=vpx.lib was not found at {}. Set VPX_ROOT or VCPKG_ROOT if needed.",
                vpx_lib.display()
            );
        }

        println!("cargo:rustc-link-search=native={}", lib_dir.display());
        println!("cargo:rustc-link-lib=static=vpx");
    } else if target_os == "linux" {
        match pkg_config::Config::new().cargo_metadata(false).probe("vpx") {
            Ok(lib) => {
                for path in lib.link_paths {
                    println!("cargo:rustc-link-search=native={}", path.display());
                }
            }
            Err(e) => {
                println!(
                    "cargo:warning=libvpx not found via pkg-config ({e}). \
                     VP9 backends will be disabled at runtime. \
                     Install libvpx-dev / libvpx-devel to enable them."
                );
            }
        }
        println!("cargo:rustc-link-lib=vpx");
    }

    // ── libdav1d (AV1 decode) ─────────────────────────────────────────────────
    if target_os == "windows" && target_env == "msvc" {
        let dav1d_root = env::var("DAV1D_ROOT")
            .map(PathBuf::from)
            .ok()
            .or_else(vcpkg_installed_root)
            .unwrap_or_else(|| PathBuf::from(r"C:\src\vcpkg\installed\x64-windows-static"));

        let lib_dir = if profile == "release" {
            dav1d_root.join("lib")
        } else {
            dav1d_root.join("debug").join("lib")
        };

        let dav1d_lib = lib_dir.join("dav1d.lib");
        if !dav1d_lib.exists() {
            println!(
                "cargo:warning=dav1d.lib was not found at {}. Set DAV1D_ROOT or VCPKG_ROOT if needed.",
                dav1d_lib.display()
            );
        }

        println!("cargo:rustc-link-search=native={}", lib_dir.display());
        println!("cargo:rustc-link-lib=static=dav1d");
    } else if target_os == "linux" {
        let lib = pkg_config::Config::new()
            .cargo_metadata(false)
            .probe("dav1d")
            .expect("Failed to find dav1d via pkg-config. Install libdav1d-dev/libdav1d-devel/dav1d.");

        for path in lib.link_paths {
            println!("cargo:rustc-link-search=native={}", path.display());
        }

        // Keep this only if nothing else already links dav1d.
        println!("cargo:rustc-link-lib=dav1d");
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let proto_dir = env::var("PROTO_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest_dir.join("../proto"));

    let proto_paths: Vec<PathBuf> = PROTO_FILES.iter().map(|p| proto_dir.join(p)).collect();

    for p in &proto_paths {
        println!("cargo:rerun-if-changed={}", p.display());
    }

    prost_build::Config::new()
        .compile_protos(&proto_paths, &[proto_dir])
        .unwrap();
}
