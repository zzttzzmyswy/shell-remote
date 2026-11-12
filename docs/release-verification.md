# 发布候选制品的验证（MYS-2821）

本文件记录 **v0.59.17 发布候选八件制品**的真实验证方式与结论边界，以及一个
已定位、**阻塞 Linux GUI 窗口验收**的制品形态缺陷。

复跑全部检查（在仓库根目录，制品先由 `tools/build-dist.sh` 产出到 `dist/`）：

```bash
# 1. 八制品启动 / 注册 / 能力声明（原生 + qemu + wine）
DIST=$PWD/dist bash tools/artifacts_verify.sh

# 2. 真实窗口（Linux 制品 + Windows 制品）与无参数入口的 fail-closed
bash tools/gui_release_window_test.sh \
  --linux-ui dist/shell-remote-ui-x86_64 \
  --windows-ui dist/shell-remote-ui-x86_64.exe

# 3. 40-agent 高频注册
SR_LOG_DIR='' BIN=$PWD/dist/shell-remote-x86_64 N=40 FLAP=10 ROUNDS=5 PORT=39726 \
  bash tools/concurrent_register_smoke.sh

# 4. 三版双向兼容
bash tools/compat_matrix.sh --new $PWD/dist/shell-remote-x86_64 \
  --versions '0.59.8 0.59.15 0.59.16'

# 5. 发布脚本自测
bash tools/release_sh_test.sh
```

## 已知缺陷：Linux 发布 UI 制品打不开窗口

**现象**：`dist/shell-remote-ui-x86_64`（以及同形态的 aarch64/armv7 制品）在
有 `DISPLAY` 的环境里无参数启动，窗口不出现，退出码 1：

```text
Error: 窗口初始化失败：winit EventLoopError: os error at
  .../winit-0.30.13/src/platform_impl/linux/mod.rs:788:
  Failed to load one of xlib's shared libraries
```

**根因**（可复现的纯 C 对照）：发布制品是 **musl 静态（static-pie）** 二进制，
而 eframe→winit→`x11-dl` 在运行时用 `dlopen` 加载 libX11 / libxkbcommon / libGL。
musl 静态二进制**不支持** `dlopen` 共享对象（`Dynamic loading not supported`）；
即使在 glibc 主机上，那些库是 glibc 构建的，引用的 `__*_chk` / `__isoc23_*`
符号在 musl 进程里也不存在。

```console
$ # 与制品同一种链接方式
$ musl-gcc -static-pie -o sp d.c -ldl && ./sp
libX11.so.6        FAIL  Dynamic loading not supported
$ # 与 cargo test 的 debug 二进制同一种链接方式
$ gcc -o dyn d.c -ldl && ./dyn
libX11.so.6        OK
```

同一份 GUI 代码用 glibc 动态链接构建（`cargo build --bin shell-remote-ui`，即
`cargo test`/负责人阶段5 用的 debug 二进制）**能正常开窗**，因此这是**制品链接
形态**的缺陷，不是 GUI 代码缺陷。该缺陷自引入窗口 GUI 的 v0.53.0 起就存在，
v0.59.16 的已发布 UI 制品同样复现。

**影响**：Linux 上所有发布 UI 制品都无法显示窗口，而这正是 "UI 版无参数启动、
所有配置通过窗口完成" 这条需求的载体。

**未在本任务修复**：任务卡禁止改运行时代码/依赖/构建配置，要求"若发现源码/构建
问题，先以失败证据报告"。修复需要改依赖（改用纯 Rust 的 `x11rb` 后端或
`x11-dl` 的静态链接方式）或改发布形态，属于设计决策。

**Windows 制品不受影响**：`shell-remote-ui-x86_64.exe` 在 wine 下能正常创建
配置窗口（`tools/gui_release_window_test.sh` 的 B 段断言了这一点）。

## 覆盖边界（不声称跑过没跑过的）

| 项目 | Linux x86_64 | Linux aarch64/armv7 | Windows x86_64 |
|---|---|---|---|
| `--version` | 原生 | qemu 用户态 | wine |
| agent 注册 + 能力声明 | 原生 | qemu 用户态 | wine |
| **窗口 GUI** | **失败（见上）** | 未跑 | **wine 下通过** |
| 桌面抓屏 / AV1 实际编码 | 未跑（无桌面内容） | 未跑 | 未跑 |

qemu / wine 只证明"进程能启动、能注册、能力声明正确"，**不证明**真实桌面抓屏与
编码性能。Windows 真实 Win10 环境的 UAC 提权行为、DACL 交接文件创建
（`SetFileSecurityW`）在 wine 下不完整，仍需真机验证。
