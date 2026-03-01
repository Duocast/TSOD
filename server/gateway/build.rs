use std::{env, path::PathBuf};

include!("../../proto/proto_files.rs");

fn main() {
    // Expected repo layout:
    // repo_root/
    //   proto/*.proto
    //   server/gateway/
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Default: ../../proto relative to server/gateway
    let proto_dir = env::var("PROTO_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest_dir.join("../../proto"));

    let proto_paths: Vec<PathBuf> = PROTO_FILES.iter().map(|p| proto_dir.join(p)).collect();

    for p in &proto_paths {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    println!("cargo:rerun-if-changed={}", proto_dir.display());

    let mut config = prost_build::Config::new();
    config.compile_protos(&proto_paths, &[proto_dir]).unwrap();
}
