#!/bin/bash
# 用**固定基线的 glibc 构建环境**产出 Linux GNU UI 制品（MYS-2821 / MYS-2828）。
#
# 为什么要有这个脚本：Linux UI 不能用 musl 静态构建。
# eframe→winit→`x11-dl` 在运行时 `dlopen` libX11 / libxkbcommon / libGL，
# 而 musl 静态二进制不支持加载共享对象（`Dynamic loading not supported`）；
# 即使宿主是 glibc，那些库引用的 `__*_chk` / `__isoc23_*` 在 musl 进程里也不
# 存在。所以 Linux UI 改为 GNU/glibc 动态运行时，CLI 仍保留 musl 静态。
#
# 基线固定为 **Ubuntu 22.04 / glibc 2.35**：在更新的发行版（例如滚动版的
# glibc 2.4x）上构建会悄悄引入更高的版本符号，产物在老系统上直接跑不起来。
# 因此构建必须在与基线一致的容器里做，并且产物要能被 `readelf --version-info`
# 证明"最高要求不超过 GLIBC_2.35 / GLIBCXX_3.4.30"。
#
# Rust 工具链固定（MYS-2828）：容器每次都是全新的，旧版用 `--default-toolchain
# stable` 会在不同日子拉到不同的 stable，同一个 commit 构建出不同字节、也可能
# 悄悄跨过 MSRV。这里 pin 到一个确切版本，并把实际版本与镜像摘要一起记录到
# OUT_DIR/RUST-TOOLCHAIN.txt。**不升级任何产品依赖**（Cargo.lock 不动）。
#
# 用法:
#   tools/build-gnu-ui.sh                  # 在本机用容器构建三个 GNU UI 目标
#   tools/build-gnu-ui.sh --inside         # 由上面的容器调用（内部使用）
#   IMAGE=ubuntu:22.04 tools/build-gnu-ui.sh
#
# 环境变量:
#   CACHE_DIR    构建缓存（默认 $HOME/.cache/shell-remote-dist）
#   OUT_DIR      产物输出目录（默认 <repo>/dist-gnu）
#   IMAGE        构建镜像（默认 ubuntu:22.04）
#   CARGO_TARGET_DIR  cargo target 目录（默认 <repo>/target-gnu，
#                     容器内固定为挂载点 /build-target）
#   GNU_RUST_TOOLCHAIN 容器内固定的 Rust 版本（默认 1.98.0）
#   FORCE_LIBAOM 1 = 强制重建 libaom（默认复用已有静态库）
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CACHE="${CACHE_DIR:-$HOME/.cache/shell-remote-dist}"
OUT_DIR="${OUT_DIR:-$ROOT/dist-gnu}"
IMAGE="${IMAGE:-ubuntu:22.04}"
GLIBC_MAX="2.35"
GLIBCXX_MAX="3.4.30"
# 宿主侧 GNU target 目录：与宿主 musl/Windows 的 target/ 分开，避免不同
# rustc 版本的增量缓存互相失效重建；也避免容器（root）写出 root 所有的
# cargo 状态文件把宿主下一次构建挡住。
GNU_TARGET_DIR="${GNU_CARGO_TARGET_DIR:-${CARGO_TARGET_DIR:-$ROOT/target-gnu}}"
RUST_TOOLCHAIN="${GNU_RUST_TOOLCHAIN:-1.98.0}"

# GNU UI 的三个目标：架构 + 对应的交叉工具链前缀 + 期望的 ELF interpreter。
GNU_PLATFORMS=(
  "x86_64-unknown-linux-gnu||x86_64-linux-gnu|/lib64/ld-linux-x86-64.so.2"
  "aarch64-unknown-linux-gnu|aarch64-linux-gnu-|aarch64-linux-gnu|/lib/ld-linux-aarch64.so.1"
  "armv7-unknown-linux-gnueabihf|arm-linux-gnueabihf-|arm-linux-gnueabihf|/lib/ld-linux-armhf.so.3"
)

