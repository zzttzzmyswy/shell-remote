# 编译与交叉编译

本仓库发布 4 个平台的**两种链接形态**，每个平台各产出 CLI 与 UI 两个二进制：

| 制品 | 平台 | 链接形态 |
|---|---|---|
| CLI `shell-remote-<arch>` | Linux x86_64 / aarch64 / armv7 | **musl 静态**（任何发行版直接跑，含 Alpine） |
| CLI `shell-remote-x86_64.exe` | Windows x86_64 | GNU/mingw，静态 C++ |
| UI `shell-remote-ui-<arch>` | Linux x86_64 / aarch64 / armv7 | **GNU/glibc 动态**（Ubuntu 22.04 / glibc 2.35 基线） |
| UI `shell-remote-ui-x86_64.exe` | Windows x86_64 | GNU/mingw，GUI subsystem，静态 C++ |

**Linux UI 为什么不能是 musl 静态**（MYS-2821）：窗口 GUI 走
`eframe→winit→x11-dl`，在运行时 `dlopen` libX11 / libxkbcommon / libGL。
musl 静态二进制不支持加载共享对象，窗口必定失败：

```text
Error: 窗口初始化失败: ... Failed to load one of xlib's shared libraries
```

同一份代码用 glibc 动态链接构建就能正常开窗。因此 Linux UI 必须动态链接
glibc，代价是需要 glibc ≥ 2.35 与 X11/GL/XKB 运行库，**不能在 Alpine/musl 上
直接运行**。CLI 不受影响。

## 前置依赖

自 v0.19.0 起 agent 通过 `openh264-sys2` 的 `source` feature 内联编译 OpenH264 的 C++ 源码，因此 UI 构建需要：

1. 可用的 C++ 编译器（`g++` / `clang++`）
2. 目标平台的 C 链接器（Linux CLI 用 `musl-gcc`；Linux UI 用对应的 GNU
   `gcc`，在基线容器里；Windows 用 `x86_64-w64-mingw32-g++`）
3. **不需要** NASM / yasm（libaom 构建才需要 nasm）
4. **Linux UI 需要 docker**：构建在固定基线的 Ubuntu 22.04 容器里跑，保证
   产物的 GLIBC 要求不超过 2.35

```bash
sudo apt install g++
rustup target add x86_64-unknown-linux-musl
rustup target add aarch64-unknown-linux-musl
rustup target add armv7-unknown-linux-musleabihf
rustup target add x86_64-unknown-linux-gnu
rustup target add aarch64-unknown-linux-gnu
rustup target add armv7-unknown-linux-gnueabihf
sudo apt install musl-tools
```

Fedora 用 `sudo dnf install musl-gcc`，Alpine 用 `apk add musl-dev`——这两条只
解决**宿主上编 CLI** 的工具链。UI 的三个 GNU 目标不由宿主发行版决定：它们在
Ubuntu 22.04 基线容器里编译（宿主是什么发行版都一样，只需能跑 docker）。

## 两个二进制的构建方式

v0.51.0 起拆分为两个二进制：

- **CLI**（`shell-remote`，relay + agent 仅终端）：`--no-default-features`，不编译任何桌面依赖
- **UI**（`shell-remote-ui`，agent 终端 + 桌面）：`--bin shell-remote-ui`，`desktop` feature 默认开启

`shell-remote-ui` 在 `Cargo.toml` 里声明了 `required-features = ["desktop", "tui", "gui"]`，lean 构建时 cargo 自动跳过该 bin。

## 静态 C++ 标准库（Windows UI）

OpenH264 是 C++ 源码，因此 UI 构建要链 C++ 标准库。**Linux CLI 是
`--no-default-features`，根本不含 OpenH264/桌面代码**，所以 musl 静态 CLI 里
没有任何 C++ 依赖，不需要打桩。真正需要处理的是 **Windows UI**：mingw 默认把
`-lstdc++` 解析到动态导入库 `libstdc++-6.dll.a`，exe 运行时就会报"找不到
libstdc++-6.dll"（用户实拍 MYS-886）。

`tools/build-dist.sh` 的做法是两条双保险：`-static-libstdc++`，外加把 mingw 的
静态 `libstdc++.a` 解包后与 `tools/glibc-cxx-stubs.c` 编出的 `stub.o` 合并进
`$CACHE/winui-x86_64/libstdc++.a`，再用 `-L` 指向该目录。桩实现补齐 mingw
静态库里缺失的 glibc 风格符号（`__vfprintf_chk` / `__vsnprintf_chk` /
`__snprintf_chk` / `__sprintf_chk`、`__str{cat,strcpy,ncat,ncpy}_chk`、
`__mem{move,cpy,set}_chk` 各自转发到无 `_chk` 版本，另有
`int __libc_single_threaded = 1;` 与 `__isoc23_strtoul` → `strtoul`）。

手工复现的做法：

