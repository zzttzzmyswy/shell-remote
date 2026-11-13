# Windows Agent

系统要求：Windows 10 1809+（ConPTY 最低版本）。

## 安装

一行安装并运行（relay 地址自动注入）：

```powershell
irm http://your-relay:3000/agent/install.ps1 | iex
```

仅下载到当前目录不执行：

```powershell
& ([scriptblock]::Create((irm http://your-relay:3000/agent/install.ps1))) --download-only
```

也可以手动下载 release 中的 `shell-remote-x86_64.exe` 并重命名为 `shell-remote.exe`。安装脚本只发布 `x86_64.exe` 一个资产——ARM64 Windows 11 通过 x64 模拟运行它。

## 启动

带界面的版本 `shell-remote-ui-x86_64.exe` 直接双击启动，无需命令行参数。在窗口中填写 Relay 地址、可选固定密钥和会话 ID；Shell、文件根目录、命令日志和桌面参数位于“高级设置”。点击“连接”开始，断开后可以修改设置再连接。

“保存设置”写入用户目录下的 `.shell-remote\ui-config.json`（优先使用 `HOME`，否则使用 `%USERPROFILE%`），不保存固定密钥。普通启动只加载设置，不自动连接；由自升级或回滚触发的重启会恢复升级前的活动配置与身份。关闭窗口会断开并清理本次连接。

不带界面的 CLI 版本继续使用：

```powershell
shell-remote.exe agent --relay-url http://your-relay:3000 --key YourFixedKey
```

### 选择 Shell

`--shell` 与 `--relay-url` 之外的参数与 Linux 一致（见 [install.md](install.md)）。Windows 上 `--shell` 默认 `cmd.exe`，也可指定 `powershell.exe`。此外 MCP 的 `shell_remote` 工具在 Windows 上有独立的 `shell` 参数，可选 `auto`（默认）/ `powershell` / `cmd`，**只在 Windows 生效、Unix 上被忽略**：`auto` 表示有 PowerShell 用 PowerShell、否则 cmd。详见 [mcp.md](mcp.md)。

```powershell
shell-remote.exe agent --relay-url http://your-relay:3000 --key YourFixedKey --shell powershell.exe
```

## 注意事项

- 建议以管理员身份运行以完整访问文件系统（`tools/build-dist.sh` 给 UI 二进制嵌入 `requireAdministrator` manifest，让键鼠注入能操作提权窗口）
- 交互式程序（ssh / vim）在 MCP exec 路径不支持；文件下载（read）正常
- 终端输出与 MCP 命令结果**按行**逐行判定编码后统一转换为 UTF-8 回传：合法 UTF-8 的行原样保留，其余行按系统 ANSI 代码页（中文 Windows 为 GBK / cp936）解码——同一条命令里 PowerShell 输出的 UTF-8 与原生程序的 GBK 可以共存。跨块被切开的多字节字符由流式解码器拼接，不会出现半个汉字。浏览器与 AI Agent 侧无需关心目标机编码
- **命令日志会记录全部终端输入**：Windows 的 ConPTY 拿不到 Unix 那样的 termios 与前台进程组信息，无法判断"是否在密码提示下"，因此 Windows agent 不区分密码输入。在 Windows 上处理敏感操作时请加 `--no-cmd-log`。详见 [mcp.md](mcp.md)

## 交叉编译（从 Linux）

```bash
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
cargo build --release --bin shell-remote-ui --target x86_64-pc-windows-gnu
```

需要 `x86_64-w64-mingw32-gcc` / `-g++` / `-ar`（mingw-w64）。完整的打包口径由 `tools/build-dist.sh` 处理，其中有几处 Windows 特有的必要设置：

- **静态 C++ 运行时**：不加 `-static-libstdc++` 与静态 `libstdc++.a` 的 `-L` 时，`-lstdc++` 会落到 mingw 的动态导入库 `libstdc++-6.dll.a`，exe 运行时报"找不到 libstdc++-6.dll"
- **manifest 嵌入**：`x86_64-w64-mingw32-windres -I windows/ windows/embed-manifest.rc -O coff -o <cache>/agent_manifest.o`，再用 `-C link-args=<cache>/agent_manifest.o` 链入；中间产物写在仓库外的缓存目录。`windows/embed-manifest.rc` 按相对路径引用同目录的 `windows/agent.manifest`
- **`crt-static`**：`-C target-feature=+crt-static`
- **libaom**：若 `~/.cache/libaom-x86_64-pc-windows-gnu` 存在则经 `LIBXAOM_DIR` 链接，否则该产物不含 AV1

## 功能对比

