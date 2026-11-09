fn main() {
    // The rustc target triple of this build (`x86_64-unknown-linux-gnu`) —
    // `cargo:rustc-env` exposes it to the lib as `env!("TAMIAS_TARGET_TRIPLE")`
    // so `drivers/bin/<triple>/` selection needs no runtime rustc probe.
    println!(
        "cargo:rustc-env=TAMIAS_TARGET_TRIPLE={}",
        std::env::var("TARGET").expect("cargo always sets TARGET for build scripts")
    );
}
