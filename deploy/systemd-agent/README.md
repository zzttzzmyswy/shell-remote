# shell-remote agent 部署模板

面向 **systemd 托管**的 shell-remote agent 部署模板。把 agent 装成一个开机自启的
系统服务，运行在固定账户下，会话 ID 与鉴权 key 固定不变，远端终端环境由一份
配置文件统一预置。

适用：需要长期在线、可被固定 URL/token 访问的机器（服务器、嵌入式设备、测试机）。

## 快速开始

在目标机（root）执行：

```sh
tar xzf shell-remote-agent-template.tar.gz
cd shell-remote-agent-template
./install.sh
```

默认行为：创建账户 `sr-agent`、把二进制装到 `/usr/local/bin/shell-remote`、
写配置 `/etc/shell-remote/agent.env`、装服务 `shell-remote-agent.service` 并
`enable --now`。会话 ID 由主机名推导，鉴权 key 随机生成并在结尾打印。

自定义：

```sh
SR_SESSION_ID=sophgo1324 \
SR_KEY=$(openssl rand -hex 24) \
SR_RELAY_URL=https://sshx.zztweb.top \
SR_SHELL=/bin/bash \
./install.sh
```

`install.sh` 是幂等的：已存在的 `agent.env` 不会被覆盖（要重写加 `--force`）。

## 文件

| 文件 | 安装到 | 说明 |
|---|---|---|
| `install.sh` | — | 安装器：建账户、装二进制、渲染配置与 unit、enable + start |
| `templates/shell-remote-agent.service` | `/etc/systemd/system/shell-remote-agent.service` | systemd unit 模板 |
| `templates/agent.env` | `/etc/shell-remote/agent.env`（0600 root） | 全部可调配置 |

## 六个要点怎么落地

### 1. 配置终端模式

先选二进制，两者协议互通、可接入同一 relay：

| 二进制 | 能力 | 适用 |
|---|---|---|
| `shell-remote`（CLI，lean） | 仅终端转发 | 服务器/嵌入式，体积小（约 6.5 MB），无桌面重依赖 |
| `shell-remote-ui`（UI） | 终端 + 桌面转发 | 需要远程桌面的机器（约 21 MB） |

模板默认用 CLI 版。

**视图模式**：`--view <auto|window|tui|headless>` **只有 UI 版有**，CLI 版没有这个参数。
systemd 下没有 TTY，`auto` 自动落到 `headless`，不会渲染状态面板。如果一定要用 UI 版
跑服务，建议显式 `SR_EXTRA_ARGS=--view=headless` 固定住。

**远端 shell**：由 `--shell` 决定（`SR_SHELL`，默认 `/bin/bash`）。

> ⚠️ **必须显式传 `--shell`。** 该参数在 CLI 上绑定了 `SHELL` 环境变量，而 systemd 会为
> 带 `User=` 的服务注入 `SHELL=<该账户登录 shell>`，不显式覆盖就会被它顶掉。
> 实测：账户登录 shell 改成 zsh 后，agent 进程里 `SHELL=/usr/bin/zsh`，远端终端
> 子进程仍是 `/bin/bash` —— 因为 unit 里显式传了 `--shell=${SR_SHELL}`。
> 本机原有的一个 agent 服务没传 `--shell`，于是远端拿到的是 `/usr/bin/zsh` 而非默认 bash。

**终端能力**：agent 会向终端子进程注入 `TERM=xterm-256color` 和 `COLORTERM=truecolor`
（网页终端做能力协商用），这两个不需要配。locale 见第 6 点。

### 2. 用 systemd 管理服务

```sh
systemctl status  shell-remote-agent
systemctl restart shell-remote-agent
journalctl -u shell-remote-agent -f
```

unit 的关键点：

- `EnvironmentFile=/etc/shell-remote/agent.env` —— 所有可调参数都从配置文件来，
  ExecStart 里用 `${VAR}` / `$VAR` 引用。改配置只需编辑该文件再 `restart`，不用动 unit。
