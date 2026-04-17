use glob::glob;
use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn get_spdk_pkgcfg_metas(pkgcfg_dir: &str) -> Vec<String> {
    let path = Path::new(pkgcfg_dir);
    if !path.exists() || !path.is_dir() {
        panic!("pkgconfig directory '{}' not found", pkgcfg_dir);
    }

    // we need all .pc files except spdk_syslibs and C++-related
    let pattern = format!("{}/*.pc", pkgcfg_dir);
    let file_names: Vec<String> = glob(&pattern)
        .unwrap()
        .map(|e| {
            e.unwrap()
                .file_stem()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        })
        .collect();
    let metas_to_exclude = &[
        "spdk_syslibs",
        "spdk_trace",
        "spdk_trace_parser",
        "spdk_ut",
        "spdk_ut_mock",
    ];
    let exclude_set: HashSet<_> = metas_to_exclude.iter().cloned().collect();
    let metas: Vec<String> = file_names
        .iter()
        .filter(|name| !exclude_set.contains(name.as_str()))
        .cloned()
        .collect();
    metas
}

fn parse_pkgcfg_tokens(tokens: Vec<String>) {
    for tok in tokens {
        if let Some(lib_path) = tok.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={}", lib_path);
        } else if tok.starts_with("-l") || tok.ends_with(".a") {
            println!("cargo:rustc-link-arg={}", &tok);
        } else if let Some(inc_path) = tok.strip_prefix("-I") {
            println!("cargo:include={}", inc_path);
        }
    }
}

fn invoke_pkgcfg(args: &[&str]) -> Vec<String> {
    let mut cmd = Command::new("pkg-config");
    for arg in args {
        cmd.arg(arg);
    }

    let out = cmd.output().expect("Failed to run pkg-config");

    if !out.status.success() {
        panic!(
            "pkg-config failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let flags = String::from_utf8(out.stdout).expect("Failed to decode pkg-config output");
    flags.split(' ').map(|s| s.to_string()).collect()
}

fn emit_link_spdk(pkgcfg_metas: &[String]) {
    let mut args: Vec<&str> = vec!["--libs"];
    let metas: Vec<&str> = pkgcfg_metas.iter().map(|s| s.as_str()).collect();
    args.extend(metas);
    let spdk_lib_toks = invoke_pkgcfg(args.as_slice());
    parse_pkgcfg_tokens(spdk_lib_toks);

    // System libraries must be linked dynamically
    println!("cargo:rustc-link-arg=-Wl,-Bdynamic");
    println!("cargo:rustc-link-arg=-Wl,--no-whole-archive");
    let sys_lib_tokens = invoke_pkgcfg(&["--libs", "--static", "spdk_syslibs"]);
    parse_pkgcfg_tokens(sys_lib_tokens);

    // Link with liburing (to fix linking bug on some environments)
    println!("cargo:rustc-link-arg=-luring");
}

fn ironspdk_sys_out_dir() -> String {
    env::var("DEP_IRONSPDK_SYS_METADATA").unwrap()
}

pub fn emit_preamble() {
    println!("cargo:rustc-link-arg=-Wl,--no-as-needed");
    println!("cargo:rustc-link-arg=-Wl,-Bstatic");
    println!("cargo:rustc-link-arg=-Wl,--whole-archive");
    let out_dir = ironspdk_sys_out_dir();
    println!("cargo:rustc-link-search=native={}", &out_dir);
    println!("cargo:rustc-link-arg=-lironspdkshim");
}

pub fn emit_rest() {
    let pc_dir =
        env::var("PKG_CONFIG_PATH").expect("PKG_CONFIG_PATH environment variable must be set");
    let pkgcfg_metas = get_spdk_pkgcfg_metas(&pc_dir);
    emit_link_spdk(pkgcfg_metas.as_slice());
}

pub fn resolve_spdk_include() -> PathBuf {
    // try to get SPDK path from environment variable
    if let Ok(dir) = env::var("SPDK") {
        let p = PathBuf::from(dir).join("include");
        if p.exists() {
            println!("cargo:rerun-if-env-changed=SPDK");
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
        "\n\nError: could not find SPDK headers.\n\
         ------------------------------------------------\n\
         Options to fix this:\n\
         1. Set the SPDK environment variable to the root of your SPDK source tree\n\
         2. Install SPDK system-wide and ensure 'pkg-config' can find it\n\
         ------------------------------------------------\n"
    );
}

pub fn include_dir() -> String {
    env::var("DEP_IRONSPDK_SYS_INCLUDE").expect("ironspdk-sys did not export include path")
}