```bash
L=$(x86_64-w64-mingw32-g++ -print-file-name=libstdc++.a)
mkdir -p ~/.cache/glibc-stubs/win-x86_64/x
cd ~/.cache/glibc-stubs/win-x86_64
(cd x && x86_64-w64-mingw32-ar x "$L")
x86_64-w64-mingw32-gcc -O2 -c -o stub.o <仓库>/tools/glibc-cxx-stubs.c
x86_64-w64-mingw32-ar rc libstdc++.a x/*.o stub.o
```

## 逐平台构建命令

> 下面这些命令都是**单独**编某一个目标的写法，用于本地调试。发布构建请直接跑
> `tools/build-dist.sh`（见后文）——它按下面的形态组装八件产物并做门禁。
> 注意 Linux UI 的三条命令必须在 **Ubuntu 22.04 基线容器**里执行：在宿主
> 发行版直接编出来的 glibc 版本符号可能高于 2.35，产物在老系统上跑不起来。

Linux x86_64 CLI（musl 静态）：

```bash
cargo build --release --target x86_64-unknown-linux-musl --no-default-features
```

Linux x86_64 UI（GNU/glibc 动态，需要基线容器）：

```bash
tools/build-gnu-ui.sh          # 三个 GNU UI 目标，在 ubuntu:22.04 容器里构建
```

aarch64 CLI（musl 静态）与 UI（GNU/glibc 动态，容器内）：

```bash
CC=aarch64-linux-gnu-gcc cargo build --release --target aarch64-unknown-linux-musl --no-default-features
# UI：容器内由 tools/build-gnu-ui.sh 交叉编译 aarch64-unknown-linux-gnu
```

armv7 CLI（musl 静态）与 UI（GNU/glibc 动态，容器内）：

```bash
CC=arm-linux-gnueabihf-gcc cargo build --release --target armv7-unknown-linux-musleabihf --no-default-features
# UI：容器内由 tools/build-gnu-ui.sh 交叉编译 armv7-unknown-linux-gnueabihf
```

Windows 交叉编译见 [windows.md](windows.md)。

> **CLI 才用 musl 工具链**。aarch64 / armv7 的 CLI 必须用 musl.cc 的真 musl
> 工具链（`aarch64-linux-musl-*` / `arm-linux-musleabihf-*`）：CLI 里 ring 的 C
> 代码交叉编译与链接要求真 musl。`tools/build-dist.sh` 默认从
> `~/.cache/muslcc-aarch64/bin/` 与 `~/.cache/muslcc-armv7/bin/` 取工具链，可用
> `MUSLCC_AARCH64` / `MUSLCC_ARMV7` 覆盖前缀。
>
> **UI 不再用 musl**（MYS-2821）。Linux UI 的三个目标（`*-unknown-linux-gnu` /
> `-gnueabihf`）由 `tools/build-gnu-ui.sh` 在 Ubuntu 22.04 基线容器里用 GNU
> 交叉工具链编译，产物是 glibc 动态链接。`build-gnu-ui.sh` 会把容器镜像摘要、
> glibc 版本与**固定的 Rust 工具链版本**（`GNU_RUST_TOOLCHAIN`，默认 1.98.0）
> 记到 `dist-gnu/RUST-TOOLCHAIN.txt`，避免每次构建临时拉取不同的 stable。
> Windows UI 仍是 mingw 静态形态（Windows 不需要 dlopen glibc 共享库）。

## 打包与发布脚本

```bash
tools/build-dist.sh
```

一次产出全部 4 平台、每平台 2 个二进制到 `dist/`：

```
dist/shell-remote-<arch>[.exe]      CLI（relay + agent 终端，lean）
dist/shell-remote-ui-<arch>[.exe]   UI（agent 终端 + 桌面）
dist/BUILD-INFO.txt                 本次构建记录（源码 commit、工具链、来源、大小、SHA256）
```

脚本在**落盘前**对八件产物逐件门禁，任一件不通过就整体非零退出、`dist/` 保持
原样不动：

| 门禁 | 判据 |
|---|---|
| 存在 | 目标文件存在且非空（否则报出是哪个 target 目录缺文件） |
| 版本 | 文件内字符串含 `Cargo.toml` 的 `version`（前后不接数字/小数点） |
| 架构 | `readelf -h` 的 Machine / PE 头的 `x86-64` |
| 链接形态 | Linux CLI 无 `PT_INTERP`（静态）；**Linux UI 必须有 `PT_INTERP`**（动态，防止 static musl UI 冒充） |
| AV1 | 四件 UI（含 Windows）都必须含 `aom_codec_av1_cx`；lean CLI 必须不含 |
| Windows | UI 为 GUI subsystem 且嵌 `requireAdministrator` manifest；CLI 为 console |

几个容易踩的点，脚本已经内建：

- **取件目录 = cargo 实际使用的目录**。脚本读 `CARGO_TARGET_DIR`（cargo 同样
  遵守），不会出现"标称从 `target/` 取件、实际是另一个目录的旧文件"。