- `$VAR`（独立成词）按空白拆分成 0..N 个参数，**变量为空或未定义时展开成 0 个参数**——
  `$SR_EXTRA_ARGS` 依赖这个语义实现「留空则不追加任何参数」。
  `${VAR}`（词内）则就地替换，变量未定义时替换为空串。
- `Restart=always` + `RestartSec=5`：兜底进程崩溃。**不依赖它扛 relay 断连**——
  agent 自身有指数退避重连（实测 2→4→8→16→32→60s 封顶），relay 挂掉期间进程不退出，
  `NRestarts` 保持 0，relay 恢复后自动重新注册。因此也不需要关掉 systemd 的启动频率限制。

### 3. 开机自启动

`install.sh` 执行 `systemctl enable`，在
`/etc/systemd/system/multi-user.target.wants/shell-remote-agent.service` 建符号链接，
配合 unit 里的 `[Install] WantedBy=multi-user.target`。

`After=network-online.target` + `Wants=network-online.target` 保证网络就绪后才启动；
即使开机时 relay 尚不可达，agent 也会在退避重连里等到它起来，不会就此失败退出。

查状态：`systemctl is-enabled shell-remote-agent` → `enabled`。

### 4. 固定会话 ID 和 key

```sh
SR_SESSION_ID=sophgo1324    # 5-20 位 ASCII 字母数字 [A-Za-z0-9]
SR_KEY=<固定值>             # 直接作为 rw token，重启不变
```

- 会话 ID 非法（长度不对 / 含 `-`、`_`、空格、非 ASCII）时 agent 启动即报错退出。
  `install.sh` 会按同一规则校验。
- **重复注册不报冲突**：relay 会顶替旧会话（newest wins），旧 token 失效。
  实测重启服务时 relay 侧日志：
  `registration evicted a previous session with the same session_id/token — duplicate agent detected`。
  （注意：`--session-id` 的 `--help` 文案仍写着「冲突则 relay 拒绝注册并退出」，与实际行为
  不符，是仓库里没同步的旧注释。）
- `--token-type`：`rw` / `ro` / `both`。**要 token 长期稳定就用 `rw`**——
  `both` 会额外生成一个随机 ro token，每次重启都变。
- 固定 key 后，浏览器可以用 `token=<SR_KEY>` 长期访问同一会话，书签不会失效。

验证（relay 管理后台 `/api/overview` 实测输出）：

```
session_id   = srtmpl01
fixed_key    = 'testkey123456'
is_temporary = False
tokens       = [{'permission': 'rw', 'token': 'testkey123456'}]
```

### 5. 账户名称固定

unit 里 `User=` / `Group=` 指定运行账户（默认 `sr-agent`，由 `install.sh` 创建）。
systemd 会为带 `User=` 的服务注入 `HOME` / `USER` / `LOGNAME` / `SHELL`，
远端终端里看到的就是这个账户：

```
USER=srtmpl  HOME=/home/srtmpl  LOGNAME=srtmpl
```

`--root`（`SR_ROOT`）是文件管理器的默认根目录，同时是远端终端的初始 cwd。

> 该账户实际上拥有一个可读写任意文件的 shell，**不要**把它当成权限边界——
> 它的权限就是你给 `User=` 的权限。要降权就在 unit 里加
> `ProtectSystem=` / `ReadWritePaths=` 之类的沙箱选项，但那会同时限制远端终端的能力。

### 6. 预置环境变量

**这是本模板最关键的一点。** 从 **v0.53.1** 起，agent 不再向终端子进程注入任何 locale
变量（`LANG`/`LC_*`），只注入 `TERM`/`COLORTERM`，**其余环境原样继承**。

也就是说：

> `/etc/shell-remote/agent.env` 就是远端终端环境的事实来源。

在里面设的每个变量，远端 `env` 都能看到；没设的，远端拿到的是 systemd 给服务的默认值。
实测（v0.53.1，agent 环境 → 远端 shell 子进程环境）：

