fn main() {
    // Expose the build target triple to source so `squeezy doctor` can report
    // which artifact the user is running.
    println!(
        "cargo:rustc-env=SQUEEZY_TARGET_TRIPLE={}",
        std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string())
    );
}