- **Windows 的静态 libaom 是开工前置**。`$CACHE/libaom-x86_64-pc-windows-gnu/lib/libaom.a`
  不存在时脚本在编译任何东西之前就失败，不再产出"静默无 AV1"的 Windows UI。
- **落盘是整批 staging + rename**。八件先复制进 `dist/.staging.XXXXXX/`，全部
  成功后才逐个 `mv` 进 `dist/`；复制中途失败（磁盘满、权限）只会清理 staging，
  `dist/` 仍是上一版，不会出现"新旧混合却看起来齐全"。
- **不清理已有 target 里的历史产物**。musl 形态的 UI 旧构建（`target/*/release/
  shell-remote-ui`）留在原地不影响发布——取件路径只认 GNU UI 目录，且 UI 门禁
  要求动态链接。
- `SKIP_GNU_UI=1` 复用 `dist-gnu/` 里已有的 GNU UI（仅调试用），**门禁照旧**。
- Linux UI 构建（`tools/build-gnu-ui.sh`）在 ABI 门禁**通过之后**才把文件发布
  到 `dist-gnu/`：`GLIBC ≤ 2.35`、`GLIBCXX ≤ 3.4.30`、interpreter 与架构匹配、
  `NEEDED` 含 `libc.so.6`、含 AV1 符号。libaom 的下载/解压走"先校验再原子
  rename"，损坏或中断不会留下被下次误用的半成品缓存。

```bash
tools/build_contract_test.sh
```

构建合同回归测试：用假 cargo / 假交叉工具链 + 真实 `readelf`/`strings`/`file` 探测
合成 ELF/PE 制品，在临时沙箱里覆盖上面每一条门禁与失败传播（缺件、版本/架构错、
静态 UI 冒充、Windows 缺 AV1、subsystem/manifest 错、`CARGO_TARGET_DIR` 覆盖、
staging 失败、半成品 libaom 缓存等）。不编译真实代码、不联网、不碰真实
`target/` 与 `dist/`。

```bash
tools/release.sh
```

发布：版本号以 `Cargo.toml` 为唯一源，脚本据此打 tag、重建 `dist/` 并创建 GitHub release。`--skip-build` 复用已有 `dist/`。CI 与人工发布都走这个入口，避免 tag 与二进制 `--version` 漂移。

发布是**先验证、后动手**：下列任一条件不满足，脚本在任何 tag / release 创建之前就会失败退出，不会留下"tag 已推出、release 没建出来"的半成品。

- 工作树必须干净（`dist/` 本身在 `.gitignore` 里，不算脏）；
- `dist/` 八个制品齐全（CLI + UI × 四平台）；
- 本地与远端都不得已存在同名 tag（脚本绝不 `tag -f` / `push -f`：强推已发布 tag 会让"某版本的字节"与 tag 指向的源码永久失配）；
- 不得已存在同名 release。

要重发某个版本，应 bump `Cargo.toml` 的 `version` 后发布；不要覆盖既有 tag/release。推送 tag 或创建 release 未成功确认时，脚本保留已有引用并停止。先核实远端 tag、release 和资产的实际状态；仅补传缺失且已校验的资产，不使用 `--clobber`。结果未知时不要重跑发布脚本。

`tools/release_sh_test.sh` 用隔离沙箱（本地 bare 仓库当 origin + 假的 `gh` 与 `build-dist`）覆盖上述每一条断言，不接触真实仓库或 GitHub：

```bash
tools/release_sh_test.sh
```

**发版前必须跑跨版本兼容矩阵，并保证全部 PASS/GAP（不允许 FAIL）**：

```bash
tools/compat_matrix.sh
```

它用真实的已发布旧二进制跑「新 relay + 旧 agent」与「旧 relay + 新 agent」两个方向，
见 [compat-matrix.md](compat-matrix.md)。有 FAIL 时退出码非 0，不要发版。

```bash
tools/build-libaom.sh
```

为各发布平台交叉编译静态 `libaom.a`（AV1 软编需要，`build.rs` 经 `LIBXAOM_DIR` 链接）。

## 验证构建产物

```bash
file dist/shell-remote-*
```

期望（不是"全部 statically linked"）：

- Linux **CLI** 三件：`ELF ... statically linked`（musl 静态，含 Alpine 直接跑）
- Linux **UI** 三件：`ELF ... dynamically linked, interpreter /lib64/ld-linux-x86-64.so.2`
  （或 aarch64 的 `/lib/ld-linux-aarch64.so.1`、armv7 的 `/lib/ld-linux-armhf.so.3`）
- Windows CLI：`PE32+ ... (console)`；Windows UI：`PE32+ ... (GUI)`

`tools/build-dist.sh` 已经把这些形态连同版本/架构/AV1/manifest 一起做成了门禁
（见上文表格），单看 `file` 只是为了人工复核。再确认两个二进制的能力差异：

```bash
tools/smoke_split.sh
```
