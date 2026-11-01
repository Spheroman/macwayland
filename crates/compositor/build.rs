//! Tells the linker where libxkbcommon is. Smithay's `xkbcommon` crate links
//! `-lxkbcommon` without searching for it, and on macOS it's in no default
//! linker path. In order: `XKBCOMMON_LIB_DIR`, then `pkg-config`, then the
//! usual Nix and Homebrew locations.

use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=XKBCOMMON_LIB_DIR");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let has_lib = |dir: &PathBuf| dir.join("libxkbcommon.dylib").exists();

    if let Some(dir) = env::var_os("XKBCOMMON_LIB_DIR") {
        println!("cargo:rustc-link-search=native={}", PathBuf::from(dir).display());
        return;
    }
    if let Ok(out) = Command::new("pkg-config").args(["--libs-only-L", "xkbcommon"]).output()
        && out.status.success()
    {
        let flags = String::from_utf8_lossy(&out.stdout);
        let dirs: Vec<_> = flags.split_whitespace().filter_map(|f| f.strip_prefix("-L")).collect();
        if !dirs.is_empty() {
            for dir in dirs {
                println!("cargo:rustc-link-search=native={dir}");
            }
            return;
        }
    }
    let mut candidates = Vec::new();
    if let Ok(user) = env::var("USER") {
        candidates.push(PathBuf::from(format!("/etc/profiles/per-user/{user}/lib")));
    }
    if let Some(home) = env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".nix-profile/lib"));
    }
    candidates.extend(["/run/current-system/sw/lib", "/opt/homebrew/lib", "/usr/local/lib"].map(PathBuf::from));
    match candidates.iter().find(|d| has_lib(d)) {
        Some(dir) => println!("cargo:rustc-link-search=native={}", dir.display()),
        None => println!(
            "cargo:warning=libxkbcommon not found: install it (Nix: libxkbcommon; Homebrew: \
             brew install libxkbcommon) or set XKBCOMMON_LIB_DIR to the directory holding libxkbcommon.dylib"
        ),
    }
}
