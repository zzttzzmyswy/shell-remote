fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for entry in std::fs::read_dir("web").unwrap().flatten() {
        println!("cargo:rerun-if-changed={}", entry.path().display());
    }
    // libvpx（VP9 软编）。
    //
    // 链接策略：
    // - 常规开发/本机构建：优先用系统 libvpx（pkg-config）；找不到则报清
    //   晰错误而非静默降级到只有 H.264。
    // - 交叉编译（tools/build-dist.sh）：脚本为每个 target 先编译静态
    //   libvpx 并放在 $LIBVPX_DIR（含 lib/ 与 include/），通过环境变量
    //   LIBVPX_DIR 传入 → 静态链接。
    if let Ok(dir) = std::env::var("LIBVPX_DIR") {
        // <dir>/lib 放 libvpx.a，<dir>/include 放 vpx/*.h
        println!("cargo:rustc-link-search=native={}/lib", dir);
        println!("cargo:rustc-link-lib=static=vpx");
        println!("cargo:rerun-if-env-changed=LIBVPX_DIR");
        return;
    }
    let mut ok = false;
    if let Ok(lib) = pkg_config::Config::new().probe("vpx") {
        for d in &lib.link_paths {
            println!("cargo:rustc-link-search=native={}", d.display());
        }
        println!("cargo:rustc-link-lib=vpx");
        ok = true;
    }
    if !ok {
        println!(
            "cargo:warning=libvpx not found — VP9 desktop encoding disabled; \
             install libvpx-dev or set LIBVPX_DIR for cross builds"
        );
    }
}