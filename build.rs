fn link_lib(name: &str, env_key: &str) {
    if let Ok(dir) = std::env::var(env_key) {
        // <dir>/lib 放 lib*.a，<dir>/include 放头文件
        println!("cargo:rustc-link-search=native={}/lib", dir);
        println!("cargo:rustc-link-lib=static={}", name);
        println!("cargo:rerun-if-env-changed={}", env_key);
        return;
    }
    let mut ok = false;
    if let Ok(lib) = pkg_config::Config::new().probe(name) {
        for d in &lib.link_paths {
            println!("cargo:rustc-link-search=native={}", d.display());
        }
        println!("cargo:rustc-link-lib={}", name);
        ok = true;
    }
    if !ok {
        println!(
            "cargo:warning={} not found — related desktop encoding disabled; \
             install lib{}-dev or set {} for cross builds",
            name, name, env_key
        );
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for entry in std::fs::read_dir("web").unwrap().flatten() {
        println!("cargo:rerun-if-changed={}", entry.path().display());
    }
    // libvpx（VP9 软编）+ libaom（AV1 软编）。
    //
    // 链接策略：
    // - 常规开发/本机构建：优先用系统库（pkg-config）；找不到则报清除错误
    //   而非静默降级到只有 H.264。
    // - 交叉编译（tools/build-dist.sh）：脚本为每个 target 先编译静态
    //   库并放在 $LIBVPX_DIR / $LIBXAOM_DIR（含 lib/ 与 include/），
    //   通过对应环境变量传入 → 静态链接。
    #[cfg(feature = "vp9")]
    link_lib("vpx", "LIBVPX_DIR");
    #[cfg(feature = "av1")]
    link_lib("aom", "LIBXAOM_DIR");
}