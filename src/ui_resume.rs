//! 无参数 GUI 的升级/回滚交接与身份恢复（MYS-2819）。
//!
//! # 为什么单独一个模块
//!
//! 无参数启动的 GUI 进程**没有 argv**：既没有 `--relay-url`，也没有
//! `--session-id`、`--key`。而自升级的既有交接协议是"原样重启同一份 argv"。
//! 两者一叠加，就出现了一个不会报错、只会悄悄发生的退化：
//!
//! - 升级后新进程拿到空 argv，活跃配置（含临时 session id、LAN 端口、桌面参数）
//!   无处可去，只能按默认值注册成**另一台设备**；
//! - 升级失败回滚时，助手会往空 argv 里补 `--session-id`——无参数入口不认识
//!   这个子命令参数，旧版直接启动失败，设备彻底失联。
//!
//! 本模块把"配置快照"接入既有交接协议，并给出入口层（`bin/shell-remote-ui.rs`
//! 与 `gui`）直接消费的一对 API。**入口与界面由负责人接入**，这里不碰它们。
//!
//! # 三条安全语义
//!
//! 1. **有效交接是唯一的自动重连授权**。磁盘上有一份普通 `ui-config.json`
//!    不代表任何恢复意图；未知的 argv/环境变量同样不算。只有隐藏恢复参数、
//!    `SR_UPGRADE_RESTORE`、或换机写下的私有指针才算。
//! 2. **快照取连接时的那一份**，不读磁盘上的普通配置文件——那可能是用户后来
//!    改过的草稿，或者是上个世纪的版本。
//! 3. **密钥不进快照**。`GuiHandoffConfig` 刻意没有 key 字段；固定密钥继续只在
//!    0600 私有交接文件里由 `fixed_key` 承载。argv、环境变量、日志都不碰它。
//!
//! # 入口层怎么用
//!
//! ```no_run
//! use shell_remote::ui_controller::UiController;
//! use shell_remote::ui_resume::resolve_gui_resume;
//!
//! /// **控制器必须由窗口/持续事件循环持有**：它是 agent 线程的所有者，
//! /// `Drop` 会请求停机并在有界时间内 join。把它留在 `main` 的栈上，
//! /// 恢复出来的连接会在窗口出现之前就被断掉。
//! struct App {
//!     ctl: UiController,
//! }
//!
//! fn start() -> anyhow::Result<App> {
//!     let mut ctl = UiController::new();
//!     // 无参数启动：没有 --upgrade-restore 参数（顶层参数解析由入口负责，
//!     // 它只需要把 Option<&str> 原样传进来）。
//!     if let Some(resume) = resolve_gui_resume(None)? {
//!         // 升级交接：恢复同配置、同身份，不弹配置窗、不等用户点击。
//!         // 用 `config_with_key()`——密钥来自交接，不在 `config` 里。
//!         ctl.connect_restored(resume.config_with_key(), resume.identity)?;
//!     }
//!     // 普通启动：只显示配置窗，绝不自动连接。
//!     Ok(App { ctl })
//! }
//! ```

use anyhow::{anyhow, Context as _};

use crate::agent::update_apply::RestoredIdentity;
use crate::ui_config::UiConfig;

/// 一次可执行的 GUI 恢复：**活跃配置快照** + **已验证的身份**。
///
/// 两个字段必须一起用：只恢复配置会以新身份注册成另一台设备，只恢复身份会
/// 让新进程跑在一份用户没选过的配置上。
#[derive(Debug)]
pub struct GuiResume {
    /// 连接时实际使用的那份配置（不是磁盘草稿）。`key` 在这里是**空的**：
    /// 固定密钥由 [`GuiResume::identity`] 里的受保护字段承载，见
    /// [`GuiResume::key`]。
    pub config: UiConfig,
    /// 交接来的身份（实际 session id + 完整 tokens + 固定密钥）。
    /// 生命周期与既有换机协议一致：注册成功即消费交接文件。
    pub identity: RestoredIdentity,
}

