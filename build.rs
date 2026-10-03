use std::env;

fn main() {
    println!("cargo::rerun-if-env-changed=CFG_COMPILER_HOST_TRIPLE");
    let host_triple = env::var("CFG_COMPILER_HOST_TRIPLE")
        .or_else(|_| env::var("TARGET"))
        .unwrap_or_default();
    println!("cargo::rustc-env=CFG_COMPILER_HOST_TRIPLE={host_triple}");
}
