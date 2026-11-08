fn main() {
    // Older versions of `libc` crate used `RUST_LIBC_UNSTABLE_MUSL_V1_2_3`,
    // but more recent ones use `CARGO_CFG_LIBC_UNSTABLE_MUSL_V1_2_3` instead.
    let musl = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() == "musl";
    let musl_v1_2_3 = std::env::var("CARGO_CFG_LIBC_UNSTABLE_MUSL_V1_2_3").is_ok();
    println!("cargo:rerun-if-env-changed=CARGO_CFG_LIBC_UNSTABLE_MUSL_V1_2_3");
    println!("cargo:rustc-check-cfg=cfg(musl_v1_2_3)");
    if musl && musl_v1_2_3 {
        println!("cargo:rustc-cfg=musl_v1_2_3");
    }

    // The targets on io-uring 0.7 (see Cargo.toml), which has IORING_ENTER_NO_IOWAIT.
    println!("cargo:rustc-check-cfg=cfg(io_uring_no_iowait)");
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "linux"
        && ["x86_64", "aarch64", "riscv64", "loongarch64", "powerpc64"]
            .contains(&target_arch.as_str())
    {
        println!("cargo:rustc-cfg=io_uring_no_iowait");
    }

    println!("cargo:rustc-check-cfg=cfg(syscall_pipe2)");
    println!("cargo:rustc-check-cfg=cfg(syscall_accept4)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if [
        "linux",
        "android",
        "freebsd",
        "illumos",
        "solaris",
        "emscripten",
        "hurd",
        "redox",
        "netbsd",
        "cygwin",
    ]
    .contains(&target_os.as_str())
    {
        println!("cargo:rustc-cfg=syscall_pipe2");
    }
    if [
        "freebsd",
        "netbsd",
        "emscripten",
        "fuchsia",
        "solaris",
        "illumos",
        "linux",
    ]
    .contains(&target_os.as_str())
    {
        println!("cargo:rustc-cfg=syscall_accept4");
    }
}