# ── libaom 源缓存：下载/解压失败绝不留半成品 ────────────────────────
#
# 旧版直接 `tar xzf $CACHE/libaom.tar.gz -C $aom_src`：curl 中途失败留下的
# 截断 tar.gz 会让 tar 失败，但目录里可能已经躺着一半源码；下次 `[ ! -d
# "$aom_src" ]` 判为"已存在"直接复用 → 半成品缓存被当成有效依赖，构建出的
# libaom 静默缺功能（甚至缺 AV1）。这里的做法：先下载到 .part → 用 gzip -t
# 校验完整性 → 解压到临时目录 → 成功后原子 rename 到位；任一步失败都删掉
# 临时产物，不留下任何可被下次误用的东西。
ensure_libaom_src() { # $1 = cache dir → echo 源码目录
  local cache="$1"
  local src="$cache/libaom-src"
  local tarball="$cache/libaom.tar.gz"

  # 结构校验：能当 libaom 源码用的最低条件。旧轮次留下的缓存没有标记文件，
  # 但结构完整就照常复用（"旧缓存可读复制"），不强制重新解压。
  libaom_src_ok() {
    [ -f "$1/CMakeLists.txt" ] && [ -f "$1/aom/aom_codec.h" ] \
      && [ -f "$1/aom/aomcx.h" ]
  }

  if libaom_src_ok "$src"; then
    [ -f "$src/.extracted-ok" ] || : > "$src/.extracted-ok" 2>/dev/null || true
    echo "$src"; return 0
  fi
  if [ -d "$src" ]; then
    echo "-- 已有 libaom-src 结构不完整（缺 CMakeLists.txt / aom/aom_codec.h），重新解压 --" >&2
    rm -rf "$src"
  fi
  mkdir -p "$cache"

  if [ -f "$tarball" ] && ! gzip -t "$tarball" 2>/dev/null; then
    echo "-- 已有 libaom.tar.gz 校验失败（截断/损坏），重新下载 --" >&2
    rm -f "$tarball"
  fi
  if [ ! -f "$tarball" ]; then
    echo "-- 下载 libaom v3.15.0 --" >&2
    if ! curl -fsSL --retry 3 --retry-delay 2 -o "$tarball.part" \
        "https://aomedia.googlesource.com/aom/+archive/v3.15.0.tar.gz"; then
      rm -f "$tarball.part"
      echo "ERROR: libaom 下载失败（无半成品缓存残留）" >&2
      return 1
    fi
    if ! gzip -t "$tarball.part" 2>/dev/null; then
      rm -f "$tarball.part"
      echo "ERROR: libaom 下载内容不是有效 gzip（无半成品缓存残留）" >&2
      return 1
    fi
    mv -f "$tarball.part" "$tarball"
  fi

  local tmp="$src.tmp.$$"
  rm -rf "$tmp"
  mkdir -p "$tmp"
  if ! tar xzf "$tarball" -C "$tmp"; then
    rm -rf "$tmp"
    echo "ERROR: libaom 解压失败（已清理临时目录，$tarball 保留供重试）" >&2
    return 1
  fi
  if ! libaom_src_ok "$tmp"; then
    rm -rf "$tmp"
    echo "ERROR: libaom 解压后结构异常（缺 CMakeLists.txt / aom/aom_codec.h），已清理" >&2
    return 1
  fi
  : > "$tmp/.extracted-ok"
  rm -rf "$src"
  mv -f "$tmp" "$src"
  echo "$src"
}

