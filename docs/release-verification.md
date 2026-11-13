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
# 1. 八制品：版本 / 注册在线 / 服务端看到的版本 / 能力 / 退出离线 / 子进程清理
DIST=$PWD/dist EXPECT_VERSION=0.59.17 bash tools/artifacts_verify.sh

# 2. 窗口门禁：真实开窗 + 存活 + 正常关闭 + 四个负例
#    不给任何用例会直接 exit 2（拒绝空跑 PASS）。
bash tools/gui_release_window_test.sh \
  --linux-ui dist/shell-remote-ui-x86_64 \
  --expect-fail-no-window         tools/fake_no_window_app.sh \
  --expect-fail-timeout           tools/fake_timeout_app.sh \
  --expect-fail-wrong-pid         tools/fake_foreign_window_app.sh \
  --expect-fail-missing-restore   dist/shell-remote-ui-x86_64

# 3. 真实无参数 GUI 的完整自升级与失败回滚（Linux 原生）
bash tools/gui_upgrade_real_test.sh \
  --ui dist/shell-remote-ui-x86_64 --relay dist/shell-remote-x86_64

# 3b. Windows 完整 GUI 链路（隔离 wine）：开窗 → 连接 → 下载 → 换机 → 回滚
bash tools/win_wine_gui_chain_test.sh \
  --ui dist/shell-remote-ui-x86_64.exe --relay dist/shell-remote-x86_64

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

门禁的判定只看**返回码与原因码**，不看日志里有没有 `✓`：

- `run_window_case` 只在全部断言成立时返回 0，失败时把原因码写进 `RESULT`；
- 负例断言的是"返回非零 **且** 原因码符合预期"；
- 用例内部的断言写用例私有的 `CASE_BAD`，**不碰**全局 `fail`——没有任何
  "把 fail 恢复回去"的分支。上一版正是在负例分支里 `fail=$before`，于是
  "打印了失败却仍然 exit 0"。

| 负例 | 期望原因码 | 实测 |
|---|---|---|
| 不建窗口、直接 exit 0 | `exited_early` | ✓ 拦住 |
| 只 sleep、永不建窗口 | `no_window`（超时不算通过） | ✓ 拦住 |
| 同屏有**别人**的窗口（标题还模仿本程序），本进程不建窗口 | `no_window` | ✓ 拦住 |
| 要求恢复但交接文件缺失 | 非零退出 + stderr 说明原因 | ✓ 拦住 |

负例 3 专门打"按标题/按有没有窗口判成功"这条错路：假程序让**子进程**弹出一个
标题含 `shell-remote` 的窗口，自己一直不建窗口；门禁按 `_NET_WM_PID` 判归属，
因此必须为空。实测该用例同屏可见窗口数为 3，门禁仍然判定本进程没有窗口。

把**真实 GUI** 塞进 `--expect-fail-no-window` 也必须非零退出（这是负责人的原始
复现）；本版实测 `EXIT=1`、输出 `✗ 负例未拦住`。

关闭窗口用 `WM_DELETE_WINDOW` 协议消息（`tools/wm_close_window.py`），**不用**
`xdotool windowclose`——后者发 `XDestroyWindow`，winit 随后访问失效窗口会 panic
（rc=101），那既不是真实用户操作，也会把"崩溃"误当成"关闭"。

## 真实 GUI 自升级与回滚

`tools/gui_upgrade_real_test.sh` 走的是完整链路，不是脚本替身：

无参数启动 GUI → 在窗口里填 relay / 固定密钥 / 会话 ID（**不保存**，保持活跃
未保存配置）→ 点"连接" → relay 用 custom 制品下发升级 → 真实下载 / 校验 /
替换 / 重启 / 注册。

窗口里填的内容（都不是默认值）：

| 字段 | 填入值 | 服务端 / 行为的可观测判据 |
|---|---|---|
| 访问权限 | **both**（"分别生成读写与只读凭据"） | overview 里恰好 1 个 `rw` + 1 个 `ro`；`rw` 的值**等于**填入的固定密钥；`ro` 是独立随机值且非空 |
| 文件工作目录 | `$W/active-root` | 相对路径上传落到该目录下（`/agent/mcp/put?path=<相对路径>`） |
| Shell | `/bin/sh`（非 `default_shell()`） | agent 子进程里 PTY 的 `argv[0]` 是 `/bin/sh` |
| 关闭命令日志 | 勾选 | `$SR_LOG_DIR/shell-remote.log*` 不新增 `[MCP] exec` 行 |

