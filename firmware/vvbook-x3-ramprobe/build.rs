use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::copy("ram.x", out.join("ram.x")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rustc-link-arg=-Tram.x");
    println!("cargo:rerun-if-changed=ram.x");
    println!("cargo:rerun-if-changed=build.rs");
}
