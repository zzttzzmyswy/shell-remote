# UI 配置与连接控制接口（MYS-2806 阶段4 / MYS-2816）

两个新模块，供 `src/gui.rs` 与 `src/bin/shell-remote-ui.rs` 接入。**本阶段只交付
模型与控制层**，界面与无参数入口由负责人实现。

模块声明（`src/lib.rs`，已加）：

```rust
#[cfg(feature = "gui")]
pub mod ui_config;
#[cfg(feature = "gui")]
pub mod ui_controller;
```

## 一、`ui_config::UiConfig`

字段与 `shell-remote-ui agent` 的现有参数同名同类型（`desktop_min_bitrate` 等
仍是 UI 侧的 **kbps** 单位，`to_desktop_config()` 负责换算成 agent 的 bps）。
不含 `view` 与 `upgrade_restore`——那是内部调度入口，不是用户配置。

```rust
pub struct UiConfig {
    pub relay_url: String,            // 默认 ""（界面显示 placeholder，绝不默认连 localhost）
    pub relay_insecure: bool,         // false
    pub key: Option<String>,          // #[serde(skip)]：只在内存
    pub root: Option<String>,         // None = 用户主目录
    pub token_type: String,           // "rw" | "ro" | "both"
    pub shell: String,                // Windows "cmd.exe"，其它 $SHELL 或 "/bin/bash"
    pub session_id: Option<String>,   // 5-20 位 ASCII 字母数字；None = relay 随机
    pub no_cmd_log: bool,
    pub desktop_capture: String,      // auto|dxgi|gdi|windows|x11|wayland|none
    pub desktop_codec: String,        // av1|h264
    pub desktop_fps: f64,             // 1..=60
    pub desktop_capture_fps: f64,     // 0（不限）或 1..=240
    pub desktop_max_bitrate: u64,     // kbps；0 = 自动
    pub desktop_quality: String,      // speed|balanced|best
    pub desktop_min_bitrate: u64,     // kbps；必须 > 0
    pub desktop_display: Option<String>,
    pub desktop_lan_port: u16,        // 0 = 不开端口
}
```

关键 API：

| 函数 | 语义 |
|---|---|
| `UiConfig::default()` | 与现有 UI CLI 默认一致（`validate()` 会因 relay 为空而失败——这是**故意**的：界面要用户先填地址） |
| `validate() -> anyhow::Result<()>` | 只判断不修正；错误可直接展示；**不含密钥** |
| `default_path() -> PathBuf` | `<HOME|USERPROFILE>/.shell-remote/ui-config.json`，不受 cwd 影响 |
| `load(&Path)` | 文件缺失 → `Default`；损坏/缺 `schema_version`/版本不认识/未知字段 → 可见错误，**不回写原文件** |
| `save(&Path)` | 先 `validate()`，再 0600 临时文件 + `rename` 原子替换；`key` 不落盘 |
| `to_desktop_config()`（`desktop` feature） | 映射成 `agent::desktop::DesktopConfig`，与现有 UI 二进制逐字段一致 |

落盘格式：

```json
{ "schema_version": 1, "settings": { "relay_url": "...", "...": "..." } }
```

`key` 因 `#[serde(skip)]` 完全不出现在 JSON 里，读回来恒为 `None`——界面保存后请
继续用**内存里那份** `UiConfig` 的 `key` 去 `connect()`；重启后需要用户重新填。

## 二、`ui_controller::UiController`

```rust
let mut ctl = UiController::new();          // 不起线程、不联网
ctl.connect(draft.clone())?;                // 校验失败不会有任何后台副作用
// 每帧（例如 500ms 重绘时）:
ctl.poll();
let phase = ctl.status().map(|s| s.phase()); // None = 未连接
let err   = ctl.last_error();                // 已脱敏，可直接显示在窗口里
if ctl.is_running() { /* 显示"断开"按钮 */ }
ctl.disconnect();                            // 立即返回，不阻塞 UI
// 等 is_running() 变 false 后可以改草稿并再次 connect()
```