- **both 是真的选出来的**，不是脚本自称：下拉选完立刻从截图读回该组合框的墨迹
  量（默认项"可读写"→ both 项"分别生成读写与只读凭据"，实测 185→608），再与
  relay 侧的 token 形状互相印证。上一版日志里身份只有 `guifixedkey2821=rw`，
  却断言"both 保持"——那是把 rw-only 说成 both。
- **权限真的有区别**：`rw` 能执行命令，`ro` 必须被服务端以
  `Read-only token cannot call shell_remote` 拒绝。只比 token 字符串无法发现
  "两条都成了 rw"。
- **活跃配置未落盘**：点了连接但没点保存 → 磁盘上不存在 `ui-config.json`。
- **`no_cmd_log` 不是空断言**：先要求日志文件本身有 ≥5 行（证明日志管道在工作），
  再要求 MCP 调用不新增 `[MCP] exec` 行。实测把勾选去掉时该断言变红（1→2）。

**活跃配置的断言是一个复用函数** `assert_active_config`，在**连接后、升级后、
回滚后三处各跑一次**，每次逐项覆盖全部三个设置：

| 设置 | 判据（每次都测） |
|---|---|
| shell | PTY 子进程的 `argv[0]` 等于窗口里配置的 `/bin/sh` |
| root | 相对路径上传必须落在配置的 `$W/active-root` 下 |
| no_cmd_log | MCP 调用不得新增 `[MCP] exec` 行，且日志本身 ≥5 行（证明管道在工作） |

> 上一版的注释写"回滚后 shell/root 都验"，实际只验了 shell；`no_cmd_log` 只在
> 连接后查过一次，升级与回滚后完全没查。现已改为同一份判据三处复用，并在文档
> 里按实际行为描述。

为证明这份判据**不是恒真**，测试内置三条负面元测试：把期望 shell 写错、把期望
root 指向别的目录、把命令日志文件藏起来——三种情况下 `assert_active_config`
都必须返回非零。三条都实测变红。

断言（成功换机）：PID 变化、磁盘 hash 变成新制品、身份**逐字节**相同
（session_id + 一 rw 一 ro）、`registered` 回执的 upgrade_id 与本轮一致（不符即
失败，上一版只 echo）、交接文件被消费、恢复后远程命令仍可执行。

断言（失败回滚）：坏制品必须**先能通过换机前的 `--version` 冒烟测试**（否则会在
替换前就被拦下，那是"失败未替换"，不是回滚），再由助手日志证明
`new binary installed → 新版启动失败 → previous binary restored` 三步都发生，
最后磁盘回到旧 hash、同身份可用。**终态必须是 `rolled_back`**——`failed` 不算
回滚通过（它可能意味着新版压根没换上、或旧版也没起来）；本轮 `upgrade_id` 必须
非空且与终态回执一致；回滚后交接文件必须无残留，且助手日志要有
`previous agent restarted`（回滚闭环）。

> 该脚本会 `unset INVOCATION_ID`：常规 CI/容器里进程处在别人的 `*.service`
> cgroup 中且不是 MainPID，产品的 fail-closed 会正确拒绝换机（那是生产安全
> 语义）。非托管形态（容器 / 普通 shell / Windows）才是这条用例要测的路径。

## 清理范围（为什么不再 `pkill -f basename`）

上一版升级脚本在 `cleanup()` 里 `pkill -9 -f "$(basename "$UI")"`，会杀掉同机
**别人的**同名进程；`wineserver -k` 同理，会波及所有 wine 前缀。本版按创建时
记录的信息精确认领：

| 对象 | 认领依据 |
|---|---|
| Xvfb / openbox / relay | 启动时记录的 PID |
| GUI 进程与换机后的新 agent | 记录值 + `/proc/<pid>/exe` 精确等于本测试副本 |
| 助手的 update-apply 副本 | `/proc/<pid>/exe` 精确等于本测试 `HOME` 下的副本 |
| PTY shell / script / 被执行的命令 | 沿 `/proc/<pid>/stat` 的 PPID 求子孙 |
| wine 内部进程（wineserver / winedevice / services.exe） | 进程环境变量 `WINEPREFIX=<本测试前缀>` |

