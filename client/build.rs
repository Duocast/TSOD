use std::{env, path::PathBuf};

include!("../proto/proto_files.rs");

fn main() {
    // Display/debug build identity (timestamp), distinct from semver release version
    // used by updater/network metadata via CARGO_PKG_VERSION.
    let build_version = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    println!("cargo:rustc-env=VP_CLIENT_BUILD_VERSION={build_version}");

    println!("cargo:rerun-if-env-changed=VCPKG_ROOT");
    println!("cargo:rerun-if-env-changed=DAV1D_ROOT");

    // Add dav1d native library search path for Windows/MSVC builds.
    // We do NOT emit `cargo:rustc-link-lib=dav1d` here because your build already
    // requests dav1d somewhere else; the error is that link.exe cannot FIND dav1d.lib.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());

        let dav1d_root = env::var("DAV1D_ROOT")
            .map(PathBuf::from)
            .or_else(|_| {
                env::var("VCPKG_ROOT")
                    .map(PathBuf::from)
                    .map(|p| p.join("installed").join("x64-windows"))
            })
            .unwrap_or_else(|_| PathBuf::from(r"C:\src\vcpkg\installed\x64-windows"));

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
