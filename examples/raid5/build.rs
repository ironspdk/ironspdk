use ironspdk_sys::build_utils;
use std::env;
use std::path::Path;

fn build_c_src(spdk_include: &Path) {
    let mut build = cc::Build::new();
    build
        .file("c_src/raid5.c")
        .flag("-isystem")
        .flag(spdk_include.to_str().unwrap())
        .include(build_utils::include_dir())
        .flag("-std=gnu11")
        .cargo_metadata(false);

    build
        .try_compile("csrc")
        .expect("Failed to compile C source code");

    let out_dir = env::var("OUT_DIR").unwrap();
    println!("cargo:rustc-link-search=native={}", out_dir);
    println!("cargo:rustc-link-arg=-lcsrc");
}

fn main() {
    build_utils::emit_preamble();
    build_c_src(&build_utils::resolve_spdk_include());
    build_utils::emit_rest();

    println!("cargo::rerun-if-changed=c_src/");
    println!("cargo::rerun-if-changed=src/");
}
