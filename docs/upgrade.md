# Agent 原子自升级

后台"设备"面板逐台一键触发：agent 从 relay 下载新二进制 → SHA-256 校验 → 可执行性冒烟测试 → 同目录原子替换 → 以原参数重启。全程进度实时显示在设备行上，任何一步失败都保留原二进制并给出明确错误。

## 制品库与两个入口

relay 加 `--download-dir <目录>` 启用（`--agent-upgrade-dir` 是兼容别名；两个都设且指向不同目录会拒绝启动）。目录里放与发布制品同名的文件（命名由 `tools/build-dist.sh` 决定）：

| 形态 | 文件名 |
|------|--------|
| CLI（lean，仅终端转发） | `shell-remote-x86_64`、`shell-remote-aarch64`、`shell-remote-armv7`、`shell-remote-x86_64.exe` |
| UI（桌面 agent + 窗口面板） | `shell-remote-ui-<arch>[.exe]` |

目录布局：

```
<root>/shell-remote-aarch64                扁平（安装脚本的既有约定）
<root>/shell-remote-aarch64.version        可选的版本标注（后台展示）
<root>/releases/v0.59.2/<文件名>            多版本隔离，读取优先于扁平名
<root>/.metadata/manifests/v0.59.2.json    官方清单缓存，不通过 /download 暴露
```

可选写一个 `<文件名去掉 .exe>.version` 标注版本号（如 `0.59.2`）。版本归档到 `releases/v<版本>/` 可让多版本并存而不互相覆盖，读取时优先于扁平名。

同一批字节有两个读取入口，权限边界不合并：

| 入口 | 用途 | 鉴权 |
|------|------|------|
| `/download/<文件名>` | 安装脚本公开下载 | 无 |
| `/agent/upgrade/blob/<文件名>` | 已注册 agent 自升级 | rw Token（`X-SR-Token`） |

两者都支持 `Range`：命中区间回 `206` 且带正确的 `Content-Range` / `Content-Length`，非法或越界区间回 `416` + `Content-Range: bytes */<len>`，供 agent 用 `.part` 文件断点续传。

## 升级任务怎么来的

**relay 不能仅凭版本号就编出一个 hash。** 判定分两步。

### 第一步：是否需要升级

`semver(relay 版本) > semver(agent 注册上报的版本)`。相等不升级，更低不降级，agent 未上报或版本非法只显示"版本未知"（不自动升级）。预发布按 semver 规则（`0.59.2-beta.1 < 0.59.2`）。`force: true` 允许重装同一版本，但绝不降级、也不在版本未知时猜。

### 第二步：升级到哪一份确切字节

按 relay 自身版本取官方 GitHub release 清单（`GET /repos/zzttzzmyswy/shell-remote/releases/tags/v<版本>`，API 根可用 `--manifest-api-base` 覆盖），只采用带 `digest: sha256:…` 且资产名精确匹配的条目。

制品形态（CLI / UI）按设备上报的 `capabilities` 选：只认 agent 显式声明的 `variant:cli` / `variant:ui`，其次用 relay 的 `--upgrade-variant`；两者都没有时**拒绝升级**（`409 variant_unknown`）。不设默认形态是有意的——升级是原地替换二进制，猜错就是把 UI 设备换成 lean CLI。`desktop:*` 不能作为判据：lean CLI 二进制同样会无条件上报 `desktop:gray` 等。

制品来源按下表决定（`asset` = 官方清单里的对应资产，`local` = 库内实测 hash 的副本）：

| asset | local | 结果 |
|-------|-------|------|
| 有 | hash/size 一致 | `source: local`，CDN 优先 + relay 镜像兜底 |
| 有 | hash/size 不一致 | **拒绝下发**（发布异常，不静默换 hash） |
| 有 | 无 | `source: official`，仅 CDN |
| 无 | 有且已 `--allow-custom-upgrade` | `source: custom`，仅 relay 镜像，不提供 CDN |
| 无 | 有但未批准 | 拒绝，提示需要 `--allow-custom-upgrade` |
| 无 | 无 | 拒绝（`目标档案不存在`） |

GitHub 不可达时：已缓存的清单继续可用（不因网络波动让已验证任务失效）；同版本 digest 与缓存不一致则标记发布异常并拒绝该版本；无缓存且无本地镜像则不生成任务。