# ── 容器内执行体 ────────────────────────────────────────────────────
if [ "${1:-}" = "--inside" ]; then
  shift || true
  echo "容器基线：$( . /etc/os-release; echo "$PRETTY_NAME" ) / $(ldd --version | head -1)"
  echo "Rust 工具链（固定）：$RUST_TOOLCHAIN"
  export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
  mkdir -p "$OUT_DIR"
  fail=0
  for entry in "${GNU_PLATFORMS[@]}"; do
    IFS='|' read -r target prefix triple want_interp <<<"$entry"
    echo
    echo "== GNU UI: $target =="
    tgt_fail=0
    cc="${prefix}gcc"; cxx="${prefix}g++"; ar="${prefix}ar"
    if [ -z "$prefix" ]; then cc=gcc; cxx=g++; ar=ar; fi
    command -v "$cc" >/dev/null || { echo "缺少交叉编译器 $cc"; exit 2; }

    # libaom：每个 GNU 目标单独一份静态库，**不能**复用 musl 缓存
    # （两者 ABI 不同，混用会链出无法运行的产物）。
    aom_dir="$CACHE/libaom-$target"
    # 已有且没被要求重建时才复用。缺 libaom.a 时必须重建——不能带着
    # "目录存在就算有"的假设继续（那会让 build.rs 静默降级到无 AV1）。
    if [ ! -f "$aom_dir/lib/libaom.a" ] || [ "${FORCE_LIBAOM:-0}" = "1" ]; then
      echo "-- 构建 libaom ($target) --"
      aom_src="$(ensure_libaom_src "$CACHE")" || exit 3
      rm -rf "$aom_dir"; mkdir -p "$aom_dir/lib" "$aom_dir/include" "$aom_dir/build"
      aom_cpu=$(case "$target" in x86_64*) echo x86_64;; aarch64*) echo arm64;; armv7*) echo armv7;; esac)
      # armv7 的 NEON intrinsic 需要显式 ABI/FPU 选项：交叉 gcc 默认的
      # -mfloat-abi 与 neon intrinsic 的 "always_inline ... target specific
      # option mismatch" 会对不上（与 musl 那版 libaom 用同一组参数）。
      aom_extra=()
      if [ "$aom_cpu" = "armv7" ]; then
        aom_extra=(-DCMAKE_C_FLAGS="-mfpu=neon -mfloat-abi=hard"
                   -DCMAKE_CXX_FLAGS="-mfpu=neon -mfloat-abi=hard")
      fi
      ( cd "$aom_dir/build" && cmake "$aom_src" \
          -DCMAKE_BUILD_TYPE=Release \
          -DCMAKE_INSTALL_PREFIX="$aom_dir" \
          -DCMAKE_SYSTEM_NAME=Linux \
          -DCMAKE_SYSTEM_PROCESSOR="$aom_cpu" \
          -DCMAKE_C_COMPILER="$cc" -DCMAKE_CXX_COMPILER="$cxx" \
          -DENABLE_DOCS=0 -DENABLE_EXAMPLES=0 -DENABLE_TESTDATA=0 -DENABLE_TESTS=0 \
          -DENABLE_TOOLS=0 -DBUILD_SHARED_LIBS=0 \
          -DCONFIG_AV1_ENCODER=1 -DCONFIG_AV1_DECODER=0 -DCONFIG_MULTITHREAD=1 \
          -DAOM_TARGET_CPU="$aom_cpu" -DENABLE_NASM=ON \
          "${aom_extra[@]}" > cmake.log 2>&1 \
        && make -j"$(nproc)" > make.log 2>&1 \
        && make install >> make.log 2>&1 ) \
        || { echo "libaom 构建失败（$target）：看 $aom_dir/build/cmake.log / make.log" >&2; rm -rf "$aom_dir"; exit 3; }
      # AV1 必须真的进来：缺库就失败，绝不静默降级到只有 H.264。
      [ -f "$aom_dir/lib/libaom.a" ] || { echo "libaom.a 未产出（$target）" >&2; rm -rf "$aom_dir"; exit 3; }
      echo "  libaom OK: $(stat -c%s "$aom_dir/lib/libaom.a") bytes"
    else
      echo "-- 复用已有 libaom ($target) --"
    fi

    echo "-- 构建 shell-remote-ui ($target) --"
    tdir="${CARGO_TARGET_DIR:-$ROOT/target}"
    bin="$tdir/$target/release/shell-remote-ui"
    name="shell-remote-ui-$(case "$target" in x86_64*) echo x86_64;; aarch64*) echo aarch64;; armv7*) echo armv7;; esac)"
    # 关键（MYS-2828）：先删掉上一次的同名产物与本目标目录里的旧二进制。
    # 否则 cargo 若因任何原因提前失败，后面 `[ -f "$bin" ]` 会看到**上一轮**
    # 的文件并通过门禁，等于把旧制品当成本次构建结果发出去。
    rm -f "$OUT_DIR/$name" "$OUT_DIR/.$name.tmp" "$bin"
    # -C target-feature=-crt-static：GNU 目标要动态链接 glibc，
    #   否则 rustc 默认把 crt 静态进来，winit 的 dlopen 又回到死路。
    # -C linker=$cc 必须显式给：`.cargo/config.toml` 只给 musl 目标配了
    #   rust-lld，GNU 目标会退回宿主 `cc`（x86_64 的 gcc），交叉链接直接失败。
    # 链接失败/编译失败都直接非零退出（旧版 `| tail -5` 会吞掉退出码）。
    if ! env CC="$cc" CXX="$cxx" AR="$ar" LIBXAOM_DIR="$aom_dir" \
        RUSTFLAGS="-C target-feature=-crt-static -C linker=$cc \
          -C link-arg=-Wl,-rpath,\$ORIGIN -C link-arg=-Wl,--enable-new-dtags" \
        cargo build --release --locked --bin shell-remote-ui \
        --target "$target" --manifest-path "$ROOT/Cargo.toml" > "$tdir/build-$target.log" 2>&1; then
      echo "ERROR: cargo build 失败（$target）：$tdir/build-$target.log" >&2
      tail -20 "$tdir/build-$target.log" >&2
      exit 4
    fi

    [ -f "$bin" ] || { echo "构建未产出 $bin" >&2; exit 4; }

    # ABI 门禁：版本符号不得超过基线；interpreter / NEEDED 必须是动态 glibc 形态。
    # 注意顺序：**门禁通过后**才把文件发布到 OUT_DIR。旧版先 mv 再检查，
    # 失败的产物已经躺在 OUT_DIR 里，下一步取件就会把它当成功结果。
    echo "-- ABI 检查（不得超过 GLIBC_$GLIBC_MAX / GLIBCXX_$GLIBCXX_MAX）--"
    ver="$(readelf --version-info "$bin" 2>/dev/null)"
    worst_glibc="$(printf '%s' "$ver" | grep -oE 'GLIBC_[0-9][0-9.]*' | sed 's/GLIBC_//' | sort -V | tail -1)"
    worst_cxx="$(printf '%s' "$ver" | grep -oE 'GLIBCXX_[0-9][0-9.]*' | sed 's/GLIBCXX_//' | sort -V | tail -1)"
    interp="$(readelf -l "$bin" 2>/dev/null | grep -m1 'Requesting program interpreter' | sed -E 's/.*: (.*)\]/\1/' | tr -d ' \r')"
    printf '  interpreter=%s\n  GLIBC 最高要求=%s  GLIBCXX 最高要求=%s\n' \
      "${interp:-<无>}" "${worst_glibc:-none}" "${worst_cxx:-none}"

    # 比较：把"上限"和"实测"一起 sort -V，谁排最后谁更大。
    # 相等或实测更小都算通过（GLIBCXX_3.4 这种短版本号也必须正确判为 ≤ 3.4.30）。
    exceeds(){ [ "$1" = "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -1)" ] && [ "$1" != "$2" ]; }
    if [ -z "${worst_glibc:-}" ]; then
      echo "  ✗ 没有读到任何 GLIBC 版本符号（不是动态链接产物？）" >&2; tgt_fail=1
    elif exceeds "$worst_glibc" "$GLIBC_MAX"; then
      echo "  ✗ 需要 GLIBC_${worst_glibc}，高于基线 GLIBC_${GLIBC_MAX}" >&2; tgt_fail=1
    else
      echo "  ✓ GLIBC 不高于基线"
    fi
    if [ -n "${worst_cxx:-}" ]; then
      if exceeds "$worst_cxx" "$GLIBCXX_MAX"; then
        echo "  ✗ 需要 GLIBCXX_${worst_cxx}，高于基线 GLIBCXX_${GLIBCXX_MAX}" >&2; tgt_fail=1
      else
        echo "  ✓ GLIBCXX 不高于基线"
      fi
    fi
    if [ "${interp:-}" != "$want_interp" ]; then
      echo "  ✗ interpreter=${interp:-<无>}，应为 $want_interp（不是 GNU 动态产物）" >&2; tgt_fail=1
    else
      echo "  ✓ interpreter 正确（$want_interp）"
    fi
    # heredoc 而非管道：pipefail 下 grep -q 提前退出的 SIGPIPE 会造成假阴性。
    if ! grep -q 'NEEDED.*libc\.so\.6' <<<"$(readelf -d "$bin" 2>/dev/null)"; then
      echo "  ✗ NEEDED 里没有 libc.so.6（不是 glibc 动态链接）" >&2; tgt_fail=1
    fi
    if ! grep -q 'aom_codec_av1_cx' <<<"$(strings -a "$bin")"; then
      echo "  ✗ 缺少 AV1 编码器符号（aom_codec_av1_cx）" >&2; tgt_fail=1
    fi
    if [ "$tgt_fail" -ne 0 ]; then
      echo "  ✗ $name 未通过门禁，不发布到 $OUT_DIR" >&2
      fail=1
      continue
    fi

    # 先写临时文件再 rename：产物目录可能被别的进程（例如正在轮询的验证脚本）
    # 同时观察，直接 cp 会让他们读到写了一半的文件。
    cp "$bin" "$OUT_DIR/.$name.tmp"
    chmod +x "$OUT_DIR/.$name.tmp"
    mv -f "$OUT_DIR/.$name.tmp" "$OUT_DIR/$name"
    echo "  产出 $name ($(stat -c%s "$OUT_DIR/$name") bytes)"
  done

  # 工具链与基线记录（可追溯：同一个 commit 才能复现同一份产物）。
  {
    echo "# Linux GNU UI 构建环境（tools/build-gnu-ui.sh 生成）"
    echo "image:           $IMAGE"
    echo "image_digest:    $(cat /.sr-image-digest 2>/dev/null || echo unknown)"
    echo "os:              $( . /etc/os-release; echo "$PRETTY_NAME" )"
    echo "glibc:           $(ldd --version | head -1)"
    echo "rust_toolchain:  $RUST_TOOLCHAIN"
    echo "rustc:           $(rustc --version 2>/dev/null || echo unknown)"
    echo "cargo:           $(cargo --version 2>/dev/null || echo unknown)"
    echo "gcc:             $(gcc --version | head -1)"
    echo "aarch64-gcc:     $(aarch64-linux-gnu-gcc --version | head -1)"
    echo "armv7-gcc:       $(arm-linux-gnueabihf-gcc --version | head -1)"
    echo "libaom:          v3.15.0（本容器内 cmake 构建，$CACHE/libaom-<target>）"
  } > "$OUT_DIR/RUST-TOOLCHAIN.txt"

  if [ $fail -eq 0 ]; then
    echo
    echo "GNU UI 构建 PASS"
    exit 0
  fi
  echo "GNU UI 构建 FAIL" >&2
  exit 1
