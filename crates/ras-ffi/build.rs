//! Generate `include/casual_ras.h` from the annotated C ABI (ADR-096). Programmatic cbindgen — no
//! external CLI needed. Only re-runs when the API / config change (so writing the header can't loop).

fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");
    println!("cargo:rerun-if-changed=build.rs");

    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let config =
        cbindgen::Config::from_file(format!("{crate_dir}/cbindgen.toml")).unwrap_or_default();
    match cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_config(config)
        .generate()
    {
        Ok(bindings) => {
            let dir = format!("{crate_dir}/include");
            let _ = std::fs::create_dir_all(&dir);
            bindings.write_to_file(format!("{dir}/casual_ras.h"));
        }
        // Never fail the library build on a header-generation hiccup — the .so/.a still compiles.
        Err(e) => println!("cargo:warning=cbindgen header generation skipped: {e}"),
    }
}
