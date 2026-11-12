#!/bin/bash
# 用**固定基线的 glibc 构建环境**产出 Linux GNU UI 制品（MYS-2821）。
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
# 用法:
#   tools/build-gnu-ui.sh                  # 在本机用容器构建三个 GNU UI 目标
#   tools/build-gnu-ui.sh --inside         # 由上面的容器调用（内部使用）
#   IMAGE=ubuntu:22.04 tools/build-gnu-ui.sh
#
# 环境变量:
#   CACHE_DIR    构建缓存（默认 $HOME/.cache/shell-remote-dist）
#   OUT_DIR      产物输出目录（默认 <repo>/dist-gnu）
#   IMAGE        构建镜像（默认 ubuntu:22.04）
#   FORCE_LIBAOM 1 = 强制重建 libaom（默认复用已有静态库）
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CACHE="${CACHE_DIR:-$HOME/.cache/shell-remote-dist}"
OUT_DIR="${OUT_DIR:-$ROOT/dist-gnu}"
IMAGE="${IMAGE:-ubuntu:22.04}"
GLIBC_MAX="2.35"
GLIBCXX_MAX="3.4.30"

# GNU UI 的三个目标：架构 + 对应的交叉工具链前缀。
GNU_PLATFORMS=(
  "x86_64-unknown-linux-gnu||x86_64-linux-gnu"
  "aarch64-unknown-linux-gnu|aarch64-linux-gnu-|aarch64-linux-gnu"
  "armv7-unknown-linux-gnueabihf|arm-linux-gnueabihf-|arm-linux-gnueabihf"
)