### CDN 列表

`--upgrade-cdn` 每项是一个纯 origin（如 `https://gh-proxy.com`），`origin + official_path` 即完整下载 URL。未指定时用 `DEFAULT_CDN_BASE_URLS`：`https://github.com`、`https://edgeone.gh-proxy.com`、`https://gh-proxy.com`、`https://gh.llkk.cc`。这些主机名同时构成 `/agent/resolve` 代解析白名单。

## 下发字段

`agent:upgrade` 的 `payload`：

```json
{
  "upgrade_id": "4f2c…",
  "target_version": "0.59.2",
  "filename": "shell-remote-ui-aarch64",
  "size": 16956344,
  "sha256": "e8233ca1…d851",
  "official_path": "/zzttzzmyswy/shell-remote/releases/download/v0.59.2/shell-remote-ui-aarch64",
  "cdn_base_urls": ["https://github.com", "https://edgeone.gh-proxy.com"],
  "relay_url": "/agent/upgrade/blob/shell-remote-ui-aarch64",
  "relay_fallback": true,
  "source": "local",
  "official_release": true,
  "variant": "ui",
  "arch": "aarch64",
  "windows": false,
  "version": "0.59.2",
  "url": "/agent/upgrade/blob/shell-remote-ui-aarch64"
}
```

`version` / `url` 是给存量 agent（≤ v0.59.2，只认这三个键加 `sha256`）的兼容别名。没有 relay 副本时不给 `url`，admin 响应里的 `legacy_compatible: false` 会提示老 agent 无法完成这次升级。

## 代解析端点

`GET /agent/resolve?host=<CDN 主机>`（rw Token 鉴权）供系统 DNS 与 DoH 都不可用时使用。它**不是**通用 DNS 代理：只解析 `--upgrade-cdn` 列出的主机（外加官方 `github.com`），裸主机名形态，其他一律 `403`（响应不回显允许列表，不给探测者提供枚举素材）。

```json
{ "host": "gh-proxy.com", "addresses": ["1.2.3.4"], "expires_at": 1793000000, "cached": false }
```

TTL 上限 300s，解析失败有 15s 负缓存（仍带 `expires_at`，让 agent 在 TTL 后重试而不是永久封死该源）。单次 `getaddrinfo` 超时 5s，单主机最多返回 8 个地址。

## 执行流程

后台"设备"面板的**升级**按钮（或 `POST <admin>/api/agent/upgrade`，body `{"session_id": "…", "force": false}`）触发后：

1. relay 做版本决策、取清单、定位制品并交叉校验，生成不可变任务下发 `agent:upgrade`
2. agent 下载制品（外部 CDN 请求不带 relay Token；relay 请求沿用 rw Token），边下边上报进度
3. 校验 SHA-256——不匹配即失败，保留原二进制；长度不符同样失败
4. Unix 上先跑一次 `--version` 冒烟测试（防架构不匹配 / 截断）；Windows 跳过（以 SHA-256 为准，且运行会短暂起第二个 agent）
5. 启动一个短命的换机助手进程（`shell-remote update-apply <handoff>`，隐藏子命令，`shell-remote-ui` 里同样存在）。它在 stdout 上只报一行 `ready`，**只有收到这一行之后**旧 agent 才退出

`GET <admin>/api/agent/upgrade/plan?session_id=…` 只做版本决策与制品定位，**不**下发指令、不退出任何进程，供设备列表显示"是否需要升级 / 能否升级"。

### 换机助手

助手等到旧进程确切退出后（Unix 用 `kill(0)` + `/proc/<pid>/stat` 的启动时间；Windows 用 `OpenProcess(SYNCHRONIZE)` + `WaitForSingleObject` + 进程创建时间——**从不按进程名判断**），把当前二进制改名为备份、原子安装新二进制、以原参数拉起，并在匿名管道上等一个 `registered` 回执后才删除备份。不安装任何 systemd / SCM 单元，也没有长期驻留的 supervisor。

关键超时：助手 ready 20s、旧进程退出 45s、新进程上报 `registered` 75s。超时或提前退出即回滚。

升级全程在设备行的"版本"列实时显示状态（已触发 / 下载中 n% / 校验中 / 安装重启中 / 失败原因）；完成后显示绿色"已升级 v<版本>"，device 版本列同步刷新。

