use std::env;
use std::path::PathBuf;

fn build_c_src(spdk_include: &PathBuf) {
    let mut build = cc::Build::new();
    build
        .file("c_src/raid1.c")
        .include(spdk_include)
        .include(ironspdk_sys::include_dir())
        .flag("-std=gnu11")
        .cargo_metadata(false);

    build
        .try_compile("csrc")
        .expect("Failed to compile C source code");

    let out_dir = env::var("OUT_DIR").unwrap();
    println!("cargo:rustc-link-search=native={}", &out_dir);
    println!("cargo:rustc-link-arg=-lcsrc");
}

fn main() {
    ironspdk_sys::emit_preamble();
    build_c_src(&ironspdk_sys::resolve_spdk_include());
    ironspdk_sys::emit_rest();

    println!("cargo::rerun-if-changed=c_src/");
    println!("cargo::rerun-if-changed=src/");
}