| 变量 | 来源 | 远端终端可见 |
|---|---|---|
| `PATH` | `agent.env` | `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` |
| `LANG` / `LC_ALL` | `agent.env` | `zh_CN.UTF-8` |
| `TZ` | `agent.env` | `Asia/Shanghai` |
| `TERM` / `COLORTERM` | agent 注入 | `xterm-256color` / `truecolor` |
| `USER` / `HOME` / `LOGNAME` | systemd（`User=`） | 运行账户 |

配置建议：

- **PATH**：systemd 给服务的默认 PATH 很短（`/usr/local/sbin:/usr/local/bin:/usr/bin`），
  不含 `/sbin`、`/usr/sbin`、`~/.local/bin`。远端要跑运维命令就得在这里补全。
- **locale**：默认**不设**，直接继承目标机自身的 locale —— systemd 会把目标机的
  `/etc/locale.conf`（或 `/etc/default/locale`）里的 `LANG`/`LC_*` 传给服务，再经 agent
  原样透传到远端终端。实测目标机只有 `C`/`C.UTF-8`/`POSIX` 时，远端终端拿到
  `LANG=C.UTF-8`，无告警。
  只有需要覆盖目标机设置时才在 `agent.env` 里取消 locale 行的注释。
  **只设目标机上真实存在的 locale**（`locale -a` 确认）。设一个不存在的 UTF-8 locale
  会让依赖 locale 的交互式程序每次启动都告警，甚至直接崩（静态 glibc 的 bash
  直接 SIGSEGV，网页端只看到一个空白终端）——这正是 v0.53.1 去掉硬编码注入的原因。
  （模板早期版本默认写死 `zh_CN.UTF-8`，在只有 `C.UTF-8` 的嵌入式目标机上会刷告警，
  已改为默认注释掉。）
- **日志**：默认输出到 stderr → journald。若要落文件，设 `SR_LOG_DIR=<目录>`
  （按小时滚动），此时 journald 里不再有业务日志。

## 手工部署（不用 install.sh）

```sh
# 1. 账户
useradd --system --create-home --shell /bin/bash sr-agent

# 2. 二进制（官方一键脚本的 --download-only 只落盘不执行）
cd /tmp && curl -fsSL https://sshx.zztweb.top/agent/install | sh -s -- --download-only
install -m 0755 /tmp/shell-remote /usr/local/bin/shell-remote

# 3. 配置
install -d -m 0755 /etc/shell-remote
install -m 0600 /dev/null /etc/shell-remote/agent.env
$EDITOR /etc/shell-remote/agent.env      # 至少填 SR_RELAY_URL / SR_SESSION_ID / SR_KEY / SR_ROOT

# 4. unit：把模板里的 __SR_USER__ / __SR_GROUP__ / __SR_BIN__ / __SR_CONF__ /
#    __SR_SERVICE__ / __SR_DESC__ 占位符替换掉
install -m 0644 shell-remote-agent.service /etc/systemd/system/

# 5. 启用
systemctl daemon-reload
systemctl enable --now shell-remote-agent
journalctl -u shell-remote-agent -n 20 --no-pager
```

启动成功的标志是这两行：

```
INFO shell_remote::agent: agent session established session=<你的会话 ID>
INFO shell_remote::agent: token: <你的 key> session=<你的会话 ID> permission=rw
```

## 运维

```sh
# 改配置
$EDITOR /etc/shell-remote/agent.env && systemctl restart shell-remote-agent

# 升级二进制（会话 ID 与 key 不变，重启后自动顶替旧会话）
cd /tmp && curl -fsSL https://sshx.zztweb.top/agent/install | sh -s -- --download-only
install -m 0755 /tmp/shell-remote /usr/local/bin/shell-remote
systemctl restart shell-remote-agent

# 卸载
systemctl disable --now shell-remote-agent
rm -f /etc/systemd/system/shell-remote-agent.service
systemctl daemon-reload
rm -rf /etc/shell-remote
userdel -r sr-agent        # 会一并删除该账户 home 下的文件
```

## 已知行为与坑

**鉴权 key 会出现在两个地方，按需取舍：**

1. `ExecStart` 的 argv 里（`--key=...`）——`/proc/<pid>/cmdline` 和 `systemctl status`
   都**对所有本地用户可读**。
