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

## 身份保持与回滚

- **制品目录必须可读**（relay 需要现场算 hash）；**agent 所在目录必须可写**（替换需要该目录写权限），失败会在后台明确提示
- **旧 agent（≤ v0.59.2）只认 relay 的 `url`**：收到仅含外部 CDN 的任务时会解析失败、只回一个错误帧，旧进程根本不会升级。因此没有本地镜像时 relay 会对这类 agent 返回 `409 legacy_agent_needs_relay_mirror`，而不是白发送。它们同样不会上报 `variant:*`，未配 `--upgrade-variant` 时返回 `409 variant_unknown`
- **身份在换机后保持不变，临时 agent 也一样。** 旧进程退出前会把**实际** `session_id`、全部 `(token, permission)` 与原始 argv 写进一次性交接文件；新进程用它作为首次注册的 `cached_tokens`，走既有 `register_existing` 路径，`session_id`、Token 与权限都不变，不新增任何 relay API。未指定 `--key` 的临时 agent 同样保住 `session_id` **和** Token，浏览器上已打开的页面与已下发的 MCP 凭据继续可用
- **交接文件安全**：放在 `0700` 私有目录下的 `0600` 文件（Unix）/ 限当前用户的 DACL（Windows），随机文件名 + `create_new`。Token 不进 argv、不进日志、不进握手管道。新进程注册成功后立即删除，**不作为长期凭据存储**
- **回滚有界。** 新进程 75s 内未上报 `registered`、提前退出，或助手 20s 内没能 `ready`，助手都会恢复备份并重启旧版；重启前先用交接的缓存身份做一次 `register_existing`（等价旧版的普通重连），所以回滚后的临时设备同样保持原 `session_id` 与 Token。若 relay 已不认这份身份（例如全量重启丢了内存中的 token 表），助手会明确报 `Lost` 并保留备份与日志，不宣称回滚成功
- **relay 失忆是已知边界**：全量重启后临时 Token 只存在于该 relay 内存中，临时 agent 只能重新签发身份；固定 key 可恢复。不会把临时 Token 静默转成永久 key
- Windows 部分仅交叉编译验证，`OpenProcess` / DACL 等路径尚未在实机跑过
- **信任边界**：hash 来自官方 release 的 asset digest，能证明字节与 GitHub 上已上传的资产一致；它**不能**抵御官方仓库或 release 资产被恶意替换。需要这个保证要另加发布签名（不在当前范围）。CDN、DoH、代解析只解决可达性，不参与版本与字节决策
