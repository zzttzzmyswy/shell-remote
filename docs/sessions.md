# 会话录制、Token 权限与文件管理器

## 会话录制

relay 加 `--record-dir <目录>` 即可录制交互式终端会话（不含 MCP exec）：

```bash
./shell-remote relay --auth YOUR_PASSWORD --bind 0.0.0.0:3000 --record-dir /var/log/shell-remote
```

- **格式**：asciinema cast v2（JSONL），可用 `asciinema play xxx.cast` 或 xterm.js 回放
- **内容**：录制输出 + 输入流；**输入流会包含终端里键入的敏感内容（如 sudo 密码），请妥善保护录制目录的文件权限**
- **命名**：每会话一个文件 `{session_id}_{unix时间戳}.cast`；agent 用 `--session-id` 指定后文件名的前半即该 ID
- **捕获点**：在 relay 侧捕获，不影响 agent；踢出/空闲回收会话时文件自动 flush 关闭
- **后台查看**：`--record-dir` 启用后后台"录像"面板列出全部录制文件与录制时间/大小，终端录像可在内嵌播放器（xterm.js）里回放，支持播放/暂停、0.25x–4x 慢放快放、时间轴拖拽跳转，可一键删除

MCP 命令审计（`.audit.jsonl`）与终端录制同开关、同目录，见 [mcp.md](mcp.md)。

## Token 权限模型

| Token 类型 | 终端输入 | 文件操作 | MCP 执行 |
|-----------|---------|---------|----------|
| ReadWrite | ✅ | ✅ | ✅ |
| ReadOnly | ❌ | 列表 / 读取 | ❌ |

- **临时 Token**：agent 不指定 `--key` 时随机生成，仅存在于 relay 内存。agent 断线后仍保留供浏览器重连，空闲 30 分钟（1800s）后清理
- **固定密钥**：通过 `--key` 指定，agent 重连后仍可使用，不受空闲清理
- **`--token-type both`**：同时签发 rw 与 ro 两把 Token（`--key` 指定时 rw 那把即 key 本身）
- 后台可在线把某个 Token 在 rw↔ro 之间切换，也可撤销或重生成（见 [admin.md](admin.md)）

只读 Token 允许发送的消息类型是白名单式的（`src/proto/mod.rs` 的 `requires_write`）：`session:users`、`session:tab_list`、`fs:list`、`fs:read`、`mcp:exec_list`，以及桌面的只读操作 `desktop:reqkey`、`desktop:test-delay`、`desktop:clipboard:get` 与 P2P 信令（`desktop:p2p-offer/answer/candidate/state`）。桌面控制类（mouse / key / clipboard:set / quality / codec / gray）与 `fs:mkdir`、终端输入等仍需写权限。

## 文件管理器

会话页右侧的文件管理器面板：

- 面包屑路径导航
- 上传（流式传输，上限由部署方反向代理配置决定，见 [mcp.md](mcp.md) 的"大小限制"）
- 下载、删除、重命名、新建文件夹、刷新
- 侧栏宽度可拖拽调整

文件操作以 agent 的 `--root`（默认 `$HOME`）为基准；绝对路径按目标机文件系统原样使用。**不做 `--root` 沙箱**：`..` 可以逃出默认目录，这是与现有测试一致的设计选择；如需限制应在目标机上用文件系统权限约束 agent 运行账户。
