use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let mut include_paths: Vec<PathBuf> = Vec::new();

    // OBS_INCLUDE_PATH — set by CI to point at OBS source header dirs.
    // Semicolon-separated on Windows, colon-separated elsewhere.
    if let Ok(p) = env::var("OBS_INCLUDE_PATH") {
        let sep = if cfg!(windows) { ';' } else { ':' };
        for part in p.split(sep) {
            if !part.is_empty() {
                include_paths.push(PathBuf::from(part));
            }
        }
    }

    // pkg-config (Linux / local dev) — finds libobs headers + link paths
    match pkg_config::Config::new()
        .atleast_version("28.0")
        .probe("libobs")
    {
        Ok(lib) => {
            for path in &lib.include_paths {
                println!("cargo:include={}", path.display());
                include_paths.push(path.clone());
            }
            for path in &lib.link_paths {
                println!("cargo:rustc-link-search=native={}", path.display());
            }
        }
        Err(e) => {
            println!("cargo:warning=pkg-config could not find libobs: {e}");
            println!("cargo:rustc-link-search=native=/usr/lib");
            include_paths.push(PathBuf::from("/usr/include/obs"));
        }
    }

    // If the OBS source tree is present (CI sparse checkout) but is missing
    // the CMake-generated obsconfig.h, create a stub so that obs-config.h
    // can find it.  This is safe — ws_shim.c only uses the websocket API,
    // not the path/feature macros defined in obsconfig.h.
    for p in &include_paths {
        let obs_config_h = p.join("obs-config.h");
        let obsconfig_h = p.join("obsconfig.h");
        if obs_config_h.is_file() && !obsconfig_h.is_file() {
            let stub = r#"/* Auto-generated stub — see build.rs */
#pragma once
#define OBS_RELEASE_CANDIDATE 0
#define OBS_BETA 0
"#;
            let _ = fs::write(&obsconfig_h, stub);
            println!("cargo:warning=Generated obsconfig.h stub at {}", obsconfig_h.display());
        }
    }

    // Compile the C shim that wraps obs-websocket inline API functions
    let mut cc = cc::Build::new();
    cc.file("src/ws_shim.c");
    for p in &include_paths {
        cc.include(p);
        let sub = p.join("obs");
        if sub.is_dir() {
            cc.include(&sub);
        }
    }
    cc.compile("ws_shim");

    println!("cargo:rerun-if-changed=src/ws_shim.c");
    println!("cargo:rerun-if-env-changed=OBS_INCLUDE_PATH");
}