fi

# ── 宿主编排：起容器、跑上面的 --inside ─────────────────────────────
mkdir -p "$GNU_TARGET_DIR" "$OUT_DIR"
# 镜像摘要记录下来：同一个 tag 的镜像内容可能变，摘要不会。
image_digest="$(docker image inspect --format '{{index .RepoDigests 0}}' "$IMAGE" 2>/dev/null || echo unknown)"
echo "构建基线镜像：$IMAGE（要求 glibc $GLIBC_MAX；$image_digest）"
echo "GNU target 目录：$GNU_TARGET_DIR"
echo "固定 Rust 工具链：$RUST_TOOLCHAIN"

# Rust 工具链与 cargo registry 走持久缓存：容器每次全新，不缓存的话每次都要
# 重新下载 ~150MB 工具链。缓存目录归属 root（容器内用户），仅作下载缓存。
mkdir -p "$CACHE/rustup-gnu" "$CACHE/cargo-gnu"

# 每次构建前清掉上一次的"环境记录"，避免构建失败时残留旧记录被当成本次结果。
rm -f "$OUT_DIR/RUST-TOOLCHAIN.txt"

docker run --rm \
  -v "$ROOT:/src:ro" \
  -v "$GNU_TARGET_DIR:/build-target" \
  -v "$CACHE:/cache" \
  -v "$OUT_DIR:/out" \
  -v "$CACHE/rustup-gnu:/rustup" \
  -v "$CACHE/cargo-gnu:/cargo" \
  -e CARGO_TARGET_DIR=/build-target \
  -e CACHE_DIR=/cache -e OUT_DIR=/out -e ROOT=/src \
  -e RUSTUP_HOME=/rustup -e CARGO_HOME=/cargo \
  -e RUST_TOOLCHAIN="$RUST_TOOLCHAIN" \
  -e IMAGE="$IMAGE" \
  -e IMAGE_DIGEST="$image_digest" \
  -w /src \
  "$IMAGE" bash -lc '
    set -e
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq build-essential cmake nasm curl ca-certificates \
      gcc-aarch64-linux-gnu g++-aarch64-linux-gnu \
      gcc-arm-linux-gnueabihf g++-arm-linux-gnueabihf \
      libclang-dev pkg-config >/dev/null
    export PATH="/cargo/bin:$PATH"
    # 固定版本安装（不是 stable）：同一个 commit 任何一天构建都得到同一工具链。
    if ! rustup toolchain list 2>/dev/null | grep -q "^$RUST_TOOLCHAIN"; then
      curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain "$RUST_TOOLCHAIN" --profile minimal >/dev/null
    fi
    rustup default "$RUST_TOOLCHAIN" >/dev/null
    rustup target add --toolchain "$RUST_TOOLCHAIN" \
      x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
      armv7-unknown-linux-gnueabihf >/dev/null
    echo "$IMAGE_DIGEST" > /.sr-image-digest
    # manifest-path 指向只读挂载的 /src，产物落 /build-target
    bash /src/tools/build-gnu-ui.sh --inside
  '

echo
echo "产物目录：$OUT_DIR"
ls -l "$OUT_DIR"
