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
- 终端输出与 MCP 命令结果会自动把 Windows 控制台的非常见编码（如 GBK / cp936）统一转换为 UTF-8 后回传，浏览器与 AI Agent 侧无需关心目标机编码
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
| 换机助手（原子自升级） | ✅ | ✅ 交叉编译验证，`OpenProcess` / DACL 路径未在实机跑过 |
