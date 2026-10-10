use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

// Macros for C FFI generation

macro_rules! opaque_types {
    ($builder:ident, $($name:literal),+ $(,)?) => {
        $(
            $builder = $builder
                .allowlist_type(concat!("^", $name, "$"))
                .opaque_type(concat!("^", $name, "$"));
        )+
    };
}

macro_rules! allowlist_types {
    ($builder:ident, $($name:literal),+ $(,)?) => {
        $(
            $builder = $builder
                .allowlist_type(concat!("^", $name, "$"));
        )+
    };
}

macro_rules! allowlist_functions {
    ($builder:ident, $($name:literal),+ $(,)?) => {
        $(
            $builder = $builder
                .allowlist_function(concat!("^", $name, "$"));
        )+
    };
}

fn generate_ffi(spdk_include_dir: &Path, out_dir: &Path) {
    let mut builder = bindgen::Builder::default()
        .header("c_src/bindgen.h")
        .clang_arg(format!("-I{}", spdk_include_dir.display()))
        .clang_arg("-std=gnu11")
        .prepend_enum_name(false)
        .allowlist_recursively(false);

    opaque_types!(
        builder,
        // Opaque SPDK types used by ironspdk
        "spdk_thread",
        "spdk_poller",
        "spdk_io_channel",
        "spdk_bdev_desc",
        "spdk_bdev",
        "spdk_bdev_io",
        "spdk_app_opts",
        "spdk_json_val",
        "spdk_cpuset",
        // Indirect SPDK dependencies
        "spdk_bdev_alias",
        "spdk_bdev_name",
        "spdk_bdev_module",
        "spdk_bdev_module_claim",
        "spdk_bdev_qos",
        "spdk_spinlock",
        "spdk_bdev_io_stat",
        "spdk_bdev_io_block_params",
        "spdk_bdev_io_reset_params",
        "spdk_bdev_io_abort_params",
        "spdk_bdev_io_nvme_passthru_params",
        "spdk_bdev_io_zone_mgmt_params",
        "lba_range_tailq_t",
        "bdev_io_tailq_t",
        "lba_range",
    );

    allowlist_types!(
        builder,
        // `struct iovec` type used by ironspdk
        "iovec",
        // enum SPDK types used by ironspdk
        "spdk_dif_type",
        "spdk_bdev_io_type",
        // Dependencies of SPDK not used directly
        "spdk_msg_fn",
        "spdk_poller_fn",
        "spdk_bdev_status",
        "spdk_bdev_claim_type",
        "spdk_bdev_unregister_cb",
        "spdk_app_shutdown_cb",
        "spdk_bdev_io_completion_cb",
        "spdk_bdev_io_status",
        // Custom ironspdk C ABI
        "SpdkBdevOptsC",
    );

    allowlist_functions!(
        builder,
        // SPDK functions used by ironspdk
        "spdk_thread_create",
        "spdk_thread_send_msg",
        "spdk_get_thread",
        "spdk_thread_get_id",
        "spdk_thread_is_exited",
        "spdk_thread_is_running",
        "spdk_thread_exit",
        "spdk_bdev_desc_get_bdev",
        "spdk_bdev_close",
        "spdk_bdev_io_get_md_buf",
        "spdk_get_io_channel",
        "spdk_bdev_get_md_size",
        "spdk_bdev_is_md_separate",
        "spdk_bdev_get_io_channel",
        "spdk_put_io_channel",
        "spdk_io_channel_ref",
        "spdk_io_channel_get_ref_count",
        "spdk_bdev_io_type_supported",
        "spdk_bdev_get_dif_type",
        "spdk_bdev_get_block_size",
        "spdk_bdev_readv_blocks",
        "spdk_bdev_writev_blocks",
        "spdk_bdev_io_complete",
        "spdk_bdev_free_io",
        "spdk_poller_register",
        "spdk_poller_unregister",
        "spdk_poller_pause",
        "spdk_poller_resume",
        "spdk_cpuset_zero",
        "spdk_cpuset_set_cpu",
        "spdk_env_get_core_count",
        "spdk_dma_malloc",
        "spdk_dma_zmalloc",
        "spdk_dma_free",
        // ironspdk C shim functions
        "u_spdk_app_.*",
        "u_spdk_cpuset_.*",
        "u_spdk_io_channel_.*",
        "u_spdk_bdev_.*",
        "u_io_channel_.*",
        "u_bdev_.*",
        // ironspdk C JSON helpers
        "u_json_.*",
        // ironspdk extern "C" Rust functions
        "rsu_bdev_.*",
        "rsu_io_.*",
    );

    let ffi = builder
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate C FFI");

    ffi.write_to_file(out_dir.join("ffi.rs"))
        .expect("Unable to write C FFI");
}

fn build_shim(spdk_include_dir: &Path) {
    let mut build = cc::Build::new();
    build
        .file("c_src/bdev.c")
        .file("c_src/json.c")
        .file("c_src/shim.c")
        .file("c_src/util.c")
        .flag("-isystem")
        .flag(spdk_include_dir.to_str().unwrap())
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

    let out_dir_path = PathBuf::from(out_dir);
    generate_ffi(&spdk_include_dir, &out_dir_path);

    println!("cargo::rerun-if-changed=c_src/");
    println!("cargo::rerun-if-changed=src/");
    println!("cargo::rerun-if-env-changed=SPDK");
    println!("cargo::rerun-if-env-changed=CARGO_MANIFEST_DIR");
}