语义要点：

- **只有一个实例**：运行中重复 `connect()` 返回 `Err("已有连接在运行或正在停止…")`，
  不会起第二个 agent 线程（防 relay 侧互相顶号）。
- **配置快照隔离**：`connect()` 拿走传入的那份配置，界面之后怎么改草稿都不影响
  运行中的实例（有回归用例）。
- **停止有界**：`disconnect()` 只置标志；agent 在正常会话 / 重试退避 / 在途注册
  三条路径上都会在 ≤200ms 内看到它。`STOP_DEADLINE`（5s）用于界面侧判超时告警。
- **不阻塞 UI 线程**：`poll()` 只在 `JoinHandle::is_finished()` 之后才 `join()`；
  `Drop` 里的等待是**有界**的 3 秒，绝不无界阻塞。
- **错误可见**：`last_error()` 来自两条路——注册失败时 agent 每次失败尝试立即上报
  （不用等 1+2+4+…+60s 的重试跑完），以及线程退出时的失败/panic 原因。连接成功
  自动清除。
- **token 刷新仍走原路**：`ctl.status().unwrap().request_refresh()`。

推荐接入骨架（`gui.rs`）：

```rust
struct GuiApp {
    draft: UiConfig,          // 绑定到输入框
    ctl: UiController,
    ctl_error: Option<String>, // connect()/save() 的同步错误（校验）
}

// 连接按钮
match self.ctl.connect(self.draft.clone()) {
    Ok(()) => self.ctl_error = None,
    Err(e) => self.ctl_error = Some(format!("{e:#}")),
}
// 断开按钮
self.ctl.disconnect();
// 保存按钮
match self.draft.save(&UiConfig::default_path()) { /* 成功提示 / 错误留在窗口 */ }
// 每帧
self.ctl.poll();
// 展示：ctl.last_error()（异步连接错误）或 ctl_error（同步校验错误）
```

## 三、`AgentStatus` 新增方法

`status.rs` 里补了错误通道（TUI 不用它，行为不变）：

```rust
pub fn set_error(&self, message: impl Into<String>);
pub fn last_error(&self) -> Option<String>;
pub fn clear_error(&self);
```

## 四、核心取消支持改动（`src/agent/`，最小面）

| 改动 | 原因 |
|---|---|
| `client::connect_with_retry` 新增 `on_error: Option<&(dyn Fn(&str) + Sync)>` 参数 | 每次失败尝试立即上报，GUI 才能"点了连接几秒内就说出原因" |
| `agent::mod` 新增 `connect_cancellable` | 注册（含 10 次退避重试、单次 POST 20s 超时）能被 `disconnect()` 打断 |
| `agent::mod` 新增 `wait_or_shutdown` | 重连退避等待分片轮询（200ms），停机不必等满 60s |
| 停机检查移到重连分支之前 | 注册期间被打断 → `run_session` 返回后立刻消费停机请求 |
| `agent::redact_secret` | 展示前的最后一道密钥脱敏 |

CLI / TUI 行为不变：不传 `on_error` 时完全走原路径；lean 构建（无 tui feature）
下两个 helper 直接转发，不额外唤醒。

## 五、验收命令

```bash
git diff --check
cargo test --locked --no-default-features                                  # 1061 passed / 6 ignored
cargo test --locked --lib --features gui ui_                               # 34 passed
xvfb-run -a cargo test --locked --lib --features gui                       # 1277 passed / 19 ignored
cargo build --locked && cargo build --locked --no-default-features         # 两套均通过
BIN=./target/debug/shell-remote N=40 FLAP=10 ROUNDS=5 PORT=39516 SR_LOG_DIR='' \
  bash tools/concurrent_register_smoke.sh                                  # PASS
```

（`--features gui` 与默认 features 下的 X11 桌面用例需要 `DISPLAY`，无显示器时用
`xvfb-run -a`；这是仓库既有前提，非本次改动引入。）