wine 那条不能用 PID 树：实测 wine 启动后会把真正的 Windows 进程 re-parent 给
init（PPID=1），`$!` 只是 loader。唯一可靠的归属标记是本测试自己创建的
`WINEPREFIX` 路径——一次运行里据此认领到 5～10 个 wine 进程。

清理之后要**验证**，不是 sleep 一下就算完：UI 进程必须全部消失（按 exe 路径核验）、
relay 端口必须不可连（relay 离线）、PTY 子孙必须退出。

**独立同名哨兵**：测试开始前在另一个目录起一个 basename 完全相同的无关进程，
整个测试（含错误/超时路径）跑完后它必须仍然存活。实测全程存活。

## 覆盖边界（不声称跑过没跑过的）

| 项目 | Linux x86_64 | Linux aarch64/armv7 | Windows x86_64（wine） |
|---|---|---|---|
| `--version` / 注册 / 能力声明 | ✅ 原生 | ✅ qemu 用户态（配匹配 sysroot） | ✅ wine |
| 服务端 `agent_version` 与本地一致 | ✅ | ✅ | ✅ |
| 退出后会话转 `online=false` | ✅ | ✅ | ✅ |
| 子进程 / wine 前缀清理 | ✅（PTY 子孙） | ✅（PTY 子孙） | ✅（按 WINEPREFIX 认领） |
| 无参数窗口 GUI | ✅ **原生真窗口** | ❌ 未跑（模拟器无显示后端） | ✅ **wine 下真实 Windows GUI 开窗**（`tools/win_wine_gui_chain_test.sh`） |
| 完整自升级 / 回滚（both 身份 + 活跃配置） | ✅ 原生跑通 | ❌ 未跑 | ✅ **wine 下跑通**：下载 → exe 真实替换 → 身份保持 → 坏版本回滚 → hash 恢复 |
| 交接文件 DACL：单 ACE 是当前用户、Everyone 被剔除 | — | — | ✅ wine（`EqualSid` 逐字节比对） |
| 交接文件 DACL：**`SE_DACL_PROTECTED` 继承保护位** | — | — | ❌ **未验证**：wine 丢弃该控制位 |
| 真实桌面抓屏 / AV1 实际编码 | ❌ 未跑 | ❌ 未跑 | ❌ 未跑 |

### 明确不计入 PASS 的项目

1. **ARM 的窗口与自升级**：qemu 用户态没有显示后端，跑不了窗口；自升级需要真实
   进程替换，也没跑。上表 ✅ 只覆盖"能启动、能注册、能力声明正确、能退出"。
2. **wine 上的 DACL 继承保护位**：见 [windows.md](windows.md) 的"DACL 的验证边界"。
   一个完全绕开产品代码的 SDDL 探针在 wine 上 `SetFileSecurityW` 返回成功，
   但 `GetNamedSecurityInfoW` 读回 rc=5，落盘 DACL 还会被追加一个 SYSTEM ACE。
   测试因此现场探测该能力，平台不支持时**只跳过保护位那一条**并打印未覆盖说明。
3. **Windows 测试目标的其余失败**：`cargo test --target x86_64-pc-windows-gnu`
   在 wine 下能**编译并执行**（MYS-2827 修好了两处 Unix-only 编译错误），但仍有
   27 个用例失败，全部因为用例本身依赖 Unix 语义：`/bin/sh` 不存在
   （`agent::exec_sessions`、`agent::shell`）、web 静态资源以 Unix 路径打包
   （`relay::tests`）、`systemd-run` 不存在（`agent::systemd_helper`）、
   `/usr/local/...` 不是绝对路径（`agent::fs`）。这些不在本任务的允许改动范围内，
   如实列出，不计 PASS。
4. **真 Win10**：UAC 提权、DACL 继承、SCM 服务形态都需要负责人在真机复验。
   本文件任何 wine 结论都不构成真机结论。

### 计数口径

各脚本末尾打印的是**断言级**计数（`PASS=<通过断言>/共 <总断言>`），不是"全部需求
已满足"。发布脚本自测通过 ≠ 产品链路通过；八制品跑完 ≠ 真机通过。以本表为准。