## systemd 托管下的换机

### 为什么需要单独处理

换机的正常路径是**旧 agent 主动退出**，让助手替换二进制。systemd 默认的
`KillMode=control-group` 会在 service 主进程退出时，把该 service cgroup 里的
**所有**进程一起杀掉——包括刚被拉起的助手。

`setsid` / `nohup` / `process_group(0)` 都救不了这一点：它们只改进程组或会话，
**不改 cgroup**。v0.59.5 及更早版本的现场故障就是这个形状：助手日志停在
`ready announced; waiting for the old process to exit`，磁盘仍是旧版，relay 收
不到终结帧，管理页永远停在"换机中"。

### v0.59.6 起怎么做

agent 启动助手前先判断自己是否受 systemd 托管，判据是**两个条件同时成立**：

| 条件 | 读法 | 为什么不能只用其中一个 |
|------|------|------------------------|
| `INVOCATION_ID` 非空 | 环境变量 | 在服务的终端里手工跑 agent 会继承它，但那种进程不在 unit cgroup 里 |
| 自身 cgroup 落在 `*.service` | `/proc/self/cgroup` | 只认 `.service`：只有 service 有"主进程"，`KillMode` 的清理由主进程退出触发；scope 没有主进程，不会因某个成员退出而拆掉 |

托管时，助手不再直接 `spawn`，而是通过 `systemd-run` 起成**独立的 transient
unit**（用户级 manager，`--user`）：

```text
systemd-run --user --unit=shell-remote-upgrade-<id> --collect --quiet \
  --property=KillMode=process --pipe \
  /usr/local/bin/shell-remote update-apply /home/sr-agent/.shell-remote/upgrade/.handoff-….json
```

- `--property=KillMode=process`：助手退出时只收掉它自己，**不连带杀掉它拉起的
  新 agent**——默认的 `control-group` 会把新 agent 一起清掉，等于把问题下移一层。
- `--pipe`：stdout 仍是 `ready` 握手通道，旧 agent 的判据与直接助手完全一致。
- 只有交接文件**路径**进 argv。Token、`--key`、原始 argv 全在 `0600` 的交接
  文件里，不进 unit 属性（`systemctl show` 可读）、不进日志。
- 非 systemd 环境（容器、普通 shell、Windows）继续走既有的直接助手流程，
  行为与之前逐字节一致。

### 失败时不会卡住

`systemd-run` 不存在 / 无权限（系统级 `systemd-run` 需要 polkit 授权）/ 起不来 /
20s 内没报 `ready` 时：

1. 删除本轮交接文件；
2. 上报 `failed` 并**继续运行**（磁盘一点没动）；
3. 绝不回退到会被同 cgroup 清理的直接助手。

### relay 侧的状态收敛

助手被连带杀掉时不会有人发终结帧。relay 因此会在 agent **重新注册**时收敛：

- 同一个 `upgrade_id` 仍在换机中途（`verifying` / `swapping` / `installing`），
  而重连回来的版本**低于**目标版本 ⇒ 当场转 `failed`，原因写明"助手未完成、
  磁盘未变、请查设备上的 `update-apply.log` 后重试"；
- 重连版本**等于**目标版本 **不算成功**——成功只认新进程的 `registered` 回执，
  版本号不能用来推断；
- 已是 `registered` / `rolled_back` / `failed` 的不覆盖（`rolled_back` 是助手给
  出的更精确结论）；
- 没有重连的悬挂任务由 30 分钟 TTL 收敛为过期清理。

### 已知边界：`Restart=always` 会补起一个重复实例

旧 agent 是**正常退出（exit 0）**的，`Restart=always` 会随即用刚换上的新版再
起一个。于是设备上短暂存在两个 agent（一个在 helper 的 transient unit 里、带
`SR_UPGRADE_RESTORE`，是真正持有会话的那个；一个在 service cgroup 里、没有
恢复意图）。新 agent 会识别这个形状并按 unit 名记 warn 日志。

**不在代码里自动杀掉它**，因为两条自动路径都实测不可靠：

- `SIGTERM` 那个 pid：`Restart=always` 立刻再补一个，形成杀掉—重启循环
  （实测 `NRestarts` 从 0 涨到 3，反而把设备推进反复重启）；
