use std::{env, path::PathBuf};

include!("../proto/proto_files.rs");

fn main() {
    let build_version = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    println!("cargo:rustc-env=VP_CLIENT_BUILD_VERSION={build_version}");

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
