use std::env;
use std::path::PathBuf;
use std::process::Command;

fn build_shim(include_dir: &PathBuf) {
    let mut build = cc::Build::new();
    build
        .file("c_src/bdev.c")
        .file("c_src/json.c")
        .file("c_src/shim.c")
        .file("c_src/util.c")
        .include(include_dir)
        .flag("-std=gnu11")
        .cargo_metadata(false);

    build
        .try_compile("ironspdkshim")
        .expect("Failed to compile ironspdk C shim");
}

fn resolve_spdk_include() -> PathBuf {
    // try to get SPDK path from environment variable
    if let Ok(dir) = env::var("SPDK") {
        let p = PathBuf::from(dir).join("include");
        if p.exists() {
            return p;
        } else {
            panic!("SPDK/include not found");
        }
    }

    // get SPDK path from pkg-config
    let output = Command::new("pkg-config")
        .args(["--cflags-only-I", "spdk"])
        .output()
        .expect("pkg-config not found");

    if output.status.success() {
        let stdout = String::from_utf8(output.stdout).unwrap();

        for token in stdout.split_whitespace() {
            if let Some(path) = token.strip_prefix("-I") {
                return PathBuf::from(path);
            }
        }
    }

    panic!(
        "\n\nError: could not find SPDK headers\n\
         ------------------------------------------------\n\
         Options to fix this:\n\
         1. Set the SPDK environment variable to the root of your SPDK source tree\n\
         2. Install SPDK system-wide and ensure 'pkg-config' can find it\n\
         ------------------------------------------------\n"
    );
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let ironspdk_include_dir = manifest_dir.join("c_src");
    // Export include directory to dependent crates
    println!("cargo:include={}", ironspdk_include_dir.display());
    println!(
        "cargo:rustc-env=IRONSPDK_INCLUDE_DIR={}",
        ironspdk_include_dir.display()
    );
    let spdk_include_dir = resolve_spdk_include();
    build_shim(&spdk_include_dir);

    // pass OUT_DIR of ironspdk-sys to dependent crates
    let out_dir = env::var("OUT_DIR").unwrap();
    println!("cargo:metadata={}", out_dir);

    println!("cargo::rerun-if-changed=c_src/");
    println!("cargo::rerun-if-changed=src/");
    println!("cargo::rerun-if-env-changed=SPDK");
    println!("cargo::rerun-if-env-changed=CARGO_MANIFEST_DIR");
}