- `systemctl stop <unit>`：语义正确（stop 会清掉待执行的重启），但 agent 现场
  以非 root 运行（unit 里 `User=sr-agent`），被 polkit 拒绝。

运维侧根治任选其一：

```sh
# 一次性：换机结束后确认只剩一个 agent
systemctl stop shell-remote-agent && systemctl start shell-remote-agent

# 长期：让 agent 主动退出时不再被重启（换机交接退出码是 0）
# 在 unit 的 [Service] 里把 Restart=always 改成：
Restart=on-failure
```

`Restart=on-failure` 只在该进程异常退出时重启，正常退出（含换机交接）不会补起，
因此从根上消除重复实例；崩溃自动恢复的能力保持不变。

### 排查清单

| 现象 | 查看 | 结论 |
|------|------|------|
| 管理页停在"换机中" | 设备上 `~/.shell-remote/upgrade/update-apply.log` | 末行是 `ready announced` 却无后续 ⇒ 助手被 cgroup 清掉（旧版本）；有 `cannot ...` ⇒ 按原因处理 |
| 管理页显示"升级失败 / 助手未完成" | 同上 + `systemctl status <unit>` | relay 在重连时判定助手没完成；磁盘未变，可直接重试 |
| 声明"无法作为 transient unit 启动" | `systemd-run --user --unit=probe --collect true` | 失败 ⇒ 该账户没有可用的用户级 manager（无 `XDG_RUNTIME_DIR` / 无 dbus session），升级会被明确拒绝而不是卡住 |
| 两个 agent | `pgrep -af 'shell-remote agent'`；`systemctl show -p NRestarts <unit>` | `Restart=always` 补起的重复实例，见上一节 |


## 身份保持与回滚

- **制品目录必须可读**（relay 需要现场算 hash）；**agent 所在目录必须可写**（替换需要该目录写权限），失败会在后台明确提示
- **旧 agent（≤ v0.59.2）只认 relay 的 `url`**：收到仅含外部 CDN 的任务时会解析失败、只回一个错误帧，旧进程根本不会升级。因此没有本地镜像时 relay 会对这类 agent 返回 `409 legacy_agent_needs_relay_mirror`，而不是白发送。它们同样不会上报 `variant:*`，未配 `--upgrade-variant` 时返回 `409 variant_unknown`
- **身份在换机后保持不变，临时 agent 也一样。** 旧进程退出前会把**实际** `session_id`、全部 `(token, permission)` 与原始 argv 写进一次性交接文件；新进程用它作为首次注册的 `cached_tokens`，走既有 `register_existing` 路径，`session_id`、Token 与权限都不变，不新增任何 relay API。未指定 `--key` 的临时 agent 同样保住 `session_id` **和** Token，浏览器上已打开的页面与已下发的 MCP 凭据继续可用
- **交接文件安全**：放在 `0700` 私有目录下的 `0600` 文件（Unix）/ 限当前用户的 DACL（Windows），随机文件名 + `create_new`。Token 不进 argv、不进日志、不进握手管道。新进程注册成功后立即删除，**不作为长期凭据存储**
- **回滚有界。** 新进程 75s 内未上报 `registered`、提前退出，或助手 20s 内没能 `ready`，助手都会恢复备份并重启旧版；重启前先用交接的缓存身份做一次 `register_existing`（等价旧版的普通重连），所以回滚后的临时设备同样保持原 `session_id` 与 Token。若 relay 已不认这份身份（例如全量重启丢了内存中的 token 表），助手会明确报 `Lost` 并保留备份与日志，不宣称回滚成功
- **relay 失忆是已知边界**：全量重启后临时 Token 只存在于该 relay 内存中，临时 agent 只能重新签发身份；固定 key 可恢复。不会把临时 Token 静默转成永久 key
- Windows 部分仅交叉编译验证，`OpenProcess` / DACL 等路径尚未在实机跑过
- **信任边界**：hash 来自官方 release 的 asset digest，能证明字节与 GitHub 上已上传的资产一致；它**不能**抵御官方仓库或 release 资产被恶意替换。需要这个保证要另加发布签名（不在当前范围）。CDN、DoH、代解析只解决可达性，不参与版本与字节决策
