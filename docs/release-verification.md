# 发布候选制品的验证（MYS-2821）

本文件记录 **v0.59.17 发布候选八件制品**的验证方式、结论与覆盖边界。每个脚本
都可以用一条命令复跑。

## 复跑全部检查

在仓库根目录，先产出八件制品：

```bash
tools/build-dist.sh          # CLI: musl 静态 ×3 + Windows；Linux UI: GNU/glibc ×3
```

Linux UI 在固定基线容器（Ubuntu 22.04 / glibc 2.35）里构建，单独跑也可以：

```bash
IMAGE=ubuntu:22.04 tools/build-gnu-ui.sh      # 只产出 dist-gnu/ 三个 GNU UI
```

然后：

```bash
# 1. 八制品：版本 / 注册在线 / 能力声明 / 清理（断言，不是打印）
DIST=$PWD/dist EXPECT_VERSION=0.59.17 bash tools/artifacts_verify.sh

# 2. 窗口门禁：真实开窗 + 存活 + 正常关闭 + 两个负例
bash tools/gui_release_window_test.sh \
  --linux-ui dist/shell-remote-ui-x86_64 \
  --expect-fail-no-window tools/fake_no_window_app.sh \
  --expect-fail-timeout   tools/fake_timeout_app.sh

# 3. 真实无参数 GUI 的完整自升级与失败回滚
bash tools/gui_upgrade_real_test.sh \
  --ui dist/shell-remote-ui-x86_64 --relay dist/shell-remote-x86_64

# 4. 40-agent 高频注册
SR_LOG_DIR='' BIN=$PWD/dist/shell-remote-x86_64 N=40 FLAP=10 ROUNDS=5 PORT=39726 \
  bash tools/concurrent_register_smoke.sh

# 5. 三版双向兼容
bash tools/compat_matrix.sh --new $PWD/dist/shell-remote-x86_64 \
  --versions '0.59.8 0.59.15 0.59.16'

# 6. 发布脚本自测
bash tools/release_sh_test.sh
```

## 两种链接形态（为什么 Linux UI 不再是 musl）

| 制品 | 链接形态 | 运行要求 |
|---|---|---|
| CLI `shell-remote-<arch>` (Linux ×3) | musl 静态 | 无依赖，Alpine 也能跑 |
| UI `shell-remote-ui-<arch>` (Linux ×3) | **GNU/glibc 动态** | glibc ≥ 2.35 + libX11 / libxkbcommon / libGL |
| Windows CLI/UI | GNU/mingw 静态 C++ | 无额外运行库 |

窗口 GUI 走 `eframe→winit→x11-dl`，X11 后端用 `dlopen` 加载图形库。musl
静态二进制不支持加载共享对象，窗口必定失败（`Failed to load one of xlib's
shared libraries`）。因此 Linux UI 改为 glibc 动态，并在 Ubuntu 22.04 容器里
构建，保证产物的版本符号不超过 `GLIBC_2.35` / `GLIBCXX_3.4.30`。

**红绿对照**（同一份 GUI 代码，只差链接形态）：

| 形态 | 窗口 | 证据 |
|---|---|---|
| musl 静态（旧发布形态） | **不出现**，退出码 1，`Failed to load one of xlib's shared libraries` | `logs/gui_rootcause.log` |
| GNU/glibc 动态（新发布形态） | **正常出现** 720×660 配置窗口，进程存活，关闭后 rc=0 | `logs/gui_window_gate.log` |

## 窗口门禁的负例

上一版门禁把"进程 exit 0"当成开窗成功，被一个不创建窗口的假程序骗过。现在
的成功判据只能是真实窗口事实（按 PID 归属的可见窗口 + 可截取内容 + 存活 +
WM_DELETE_WINDOW 关闭后自行退出），并且内置两条负例：

| 负例 | 期望 | 实测 |
|---|---|---|
| 不建窗口、直接 exit 0 | 判失败 | ✓ 拦住 |
| 只 sleep、永不建窗口 | 判失败（超时不算通过） | ✓ 拦住 |

关闭窗口用 `WM_DELETE_WINDOW` 协议消息（`tools/wm_close_window.py`），**不用**
`xdotool windowclose`——后者发 `XDestroyWindow`，winit 随后访问失效窗口会 panic
（rc=101），那既不是真实用户操作，也会把"崩溃"误当成"关闭"。

## 真实 GUI 自升级与回滚

`tools/gui_upgrade_real_test.sh` 走的是完整链路，不是脚本替身：

无参数启动 GUI → 在窗口里填 relay / 固定密钥 / 会话 ID（**不保存**，保持活跃
未保存配置）→ 点"连接" → relay 用 custom 制品下发升级 → 真实下载 / 校验 /
替换 / 重启 / 注册。

断言（成功换机）：PID 变化、磁盘 hash 变成新制品、服务端 session_id 与 tokens
（both 权限）不变、`registered` 回执的 upgrade_id 与本轮一致、交接文件被消费、
恢复后远程命令仍可执行。

断言（失败回滚）：坏制品必须**先能通过换机前的 `--version` 冒烟测试**（否则会在
替换前就被拦下，那是"失败未替换"，不是回滚），再由助手日志证明
`new binary installed → 新版启动失败 → previous binary restored` 三步都发生，
最后磁盘回到旧 hash、同身份可用。

> 该脚本会 `unset INVOCATION_ID`：常规 CI/容器里进程处在别人的 `*.service`
> cgroup 中且不是 MainPID，产品的 fail-closed 会正确拒绝换机（那是生产安全
> 语义）。非托管形态（容器 / 普通 shell / Windows）才是这条用例要测的路径。

## 覆盖边界（不声称跑过没跑过的）

| 项目 | Linux x86_64 | Linux aarch64/armv7 | Windows x86_64 |
|---|---|---|---|
| `--version` / 注册 / 能力声明 | 原生 | qemu 用户态（配匹配 sysroot） | wine |
| 窗口 GUI | **原生真窗口** | 未跑（模拟器无显示后端） | wine 下出现配置窗口 |
| 完整自升级 / 回滚 | **原生跑通** | 未跑 | wine 下未跑完整换机 |
| 真实桌面抓屏 / AV1 实际编码 | 未跑 | 未跑 | 未跑 |

ARM 与 Windows 的窗口、以及 ARM 的自升级未实跑；真实 Win10 的 UAC 提权与 DACL
行为仍需负责人在真机复验。
