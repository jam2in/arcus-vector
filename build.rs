//! Generates `engine_api.rs` from the arcus-memcached headers at the crate root.

use std::env;
use std::error::Error;
use std::path::{Path, PathBuf};

const ENGINE_HEADER: &str = "include/memcached/engine.h";
const CONFIG_FILES: [&str; 2] = ["config.h", "config_static.h"];
const COPY_HINT: &str = "Copy config.h, config_static.h and include/memcached/*.h from a \
                         configured arcus-memcached tree to the crate root.";

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=include");
    for name in CONFIG_FILES {
        println!("cargo:rerun-if-changed={name}");
    }

    for path in [ENGINE_HEADER].iter().chain(&CONFIG_FILES) {
        if !Path::new(path).is_file() {
            return Err(format!("{path} is missing. {COPY_HINT}").into());
        }
    }

    let out = PathBuf::from(env::var("OUT_DIR")?).join("engine_api.rs");

    bindgen::Builder::default()
        .header(ENGINE_HEADER)
        .clang_arg("-Iinclude")
        .clang_arg("-I.")
        .clang_args(["-include", "config.h"])
        .allowlist_file(".*memcached/.*\\.h")
        .allowlist_var("VERSION")
        .generate()
        .map_err(|e| format!("{}. {COPY_HINT}", e.to_string().trim_end()))?
        .write_to_file(out)?;

    Ok(())
}