# ── 容器内执行体 ────────────────────────────────────────────────────
if [ "${1:-}" = "--inside" ]; then
  shift || true
  echo "容器基线：$( . /etc/os-release; echo "$PRETTY_NAME" ) / $(ldd --version | head -1)"
  export PATH="$HOME/.cargo/bin:$PATH"
  mkdir -p "$OUT_DIR"
  fail=0
  for entry in "${GNU_PLATFORMS[@]}"; do
    IFS='|' read -r target prefix triple <<<"$entry"
    echo
    echo "== GNU UI: $target =="
    cc="${prefix}gcc"; cxx="${prefix}g++"; ar="${prefix}ar"
    if [ -z "$prefix" ]; then cc=gcc; cxx=g++; ar=ar; fi
    command -v "$cc" >/dev/null || { echo "缺少交叉编译器 $cc"; exit 2; }

    # libaom：每个 GNU 目标单独一份静态库，**不能**复用 musl 缓存
    # （两者 ABI 不同，混用会链出无法运行的产物）。
    aom_dir="$CACHE/libaom-$target"
    # 已有且没被要求重建时才复用。
    if [ ! -f "$aom_dir/lib/libaom.a" ] || [ "${FORCE_LIBAOM:-0}" = "1" ]; then
      echo "-- 构建 libaom ($target) --"
      aom_src="$CACHE/libaom-src"
      if [ ! -d "$aom_src" ]; then
        mkdir -p "$aom_src"
        if [ ! -f "$CACHE/libaom.tar.gz" ]; then
          curl -sL -o "$CACHE/libaom.tar.gz" \
            "https://aomedia.googlesource.com/aom/+archive/v3.15.0.tar.gz"
        fi
        tar xzf "$CACHE/libaom.tar.gz" -C "$aom_src"
      fi
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
        || { echo "libaom 构建失败（$target）：看 $aom_dir/build/cmake.log / make.log"; exit 3; }
      # AV1 必须真的进来：缺库就失败，绝不静默降级到只有 H.264。
      [ -f "$aom_dir/lib/libaom.a" ] || { echo "libaom.a 未产出（$target）"; exit 3; }
      echo "  libaom OK: $(stat -c%s "$aom_dir/lib/libaom.a") bytes"
    else
      echo "-- 复用已有 libaom ($target) --"
    fi

    echo "-- 构建 shell-remote-ui ($target) --"
    # -C target-feature=-crt-static：GNU 目标要动态链接 glibc，
    #   否则 rustc 默认把 crt 静态进来，winit 的 dlopen 又回到死路。
    # -C linker=$cc 必须显式给：`.cargo/config.toml` 只给 musl 目标配了
    #   rust-lld，GNU 目标会退回宿主 `cc`（x86_64 的 gcc），交叉链接直接失败。
    tdir="${CARGO_TARGET_DIR:-$ROOT/target}"
    env CC="$cc" CXX="$cxx" AR="$ar" LIBXAOM_DIR="$aom_dir" \
        RUSTFLAGS="-C target-feature=-crt-static -C linker=$cc \
          -C link-arg=-Wl,-rpath,\$ORIGIN -C link-arg=-Wl,--enable-new-dtags" \
        cargo build --release --locked --bin shell-remote-ui \
        --target "$target" --manifest-path "$ROOT/Cargo.toml" \
        2>&1 | tail -5

    bin="$tdir/$target/release/shell-remote-ui"
    [ -f "$bin" ] || { echo "构建未产出 $bin"; exit 4; }
    name="shell-remote-ui-$(case "$target" in x86_64*) echo x86_64;; aarch64*) echo aarch64;; armv7*) echo armv7;; esac)"
    # 先写临时文件再 rename：产物目录可能被别的进程（例如正在轮询的验证脚本）
    # 同时观察，直接 cp 会让他们读到写了一半的文件。
    cp "$bin" "$OUT_DIR/.$name.tmp"
    chmod +x "$OUT_DIR/.$name.tmp"
    mv -f "$OUT_DIR/.$name.tmp" "$OUT_DIR/$name"
    echo "  产出 $name ($(stat -c%s "$OUT_DIR/$name") bytes)"

    # ABI 门禁：版本符号不得超过基线。
    echo "-- ABI 检查（不得超过 GLIBC_$GLIBC_MAX / GLIBCXX_$GLIBCXX_MAX）--"
    ver="$(readelf --version-info "$OUT_DIR/$name" 2>/dev/null)"
    worst_glibc="$(printf '%s' "$ver" | grep -oE 'GLIBC_[0-9][0-9.]*' | sed 's/GLIBC_//' | sort -V | tail -1)"
    worst_cxx="$(printf '%s' "$ver" | grep -oE 'GLIBCXX_[0-9][0-9.]*' | sed 's/GLIBCXX_//' | sort -V | tail -1)"
    echo "  GLIBC 最高要求=${worst_glibc:-none}  GLIBCXX 最高要求=${worst_cxx:-none}"

    # 比较：把"上限"和"实测"一起 sort -V，谁排最后谁更大。
    # 相等或实测更小都算通过（GLIBCXX_3.4 这种短版本号也必须正确判为 ≤ 3.4.30）。
    exceeds(){ [ "$1" = "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -1)" ] && [ "$1" != "$2" ]; }
    if [ -z "${worst_glibc:-}" ]; then
      echo "  ✗ 没有读到任何 GLIBC 版本符号（不是动态链接产物？）"; fail=1
    elif exceeds "$worst_glibc" "$GLIBC_MAX"; then
      echo "  ✗ 需要 GLIBC_${worst_glibc}，高于基线 GLIBC_${GLIBC_MAX}"; fail=1
    else
      echo "  ✓ GLIBC 不高于基线"
    fi
    if [ -n "${worst_cxx:-}" ]; then
      if exceeds "$worst_cxx" "$GLIBCXX_MAX"; then
        echo "  ✗ 需要 GLIBCXX_${worst_cxx}，高于基线 GLIBCXX_${GLIBCXX_MAX}"; fail=1
      else
        echo "  ✓ GLIBCXX 不高于基线"
      fi
    fi
  done
  [ $fail -eq 0 ] && echo && echo "GNU UI 构建 PASS" || { echo "GNU UI 构建 FAIL"; exit 1; }
  exit 0
fi

# ── 宿主编排：起容器、跑上面的 --inside ─────────────────────────────
echo "构建基线镜像：$IMAGE（要求 glibc $GLIBC_MAX）"
docker run --rm \
  -v "$ROOT:/src:ro" \
  -v "$ROOT/target:/build-target" \
  -v "$CACHE:/cache" \
  -v "$OUT_DIR:/out" \
  -e CARGO_TARGET_DIR=/build-target \
  -e CACHE_DIR=/cache -e OUT_DIR=/out -e ROOT=/src \
  -w /src \
  "$IMAGE" bash -lc '
    set -e
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq build-essential cmake nasm curl ca-certificates \
      gcc-aarch64-linux-gnu g++-aarch64-linux-gnu \
      gcc-arm-linux-gnueabihf g++-arm-linux-gnueabihf \
      libclang-dev pkg-config >/dev/null
    if ! command -v cargo >/dev/null; then
      curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal >/dev/null
    fi
    export PATH="$HOME/.cargo/bin:$PATH"
    if ! rustup target list --installed | grep -qx aarch64-unknown-linux-gnu; then
      rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
        armv7-unknown-linux-gnueabihf >/dev/null
    fi
    # manifest-path 指向只读挂载的 /src，产物落 /build-target
    bash /src/tools/build-gnu-ui.sh --inside
  '
echo
echo "产物目录：$OUT_DIR"
ls -l "$OUT_DIR"