| 功能 | Linux | Windows |
|------|-------|---------|
| PTY 交互式 Shell | ✅（portable-pty） | ✅（ConPTY） |
| 命令执行 | ✅ | ✅（cmd / PowerShell） |
| 文件浏览 / 读写 / 改名 / 删除 | ✅ | ✅ |
| 文件下载（read） | ✅ | ✅ |
| 文件上传（upload） | ✅ | ✅ |
| 交互式程序（ssh / vim） | ✅ | ⚠️ MCP exec 路径不支持 |
| 文件权限位（mode） | ✅ | 显示占位（无 POSIX 权限） |
| 桌面捕获 | ✅ X11（Wayland 需 `--features wayland`） | ✅ DXGI → GDI 回退 |
| 登录 shell 环境（`/etc/profile`） | ✅ | 不适用（无该概念） |
| 命令日志的密码输入过滤 | ✅（termios + 前台进程组） | ❌ 记录全部输入 |
| 升级的 `--version` 冒烟测试 | ✅ | 跳过（以 SHA-256 为准） |
| 换机助手（原子自升级） | ✅ | ⚠️ wine 下走通下载/校验/替换/注册链路；交接文件 DACL **只部分验证**（见下），Win10 真机仍待复验 |

### 交接文件 DACL 的验证边界

`src/agent/update_apply.rs` 的 `restrict_to_current_user()` 用 SDK 的
`SECURITY_DESCRIPTOR`（x64 上 40 字节，`Dacl` 在偏移 32）构造单 ACE DACL，
并请求 `PROTECTED_DACL_SECURITY_INFORMATION`。修复前那里是一个手写的
`[u8; 32]` 缓冲区，`InitializeSecurityDescriptor` 会写出界 8 字节，踩坏栈上
相邻数据，后续 `SetFileSecurityW` 报 998（ERROR_NOACCESS）。

在 **wine-11.21** 上跑 `cargo test --target x86_64-pc-windows-gnu` 的
`windows_handoff_dacl` 三个用例，实测能验证的与不能验证的分列如下：

| 项目 | wine 下 | 真 Windows | 判据 |
|---|---|---|---|
| `SECURITY_DESCRIPTOR` 尺寸/`Dacl` 偏移 | ✅ 已验证 | ✅ 必过 | 40 / 32，编译期常量断言 |
| 写交接文件不报 998、内容可读回 | ✅ 已验证 | ✅ 必过 | 走真实 `write_private_new` 路径 |
| DACL 里有当前用户、且 SID 与令牌一致 | ✅ 已验证 | ✅ 必过 | `GetNamedSecurityInfoW` + `EqualSid` 逐字节比对 |
| 收紧能压掉**显式设置的宽 ACL** | ✅ 已验证 | ✅ 必过 | 先把 DACL 设成「受保护 + Everyone 全权」，再走产品路径，要求 Everyone 消失 |
| **`SE_DACL_PROTECTED` 继承保护位** | ❌ **未覆盖** | ✅ **必须成立，不允许跳过** | 见下 |

#### 跳过与否由「环境」决定，不由「探测结果」决定

这是本用例最关键的一条设计约束。早期的写法是"探测一下平台支不支持保护位，
不支持就跳过"——那会在**真实 Windows 也回归时静默降级**，而那恰好是最该报错的
时刻。

现在先用 `running_under_wine()` 判断环境：wine 的 `ntdll.dll` 导出
`wine_get_version`，真实 Windows 没有这个符号。

- **环境是 wine** → 保护位那段打印"未覆盖（wine 限制）"并**继续**；同时用独立
  探针复验"wine 确实丢弃了该控制位"，如果哪天 wine 支持了，这条会反过来失败并
  提示把断言改成无条件执行。
- **环境是真 Windows** → 保护位与"恰好一个 ACE"**无条件断言**，没有任何降级
  分支。真机上跳过保护位检查是不可能的。

#### 权限负面对照不依赖临时目录的默认 ACL

上一版用"普通文件的 ACE 数不是 1"做对照，那依赖临时目录默认权限恰好比较宽——
换台机器、换个父目录就可能空过。现在改成**自己造输入**：用 SDDL
`D:P(A;;GA;;;WD)` 把文件 DACL 显式设成「受保护 + Everyone 全权」，确认这个宽
权限真的读得回来，再调 `restrict_to_current_user`，要求 Everyone 消失、当前
用户仍在列。wine 上实测 `["S-1-5-18", "…-1000", "S-1-1-0"]` → `["S-1-5-18", "…-1000"]`。

#### 为什么 wine 上验不了保护位

一个**完全绕开产品代码**的独立探针（SDDL 直接调 `SetFileSecurityW`）在 wine 上
返回成功，但 `GetNamedSecurityInfoW` 读回 rc=5，且落盘 DACL 会被追加一个
`SYSTEM`（S-1-5-18）ACE——即"API 说成功、语义不落地"。所以 wine 的 DACL 结果
只能作为"产品代码确实调用了正确的 API 并落盘"的证据，**不能**证明继承保护位。

真 Win10 上的 DACL/继承行为需要负责人在真机复验；本仓库的任何 wine 结果都不
构成真机结论。