impl GuiResume {
    /// 本次恢复带的固定密钥（如果有）。
    ///
    /// 单独给出这个方法而不是把 key 塞进 `config`：`UiConfig` 会被界面持有、
    /// 被 `Debug` 打印、被错误文本带出去；密钥只应该出现在需要它的那一行。
    pub fn key(&self) -> Option<&str> {
        self.identity.restore.fixed_key.as_deref()
    }

    /// 恢复用的配置（含固定密钥）。**只交给 `connect_restored`**，不要落盘、
    /// 不要打印。
    pub fn config_with_key(&self) -> UiConfig {
        let mut config = self.config.clone();
        config.key = self.identity.restore.fixed_key.clone();
        config
    }
}

/// 解析无参数 GUI 的恢复意图（MYS-2819）。
///
/// - `flag`：入口层从隐藏参数 `--upgrade-restore` 拿到的值（没有就是 `None`）。
/// - 返回 `Ok(None)`：**普通启动**，没有任何恢复意图——只显示配置窗，不自动连接。
/// - 返回 `Ok(Some(_))`：一次有效的升级交接，配置与身份都来自受保护交接文件。
/// - 返回 `Err(_)`：**明确要求了恢复但拿不到可用内容**（文件缺失/损坏/字段不
///   一致）。必须让进程启动失败并让助手回滚，绝不能退化成"新设备"——那正是
///   既有 fail-closed 的语义，这里不改。
///
/// 识别顺序与既有 `resolve_restore` 完全一致（显式参数 → 环境变量 → 换机指针），
/// 因此两条路径不会对"这次到底算不算恢复"产生分歧。
pub fn resolve_gui_resume(flag: Option<&str>) -> anyhow::Result<Option<GuiResume>> {
    let identity = crate::agent::update_apply::resolve_restore(flag)
        .map_err(|e| anyhow!("{e}"))
        .context("cannot resolve the handed-over GUI connection")?;
    let Some(identity) = identity else {
        return Ok(None);
    };

    // 交接文件里没有 GUI 快照 ⇒ 发起的是一次**命令行**启动的升级。这里不能
    // 凭空造一份配置：命令行那次运行的配置在 argv 里，而 argv 不属于 GUI 的
    // 世界。把它当成"没有恢复意图"，让入口按普通启动处理（显示配置窗）。
    //
    // 这不会造成静默换身份：命令行升级从来不走本函数，走到这里的只可能是一个
    // 无参数 GUI 进程被拉起——它的 argv 里没有任何配置可恢复，强行按默认值
    // 连接才是"静默变成新设备"。
    let snapshot = identity
        .gui_snapshot()
        .ok_or_else(|| {
            anyhow!(
                "the upgrade handoff carries no GUI connection snapshot; \
                 this build cannot restore an argument-less GUI session from it"
            )
        })?
        .clone();

    let config = snapshot
        .to_ui_config()
        .context("the handed-over GUI snapshot is unusable")?;

    // 快照与实际身份/relay 必须一致：不一致只可能是文件损坏或被改过。
    // 这里**不修正**任何一侧，直接拒绝（fail-closed），让助手回滚。
    let actual_relay = config.relay_url.trim_end_matches('/');
    if actual_relay != snapshot.relay() {
        return Err(anyhow!(
            "the GUI snapshot relay is inconsistent with itself"
        ));
    }
    if !identity.restore.is_usable() {
        return Err(anyhow!(
            "the upgrade handoff carries no usable identity; refusing to start as a new device"
        ));
    }
    if snapshot.session_id.as_deref() != Some(identity.restore.session_id.as_str()) {
        return Err(anyhow!(
            "the GUI snapshot session id does not match the handed-over identity"
        ));
    }

    Ok(Some(GuiResume { config, identity }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::update_apply::{GuiHandoffConfig, RestartContext, RESTORE_ENV};
    use std::path::{Path, PathBuf};

    /// 进程级环境变量（`SR_UPGRADE_RESTORE` / `SR_UPGRADE_ROLLBACK`）是全局的：
    /// 并行跑会互相覆盖，取值/清理都必须串行。
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "sr-uiresume-{tag}-{}-{:08x}",
                std::process::id(),
                rand::random::<u32>()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn snapshot(sid: &str, relay: &str) -> GuiHandoffConfig {
        GuiHandoffConfig {
            relay_url: relay.to_string(),
            relay_insecure: false,
            root: None,
            token_type: "rw".to_string(),
            shell: "/bin/sh".to_string(),
            session_id: Some(sid.to_string()),
            no_cmd_log: false,
            desktop_capture: "none".to_string(),
            desktop_codec: "h264".to_string(),
            desktop_fps: 20.0,
            desktop_capture_fps: 0.0,
            desktop_max_bitrate: 2000,
            desktop_quality: "balanced".to_string(),
            desktop_min_bitrate: 100,
            desktop_display: None,
            desktop_lan_port: 39616,
        }
    }

    /// 造一份交接文件：`gui` 为 `None` 时是命令行启动的升级。
    fn write_handoff(
        dir: &TmpDir,
        sid: &str,
        relay: &str,
        gui: Option<GuiHandoffConfig>,
        key: Option<&str>,
    ) -> PathBuf {
        let target = dir.path().join("shell-remote");
        std::fs::write(&target, b"#!/bin/sh\n").unwrap();
        let new_binary = dir.path().join("artifact");
        std::fs::write(&new_binary, b"#!/bin/sh\n# new\n").unwrap();
        // 交接文件只校验 sha 的形状（64 hex）；内容真实性由助手安装前校验。
        let sha = crate::agent::upgrade::sha256_hex_file(&new_binary).unwrap();
        let mut ctx = RestartContext::new(
            sid.to_string(),
            vec![("tok-rw".to_string(), "rw".to_string())],
            key.map(|k| k.to_string()),
            "rw".to_string(),
            Some(relay.to_string()),
            false,
            Vec::new(),
            target,
            new_binary,
            sha,
            "up-1".to_string(),
            "9.9.9".to_string(),
            None,
            None,
            None,
            false,
        );
        if let Some(snap) = gui {
            ctx = ctx.with_gui_snapshot(snap);
        }
        ctx.write_handoff_into(dir.path()).unwrap()
    }

    #[test]
    fn test_plain_start_without_any_restore_intent_returns_none() {
        // 普通无参数启动：没有隐藏参数、没有环境变量、没有指针 ⇒ 必须返回
        // `None`，界面只显示配置、绝不自动连接。
        let _env = env_guard();
        std::env::remove_var(RESTORE_ENV);
        assert!(
            resolve_gui_resume(None).unwrap().is_none(),
            "没有任何恢复意图时不得自动恢复"
        );
    }

    #[test]
    fn test_valid_gui_handoff_resolves_to_snapshot_and_identity() {
        let _env = env_guard();
        std::env::remove_var(RESTORE_ENV);
        let dir = TmpDir::new("ok");
        let relay = "http://127.0.0.1:39999";
        let handoff = write_handoff(
            &dir,
            "guisess7",
            relay,
            Some(snapshot("guisess7", relay)),
            Some("FIXED-KEY-42"),
        );
        let resume = resolve_gui_resume(Some(&handoff.to_string_lossy()))
            .expect("有效交接必须能解析")
            .expect("必须返回一份恢复");
        // 配置来自交接快照，不是磁盘上的 ui-config.json。
        assert_eq!(resume.config.relay_url, relay);
        assert_eq!(resume.config.session_id.as_deref(), Some("guisess7"));
        assert_eq!(resume.config.desktop_lan_port, 39616);
        assert_eq!(resume.config.desktop_fps, 20.0);
        // 密钥不在 config 里裸放，但要能通过受保护通道取到。
        assert!(resume.config.key.is_none(), "config 本身不带密钥");
        assert_eq!(resume.key(), Some("FIXED-KEY-42"));
        assert_eq!(
            resume.config_with_key().key.as_deref(),
            Some("FIXED-KEY-42"),
            "交给 connect_restored 的配置要带上交接来的密钥"
        );
        // 身份完整可用。
        assert_eq!(resume.identity.restore.session_id, "guisess7");
        assert!(resume.identity.restore.is_usable());
        // Debug 不泄露密钥。
        assert!(!format!("{resume:?}").contains("FIXED-KEY-42"));
    }

    #[test]
    fn test_env_channel_resolves_the_same_handoff() {
        // 回滚路径只走环境变量（入口拿不到 argv）。
        let _env = env_guard();
        let dir = TmpDir::new("env");
        let relay = "http://127.0.0.1:39998";
        let handoff = write_handoff(
            &dir,
            "guisess8",
            relay,
            Some(snapshot("guisess8", relay)),
            None,
        );
        std::env::set_var(RESTORE_ENV, &handoff);
        let resume = resolve_gui_resume(None)
            .expect("环境变量里的交接必须能解析")
            .expect("必须返回恢复");
        assert_eq!(resume.identity.restore.session_id, "guisess8");
        std::env::remove_var(RESTORE_ENV);
    }

    #[test]
    fn test_command_line_handoff_is_not_a_gui_resume() {
        // 命令行启动的升级没有 GUI 快照：GUI 侧无从得知"配置是什么"，
        // 必须明确失败，而不是按默认值连上去变成另一台设备。
        let _env = env_guard();
        std::env::remove_var(RESTORE_ENV);
        let dir = TmpDir::new("cli");
        let handoff = write_handoff(&dir, "clisess1", "http://127.0.0.1:39997", None, None);
        let err = resolve_gui_resume(Some(&handoff.to_string_lossy()))
            .expect_err("没有 GUI 快照时必须报错");
        let msg = format!("{err:#}");
        assert!(msg.contains("snapshot"), "{msg}");
    }

    #[test]
    fn test_missing_or_corrupt_handoff_is_fail_closed() {
        let _env = env_guard();
        std::env::remove_var(RESTORE_ENV);

        // 1. 指向不存在的文件。
        let dir = TmpDir::new("missing");
        let missing = dir.path().join("nope.json");
        assert!(
            resolve_gui_resume(Some(&missing.to_string_lossy())).is_err(),
            "明确要求恢复但文件不存在时必须报错，不能退化成全新设备"
        );

        // 2. 内容损坏。
        let corrupt = dir.path().join("corrupt.json");
        std::fs::write(&corrupt, b"{not json").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&corrupt, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(
            resolve_gui_resume(Some(&corrupt.to_string_lossy())).is_err(),
            "损坏的交接必须拒绝"
        );

        // 3. 显式给了参数但为空：也算"要求恢复"，同样不许当普通启动。
        assert!(
            resolve_gui_resume(Some("")).is_err(),
            "空的恢复参数不能被当成没有恢复意图"
        );
    }

    #[test]
    fn test_snapshot_identity_mismatch_is_rejected() {
        // 交接文件被改过：快照里的 sid 与身份不一致 ⇒ fail-closed。
        let _env = env_guard();
        std::env::remove_var(RESTORE_ENV);
        let dir = TmpDir::new("mismatch");
        let relay = "http://127.0.0.1:39996";
        // 构造时两者已经一致，直接改盘上的 JSON 模拟损坏。
        let handoff = write_handoff(
            &dir,
            "guisess1",
            relay,
            Some(snapshot("guisess1", relay)),
            None,
        );
        let mut json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&handoff).unwrap()).unwrap();
        json["gui"]["session_id"] = serde_json::json!("attacker1");
        let tampered = dir.path().join("tampered.json");
        std::fs::write(&tampered, serde_json::to_vec(&json).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tampered, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(
            resolve_gui_resume(Some(&tampered.to_string_lossy())).is_err(),
            "快照与身份不一致必须拒绝（不能用默认值掩盖）"
        );
    }

    #[test]
    fn test_snapshot_converts_back_to_a_validatable_ui_config() {
        // 交接快照 → UiConfig 必须能通过 GUI 自己的校验（字段映射没有漂移）。
        let _env = env_guard();
        std::env::remove_var(RESTORE_ENV);
        let dir = TmpDir::new("roundtrip");
        let relay = "http://127.0.0.1:39995";
        let handoff = write_handoff(
            &dir,
            "guisess5",
            relay,
            Some(snapshot("guisess5", relay)),
            None,
        );
        let resume = resolve_gui_resume(Some(&handoff.to_string_lossy()))
            .unwrap()
            .unwrap();
        resume
            .config
            .validate()
            .expect("交接恢复出来的配置必须通过 GUI 校验");
        assert_eq!(resume.config.token_type, "rw");
        assert_eq!(resume.config.shell, "/bin/sh");
    }
}
