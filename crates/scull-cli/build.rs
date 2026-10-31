//! Records the target triple so `scull --version` can print it without runtime work.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=SCULL_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
