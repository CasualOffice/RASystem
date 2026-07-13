//! Offline protobuf codegen for the Casual RAS control channel.
//!
//! `proto/casual_ras.proto` (package `casual_ras.v1`) is the wire source of truth. We compile it
//! with `protox` (a pure-Rust protoc replacement) into a `FileDescriptorSet`, then let
//! `prost-build` emit Rust into `OUT_DIR`. No system `protoc`, no network, no vendored binary.
//! Generated code is `include!`d from `src/codec.rs` and is never committed or hand-edited.
//!
//! Must not use `unwrap`/`expect` (workspace clippy gate applies to build scripts too): `main`
//! returns `Result` and propagates via `?`.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Resolve the workspace-root `proto/` dir relative to this crate's manifest so the build works
    // regardless of the cwd cargo invokes it from.
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_root = manifest_dir.join("..").join("..").join("proto");
    let proto_file = proto_root.join("casual_ras.proto");

    // Rebuild only when the schema (or this script) changes — keeps incremental builds hermetic.
    println!("cargo:rerun-if-changed={}", proto_file.display());
    println!("cargo:rerun-if-changed=build.rs");

    // 1. Pure-Rust parse+link: .proto -> FileDescriptorSet. No protoc process is spawned.
    let file_descriptors = protox::compile([&proto_file], [&proto_root])?;

    // 2. FileDescriptorSet -> Rust source in OUT_DIR. `compile_fds` (not `compile_protos`) consumes
    //    an already-built descriptor and therefore never invokes `protoc`.
    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    prost_build::Config::new()
        .out_dir(&out_dir)
        // `Bytes` for `bytes`-typed proto fields so AuthEnvelope.payload maps to `bytes::Bytes`
        // (zero-copy, lossless opaque round-trip) rather than `Vec<u8>`.
        .bytes(["."])
        .compile_fds(file_descriptors)?;

    Ok(())
}
