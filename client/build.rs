use std::{env, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let proto_dir = env::var("PROTO_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest_dir.join("../proto"));

    let protos = [
        "common.proto",
        "caps.proto",
        "auth.proto",
        "channel.proto",
        "presence.proto",
        "chat.proto",
        "moderation.proto",
        "telemetry.proto",
        "control.proto",
    ];

    let proto_paths: Vec<PathBuf> = protos.iter().map(|p| proto_dir.join(p)).collect();

    for p in &proto_paths {
        println!("cargo:rerun-if-changed={}", p.display());
    }

    prost_build::Config::new()
        .compile_protos(&proto_paths, &[proto_dir])
        .unwrap();
}