2. 远端终端的 `env` 里——因为 agent 把自身环境透传给终端子进程，实测远端 shell 能看到
   `SR_KEY=testkey123456`。

  第 2 点意味着：**任何连上来的人都能读到这个长期有效的 key**，一次性的访问者因此变成
  永久 key 持有者。如果这不可接受，就别把 key 放进 `agent.env`，改成直接在 unit 的
  `ExecStart` 里写死 `--key=<值>`（`chmod 0600` 一个 drop-in 覆盖文件），这样它只在
  argv 里、不进环境。

  反过来，`EnvironmentFile` 里的值**不会**被非特权用户通过 `systemctl show` 读到
  （实测输出为空的 `Environment=`），`/proc/<pid>/environ` 也是 `0600 root`。

**relay 侧要求**：`--auth <密码>` 是必填的；管理后台需要显式 `--admin-path` 才能访问
（首页没有任何入口链接）。relay 默认自签 TLS 证书——连自签 relay 时 agent 必须加
`--relay-insecure`（`SR_EXTRA_ARGS=--relay-insecure`），否则 register 握手直接失败。
`https://sshx.zztweb.top` 用的是 Let's Encrypt 证书，不需要这个参数。

**重连后管理后台的 `fixed_key` 会显示为 `null`、`is_temporary` 变 `true`**：agent 重连
走的是 `register_existing()` 路径，不携带 `fixed_key`，而 `is_temporary` 的定义就是
`fixed_key.is_none()`。这**不影响 token 本身**——实测重连后 token 仍是固定的
`testkey123456`。仅影响后台展示，以及让该会话在空闲 30 分钟后可能被回收
（agent 随即重连并重新注册同样的 token）。

**会话 ID 顶替是全局的**：同一 relay 上两台机器用同一个 `SR_SESSION_ID` 会互相顶替，
后注册的把先注册的踢下线（旧 token 同时失效）。多机部署时每个会话 ID 必须唯一。

## 验证记录

本模板在 13.24（zztArch，systemd 261，shell-remote v0.53.1）上做过完整验证，
使用独立的测试账户/服务名/配置目录和一个本地 relay（`relay --bind 127.0.0.1:39000
--auth testpw --no-tls --admin-path /sr-admin-t`），未触碰机器上原有的
`shell-remote-agent.service`。验证项：

| 验证项 | 结果 |
|---|---|
| 服务启动 / `enable` 自启 | `active` / `enabled`，`multi-user.target.wants` 链接已建 |
| 固定 session-id + key | relay `/api/overview`：`session_id=srtmpl01`、`token=testkey123456` |
| 固定运行账户 | 远端 shell 子进程 `USER=srtmpl HOME=/home/srtmpl LOGNAME=srtmpl` |
| `--shell` 覆盖 systemd 注入的 `SHELL` | 账户登录 shell=zsh，终端子进程仍为 `/bin/bash` |
| 预置环境变量透传 | `PATH`/`LANG`/`LC_ALL`/`TZ` 均出现在终端子进程环境 |
| 重启后 ID/key 不变 | 重启 + relay 重启后仍为 `srtmpl01` / `testkey123456` |
| 同 ID 重复注册 | relay 顶替旧会话，agent 不报错退出 |
| relay 断连恢复 | 进程不退出、`NRestarts=0`，退避重连后自动恢复 |
| `$SR_EXTRA_ARGS` 留空 | ExecStart 展开后无多余空参数（`systemctl status` 可见完整 argv） |

另在**真实设备**上部署验证过：Sophon BM1684（aarch64，Ubuntu 20.04，systemd 245，
overlayfs 根文件系统 + eMMC 可写上层）。`User=` 用设备已有的 `linaro` 账户，
`SR_SESSION_ID=sophonbm1684`，接入生产 relay `https://sshx.zztweb.top` 注册成功；
`enable` 后服务随开机自启（unit 与 enable 符号链接都落在可持久化的 overlay 上层）。
该设备只有 `C`/`C.UTF-8`/`POSIX` locale，据此把模板默认的硬编码 locale 改成了默认不设。
