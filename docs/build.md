# 编译与交叉编译

本仓库发布 4 个平台的 musl 静态单文件二进制：`x86_64` / `aarch64` / `armv7`（Linux musl）与 `x86_64`（Windows GNU）。每个平台产出 CLI 与 UI 两个二进制。

## 前置依赖

自 v0.19.0 起 agent 通过 `openh264-sys2` 的 `source` feature 内联编译 OpenH264 的 C++ 源码，因此 UI 构建需要：

1. 可用的 C++ 编译器（`g++` / `clang++`）
2. 目标平台的 C 链接器（Linux 用 `musl-gcc`，Windows 用 `x86_64-w64-mingw32-g++`）
3. **不需要** NASM / yasm

```bash
sudo apt install g++
rustup target add x86_64-unknown-linux-musl
rustup target add aarch64-unknown-linux-musl
rustup target add armv7-unknown-linux-musleabihf
sudo apt install musl-tools
sudo apt install gcc-aarch64-linux-gnu gcc-arm-linux-gnueabihf
```

Fedora 用 `sudo dnf install musl-gcc`，Alpine 用 `apk add musl-dev`。

## 两个二进制的构建方式

v0.51.0 起拆分为两个二进制：

- **CLI**（`shell-remote`，relay + agent 仅终端）：`--no-default-features`，不编译任何桌面依赖
- **UI**（`shell-remote-ui`，agent 终端 + 桌面）：`--bin shell-remote-ui`，`desktop` feature 默认开启

`shell-remote-ui` 在 `Cargo.toml` 里声明了 `required-features = ["desktop", "tui", "gui"]`，lean 构建时 cargo 自动跳过该 bin。

## musl 目标的静态 C++ 标准库

OpenH264 是 C++ 源码，musl 静态二进制也要链 C++ 标准库。发行版交叉 sysroot 里只有 **glibc 编译**的 `libstdc++.a`，它会带进 glibc 独有的符号（`__sprintf_chk`、`__libc_single_threaded`、`__isoc23_strtoul`），静态链到 musl 上会失败。

仓库的 `tools/glibc-cxx-stubs.c` 就是这些符号的桩实现（`__vfprintf_chk` / `__vsnprintf_chk` / `__snprintf_chk` / `__sprintf_chk`、`__str{cat,strcpy,ncat,ncpy}_chk`、`__mem{move,cpy,set}_chk` 各自转发到无 `_chk` 版本，另有 `int __libc_single_threaded = 1;` 与 `__isoc23_strtoul` → `strtoul`）。`tools/build-dist.sh` 会用目标的 `-gcc` 把它编成 `stub.o` 并与目标的 `libstdc++.a` 合并进缓存目录，再以 `-L` 指向该目录链接。

手工复现的做法（以 aarch64 为例）：

```bash
L=$(aarch64-linux-gnu-g++ -print-file-name=libstdc++.a)
mkdir -p ~/.cache/glibc-stubs/aarch64/x
cd ~/.cache/glibc-stubs/aarch64
ar x "$L" --output=x
aarch64-linux-gnu-gcc -c -o x/stub.o <仓库>/tools/glibc-cxx-stubs.c
ar rc libstdc++.a x/*.o
```

## 逐平台构建命令

```bash
cargo build --release --target x86_64-unknown-linux-musl --no-default-features
cargo build --release --target x86_64-unknown-linux-musl --bin shell-remote-ui
```

```bash
CC=aarch64-linux-gnu-gcc cargo build --release --target aarch64-unknown-linux-musl --no-default-features
CC=aarch64-linux-musl-gcc CXX=aarch64-linux-musl-g++ AR=aarch64-linux-musl-ar \
  cargo build --release --target aarch64-unknown-linux-musl --bin shell-remote-ui
```

```bash
CC=arm-linux-gnueabihf-gcc cargo build --release --target armv7-unknown-linux-musleabihf --no-default-features
CC=arm-linux-musleabihf-gcc CXX=arm-linux-musleabihf-g++ AR=arm-linux-musleabihf-ar \
  cargo build --release --target armv7-unknown-linux-musleabihf --bin shell-remote-ui
```

Windows 交叉编译见 [windows.md](windows.md)。

> aarch64 / armv7 的 **UI** 构建必须用 musl.cc 的真 musl 工具链（`aarch64-linux-musl-*` / `arm-linux-musleabihf-*`）：UI 里 str0m 引入的 `aws-lc-sys` 与 CLI 里 ring 的 C 代码交叉编译加链接都要求真 musl。`tools/build-dist.sh` 默认从 `~/.cache/muslcc-aarch64/bin/` 与 `~/.cache/muslcc-armv7/bin/` 取工具链，可用 `MUSLCC_AARCH64` / `MUSLCC_ARMV7` 覆盖前缀。

## 打包与发布脚本

```bash
tools/build-dist.sh
```

一次产出全部 4 平台、每平台 2 个二进制到 `dist/`：

```
dist/shell-remote-<arch>[.exe]      CLI（relay + agent 终端，lean）
dist/shell-remote-ui-<arch>[.exe]   UI（agent 终端 + 桌面）
```

脚本会自动处理静态 libaom（若 `~/.cache/libaom-<target>` 存在则加 `LIBXAOM_DIR`，否则构建产物不含 AV1）、libstdc++ 合并、aarch64/armv7 的空 `libdl.a` stub，以及 Windows 的 `requireAdministrator` manifest 嵌入（中间产物写到仓库外的缓存目录）。

```bash
tools/release.sh
```

发布：版本号以 `Cargo.toml` 为唯一源，脚本据此打 tag、重建 `dist/` 并创建 GitHub release。`--skip-build` 复用已有 `dist/`。CI 与人工发布都走这个入口，避免 tag 与二进制 `--version` 漂移。

```bash
tools/build-libaom.sh
```

为各发布平台交叉编译静态 `libaom.a`（AV1 软编需要，`build.rs` 经 `LIBXAOM_DIR` 链接）。

## 验证构建产物

```bash
file dist/shell-remote-*
```

输出应显示 `statically linked`。再确认两个二进制的能力差异：

```bash
tools/smoke_split.sh
```
