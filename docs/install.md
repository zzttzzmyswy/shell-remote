# 安装与部署

## 下载预编译二进制

[GitHub Releases](https://github.com/zzttzzmyswy/shell-remote/releases) 每个架构提供两个 musl 静态二进制：`shell-remote-<arch>`（CLI，relay + agent 仅终端）与 `shell-remote-ui-<arch>`（UI，agent 终端 + 桌面）。

```bash
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-x86_64
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-ui-x86_64
chmod +x shell-remote-x86_64 shell-remote-ui-x86_64
```

```bash
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-aarch64
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-ui-aarch64
chmod +x shell-remote-aarch64 shell-remote-ui-aarch64
```

```bash
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-armv7
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-ui-armv7
chmod +x shell-remote-armv7 shell-remote-ui-armv7
```

架构对应：`shell-remote-<arch>` 中的 `<arch>` 取 `x86_64`（Intel/AMD）、`aarch64`（ARM 64 位，树莓派 4/5、云服务器）、`armv7`（ARM 32 位，树莓派 2/3）。Windows 为 `shell-remote-x86_64.exe` / `shell-remote-ui-x86_64.exe`，见 [windows.md](windows.md)。

### 一行安装脚本

relay 内置安装脚本（`/agent/install` 与 `/agent/install.ps1`），会自动注入 relay 地址，优先从本 relay 的 `/download/<文件名>` 取二进制，GitHub 镜像仅作回退，内部同时支持 curl 与 wget：

```bash
curl -fsSL http://your-relay:3000/agent/install | sh
```

仅下载到当前目录不执行：

```bash
curl -fsSL http://your-relay:3000/agent/install | sh -s -- --download-only
```

`/download/<文件名>` 需要 relay 配置 `--download-dir`（见 [upgrade.md](upgrade.md)），未配置时脚本回退到 GitHub。

## 编译

```bash
git clone https://github.com/zzttzzmyswy/shell-remote.git
cd shell-remote
cargo build --release --no-default-features
```

- `--no-default-features` → lean CLI，产物 `target/release/shell-remote`，不编译任何桌面依赖
- `cargo build --release` → 默认 features，产物含 `shell-remote` 与 `shell-remote-ui`（`shell-remote-ui` 要求 `desktop` + `tui` + `gui` feature，见 `Cargo.toml` 的 `[[bin]]` 段）
- `cargo build --release --bin shell-remote-ui` → 只产 UI 二进制

交叉编译、musl 静态链路与四平台打包见 [build.md](build.md)。

## Relay 参数

```bash
./shell-remote relay --auth YourStrongPassword --bind 0.0.0.0:3000
```

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--bind` | `0.0.0.0:3000` | 监听地址 |
| `--auth` | 无默认值 | 服务器密码，**必填**（为空或缺失时 relay 拒绝启动） |
| `--no-tls` | 关闭 | 关闭 TLS，在监听端口提供纯 HTTP。默认走 HTTPS，未给证书时自动生成自签证书并持久化到 `~/.shell-remote/self-signed/`（`relay-cert.pem` / `relay-key.pem`），后续重启复用 |
| `--tls-cert` / `--tls-key` | 无 | 自定义 PEM 证书与私钥，必须成对提供 |
| `--admin-path` | 无 | 管理后台的秘密子路径（如 `/sr-admin-x7k`）。不设则后台完全不注册路由 |
| `--admin-user` | 设了 `--admin-path` 后为 `admin` | 后台登录用户名 |
| `--admin-pass` | 无 | 后台登录密码，设 `--admin-path` 时必填 |
| `--record-dir` | 无 | 终端会话录制目录（asciinema cast v2）；不设则不录制 |
| `--download-dir` | 无 | 统一制品库目录（安装分发 + agent 自升级共用同一份字节）。见 [upgrade.md](upgrade.md) |
| `--agent-upgrade-dir` | 无 | `--download-dir` 的兼容别名。两者都指定且指向不同目录会拒绝启动 |
| `--upgrade-cdn` | 内置官方 origin + 镜像列表 | 升级任务下发给 agent 的外部 CDN 基础地址，可重复或逗号分隔，每项为纯 origin。这些主机名同时构成 `/agent/resolve` 代解析白名单 |
| `--allow-custom-upgrade` | 关闭 | 允许为没有官方 digest 来源的本地镜像生成升级任务（标记 `source: custom`，不提供外部 CDN） |
| `--upgrade-variant` | 无（拒绝升级） | agent 未声明 `variant:cli` / `variant:ui` 时按此形态挑制品。不设默认值是有意的：猜错就是把 UI 设备换成 lean CLI |
| `--manifest-api-base` | `https://api.github.com` | 官方 release API 根，仅用于内网镜像或测试 |
| `--registration-rate-limit` | `120` | 每 IP 的 `agent:register` 限流（次/分钟）。同出口 IP 多台 agent 同时注册/重连时放宽 |
| `--strict-agent-auth` | 关闭 | 拒绝所有不带 `X-SR-Agent-Token` 的 agent 上行（`/agent/events`、`/agent/ws/send`、`/agent/send`）。开启后旧版 agent 无法注册上行 |
| `--bandwidth-down-mbps` | `0`（不限） | relay 整体下行（出站）带宽上限，Mbit/s。优先级：settings 文件 > 本参数 > 不限 |
| `--bandwidth-up-mbps` | `0`（不限） | relay 整体上行（入站）带宽上限，Mbit/s，优先级同上 |
| `--settings-file` | `~/.shell-remote/relay-settings.json` | 带宽设置的持久化文件。后台每次保存先写临时文件再 rename（原子写）；文件损坏或缺失按"未配置"处理并退回 CLI 值 |

## Agent 参数

两个二进制共享下表参数；`--desktop-*` 与 `--view` 仅 `shell-remote-ui` 有。

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--relay-url` | `http://localhost:3000` | Relay 地址，**只接受 `http://` 或 `https://`**（`ws://` / `wss://` 会被拒绝，内部自动按 http→ws / https→wss 推导视频上行） |
| `--relay-insecure` | 关闭 | 信任自签证书。连默认自签 TLS 的 relay 时不加会在 register 握手阶段失败 |
| `--key` | 无 | 固定鉴权密钥；不指定则随机生成临时 Token |
| `--root` | `$HOME`（Windows 为 `%USERPROFILE%`） | 文件管理器默认目录 |
| `--token-type` | `rw` | `rw` / `ro` / `both`。`both` 同时签发读写与只读两把 Token |
| `--shell` | `/bin/bash`（Windows 为 `cmd.exe`） | Shell 路径。该参数绑定了 `SHELL` 环境变量，systemd 下会被服务注入的 `SHELL`（账户登录 shell）顶掉，需要固定时请显式传参 |
| `--session-id` | 无 | 自定义会话 ID，5–20 位 ASCII 字母数字。后台据此区分设备 |
| `--no-cmd-log` | 关闭命令日志 | 默认开启：把 MCP 执行的命令与 Web 终端输入的命令行打进日志。格式见 [mcp.md](mcp.md) |

启动后日志里会有一行 `agent session established`，随后每把 Token 各一行 `token: <值>`，形如：

```
session: a1b2c3d4
  rw: 5fe42fc877b0a721157508c67fd19633c9c03cc97aaa2d5af0ced67cd3980d90
```

### 关于 `--session-id`

同一个 ID 可以被**重复使用**：agent 重启时以相同 ID 注册会顶替旧会话（旧 Token 失效），不是错误。唯一的例外是该 ID 当前有在线 agent 且新请求无法证明归属——用不同 `--key`、且没有该会话的缓存 Token 时，relay 返回 `409 SESSION_ID_IN_USE`。判据是「在线」而非「存在」：会话条目在 agent 断线后仍保留供浏览器重连，所以正常重启不会被拒。

## systemd 部署

把 agent 装成开机自启服务（固定运行账户、固定 session-id 与 key、预置远端终端环境变量）用仓库内的模板：

```bash
tar xzf shell-remote-agent-template.tar.gz
cd shell-remote-agent-template
./install.sh
```

默认创建账户 `sr-agent`、把二进制装到 `/usr/local/bin/shell-remote`、配置写到 `/etc/shell-remote/agent.env`、安装并 `enable --now` 服务。`install.sh` 幂等，已存在的 `agent.env` 不会被覆盖（要重写加 `--force`）。完整说明见 [deploy/systemd-agent/README.md](../deploy/systemd-agent/README.md)。

## Docker

仓库根目录的 `Dockerfile` 是一个两阶段构建（`rust:1.85-alpine` 构建、`alpine:3.21` 运行），把 `shell-remote` 装到 `/usr/local/bin` 并 `EXPOSE 3000`，入口即二进制本身，因此容器启动命令就是 relay/agent 的参数。容器内注意：

- `--bind` 应设为 `0.0.0.0:<port>` 才对外可达
- 自签证书目录 `~/.shell-remote/self-signed/`、`--settings-file`、`--record-dir` 与 `--download-dir` 需要挂载卷才能持久化
- `HOME` 未设置时自签证书目录会落到当前工作目录
