//! UI 配置模型（MYS-2806 阶段4）：`shell-remote-ui` 无参数启动后，用户要填的
//! 全部 agent 配置都活在这里。
//!
//! 与 CLI 参数的关系：字段与 `shell-remote-ui agent` 的命令行参数同名同义
//! （`--relay-url` / `--desktop-fps` …），默认值也保持一致；`view` 与
//! `upgrade-restore` 是内部调度入口，不属于用户配置，故意不在这里。
//!
//! 三条硬约束（任务卡）：
//! 1. **密钥只在内存**：`key` 标 `#[serde(skip)]`，不落盘、不进日志、不进错误
//!    文本，`Debug` 也是手写的脱敏版本。
//! 2. **不静默修正**：`validate()` 只判断并报错，绝不夹带"帮你改成合法值"。
//! 3. **失败可见且不破坏现场**：`load()` 对损坏/未知 schema 返回错误，不覆盖
//!    用户的文件；`save()` 校验通过后原子替换（临时文件 + rename）。
//!
//! 仅在 `gui` feature 下编译：CLI 二进制（lean 构建）不需要它。

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};
use serde::{Deserialize, Serialize};

/// 配置文件 schema 版本。字段增删必须同步 +1：旧版读到不认识的版本会明确
/// 报错，而不是把用户配置读成一半默认值再写回去。
pub const SCHEMA_VERSION: u32 = 1;

/// 配置文件名（位于用户目录下的 `.shell-remote/`，与日志同目录）。
pub const FILE_NAME: &str = "ui-config.json";

/// 码率上限（kbps）。1 Gbps 对软件编码的桌面流是绝对上限；同时保证
/// `kbps × 1000` 不会溢出 u64（1e6 × 1000 = 1e9）。
pub const MAX_BITRATE_KBPS: u64 = 1_000_000;

/// 可选的桌面抓屏后端（与 `agent::desktop::capture::open_source` 的取值一致）。
const CAPTURE_BACKENDS: &[&str] = &["auto", "dxgi", "gdi", "windows", "x11", "wayland", "none"];
/// 可选编码器（MYS-954 之后只剩这两种）。
const CODECS: &[&str] = &["av1", "h264"];
/// 编码质量档（rustdesk 同款三档）。
const QUALITIES: &[&str] = &["speed", "balanced", "best"];
/// 权限组合（`proto::TokenType` 的字符串形式）。
const TOKEN_TYPES: &[&str] = &["rw", "ro", "both"];

/// 无参数启动后由配置窗口填写、并原样交给 agent 的全部设置。
///
/// `Debug` 手写实现：`key` 只输出 `<redacted>`。禁止 `derive(Debug)`——那会把
/// 固定密钥随任意一句 `{:?}` 打印进日志。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiConfig {
    /// Relay 地址（只接受 http/https）。**默认空串**：界面显示 placeholder，
    /// 绝不默认连 localhost——那会让用户以为自己已经连上了。
    pub relay_url: String,
    /// 信任自签证书（relay 默认自签；不勾选则注册握手直接失败）。
    pub relay_insecure: bool,
    /// 固定认证密钥。**只在内存**：`serde(skip)` 让它既不写盘也读不回来
    /// （保存后由界面内存里那份副本继续使用，重启后需要重新输入）。
    #[serde(skip)]
    pub key: Option<String>,
    /// 文件管理根目录；None = 用户主目录。
    pub root: Option<String>,
    /// 权限：rw / ro / both。
    pub token_type: String,
    /// Shell 路径（Windows `cmd.exe`，其它 `/bin/bash` 或 `$SHELL`）。
    pub shell: String,
    /// 固定会话 id（5-20 位 ASCII 字母数字）；None = relay 随机分配。
    pub session_id: Option<String>,
    /// 关闭命令日志。
    pub no_cmd_log: bool,
    /// 桌面抓屏后端：auto / dxgi / gdi / windows / x11 / wayland / none。
    pub desktop_capture: String,
    /// 桌面编码器：av1 / h264。
    pub desktop_codec: String,
    /// 编码帧率（1-60）。
    pub desktop_fps: f64,
    /// 抓帧上限（0 = 不限速）。
    pub desktop_capture_fps: f64,
    /// 最大码率（kbps；0 = 自动按 rustdesk 模型）。
    pub desktop_max_bitrate: u64,
    /// 编码质量档：speed / balanced / best。
    pub desktop_quality: String,
    /// 最小码率（kbps）。
    pub desktop_min_bitrate: u64,
    /// X11 display（None = $DISPLAY）。
    pub desktop_display: Option<String>,
    /// LAN 直连端口（0 = 不开启）。
    pub desktop_lan_port: u16,
}

/// UI 里 shell 的默认值：与 CLI 的 `default_value` + `env = "SHELL"` 等价。
fn default_shell() -> String {
    if cfg!(windows) {
        return "cmd.exe".to_string();
    }
    match std::env::var("SHELL") {
        Ok(s) if !s.trim().is_empty() => s,
        _ => "/bin/bash".to_string(),
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            // 空 = 让界面显示 placeholder；绝不预填 localhost。
            relay_url: String::new(),
            relay_insecure: false,
            key: None,
            root: None,
            token_type: "rw".to_string(),
            shell: default_shell(),
            session_id: None,
            no_cmd_log: false,
            desktop_capture: "auto".to_string(),
            desktop_codec: "av1".to_string(),
            desktop_fps: 30.0,
            desktop_capture_fps: 0.0,
            desktop_max_bitrate: 0,
            desktop_quality: "balanced".to_string(),
            desktop_min_bitrate: 80,
            desktop_display: None,
            desktop_lan_port: 0,
        }
    }
}

impl std::fmt::Debug for UiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiConfig")
            .field("relay_url", &self.relay_url)
            .field("relay_insecure", &self.relay_insecure)
            // 只说明"有没有填"，永远不输出密钥内容。
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .field("root", &self.root)
            .field("token_type", &self.token_type)
            .field("shell", &self.shell)
            .field("session_id", &self.session_id)
            .field("no_cmd_log", &self.no_cmd_log)
            .field("desktop_capture", &self.desktop_capture)
            .field("desktop_codec", &self.desktop_codec)
            .field("desktop_fps", &self.desktop_fps)
            .field("desktop_capture_fps", &self.desktop_capture_fps)
            .field("desktop_max_bitrate", &self.desktop_max_bitrate)
            .field("desktop_quality", &self.desktop_quality)
            .field("desktop_min_bitrate", &self.desktop_min_bitrate)
            .field("desktop_display", &self.desktop_display)
            .field("desktop_lan_port", &self.desktop_lan_port)
            .finish()
    }
}

impl UiConfig {
    /// 校验用户输入。只判断、不修正：任何一条不满足都返回可直接展示给用户的
    /// 中文原因，且**绝不包含密钥内容**。
    pub fn validate(&self) -> anyhow::Result<()> {
        self.validate_relay_url()?;

        if let Some(sid) = &self.session_id {
            if sid.trim().is_empty() {
                bail!("会话 id 为空：留空表示由 relay 随机分配，填写则必须是 5-20 位字母或数字");
            }
            if !crate::proto::is_valid_custom_session_id(sid) {
                bail!("会话 id「{sid}」不合法：必须是 5-20 位 ASCII 字母或数字");
            }
        }

        if !TOKEN_TYPES.contains(&self.token_type.as_str()) {
            bail!("权限「{}」不合法：只能是 rw、ro 或 both", self.token_type);
        }

        if self.shell.trim().is_empty() {
            bail!("Shell 不能为空（例如 /bin/bash 或 cmd.exe）");
        }
        if self.shell.contains('\0') {
            bail!("Shell 路径包含非法字符");
        }

        match &self.root {
            None => {}
            Some(root) => {
                if root.trim().is_empty() {
                    bail!("根目录为空：留空表示使用用户主目录");
                }
                if root.contains('\0') {
                    bail!("根目录包含非法字符");
                }
                if !Path::new(root).is_dir() {
                    // 提前挡住：否则 agent 只在注册之后才发现目录不可用，
                    // 界面会停在"重连中"而看不出原因。
                    bail!("根目录不存在或不是目录：{root}");
                }
            }
        }

        if let Some(key) = &self.key {
            if key.trim().is_empty() {
                bail!("固定密钥为空：请填写密钥，或留空让 relay 自动生成");
            }
            if key.len() > 512 {
                bail!("固定密钥过长（最多 512 字节）");
            }
        }

        if !CAPTURE_BACKENDS.contains(&self.desktop_capture.as_str()) {
            bail!(
                "桌面抓屏「{}」不合法：只能是 auto、dxgi、gdi、x11、wayland、none",
                self.desktop_capture
            );
        }
        if !CODECS.contains(&self.desktop_codec.as_str()) {
            bail!(
                "桌面编码「{}」不合法：只能是 av1 或 h264",
                self.desktop_codec
            );
        }
        if !QUALITIES.contains(&self.desktop_quality.as_str()) {
            bail!(
                "画质档「{}」不合法：只能是 speed、balanced 或 best",
                self.desktop_quality
            );
        }

        // NaN/∞ 直接拒（`contains` 对 NaN 恒 false，单独报错更清楚）。
        if !self.desktop_fps.is_finite() {
            bail!("桌面帧率必须是有限数值");
        }
        if !(1.0..=60.0).contains(&self.desktop_fps) {
            bail!("桌面帧率 {} 超出范围：只能填 1-60", self.desktop_fps);
        }
        if !self.desktop_capture_fps.is_finite() {
            bail!("抓帧上限必须是有限数值");
        }
        if self.desktop_capture_fps != 0.0 && !(1.0..=240.0).contains(&self.desktop_capture_fps) {
            bail!(
                "抓帧上限 {} 超出范围：0（不限速）或 1-240",
                self.desktop_capture_fps
            );
        }

        self.validate_bitrate("最小码率", self.desktop_min_bitrate, false)?;
        self.validate_bitrate("最大码率", self.desktop_max_bitrate, true)?;
        if self.desktop_max_bitrate != 0 && self.desktop_max_bitrate < self.desktop_min_bitrate {
            bail!(
                "最大码率（{} kbps）不能小于最小码率（{} kbps）",
                self.desktop_max_bitrate,
                self.desktop_min_bitrate
            );
        }

        if let Some(display) = &self.desktop_display {
            if display.trim().is_empty() {
                bail!("X11 display 为空：留空表示使用 $DISPLAY");
            }
            if display.contains('\0') {
                bail!("X11 display 包含非法字符");
            }
        }

        Ok(())
    }

    /// Relay 地址：http/https + 主机、禁内嵌凭据、禁查询串/片段。
    ///
    /// 报错不回显原值：内嵌凭据的 URL 本身就是敏感信息。
    fn validate_relay_url(&self) -> anyhow::Result<()> {
        let raw = self.relay_url.as_str();
        if raw.trim().is_empty() {
            bail!("请填写 Relay 地址（例如 https://relay.example.com）");
        }
        if raw != raw.trim() {
            bail!("Relay 地址首尾不能有空格");
        }
        if !raw.contains("://") {
            bail!("Relay 地址必须以 http:// 或 https:// 开头");
        }
        // `//` 之后必须直接是主机名：`https:///path` 会被 url crate 宽松地当成
        // 主机名 `path`（实测），那不是用户想要的地址。
        let after_scheme = raw.split_once("://").map(|(_, rest)| rest).unwrap_or("");
        if after_scheme.is_empty() || after_scheme.starts_with('/') {
            bail!("Relay 地址缺少主机名（格式：http(s)://主机[:端口]）");
        }
        let parsed = url::Url::parse(raw).map_err(|e| anyhow!("Relay 地址不是合法 URL：{e}"))?;
        match parsed.scheme() {
            "http" | "https" => {}
            "ws" | "wss" => bail!(
                "Relay 地址请用 http:// 或 https://（程序内部自动推导 ws/wss），不要直接填 ws:// 或 wss://"
            ),
            other => bail!("Relay 地址协议「{other}」不支持：只能用 http 或 https"),
        }
        if parsed.host_str().map_or(true, |h| h.trim().is_empty()) {
            bail!("Relay 地址缺少主机名");
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            bail!("Relay 地址不能内嵌用户名或密码：请把凭据从地址里去掉");
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            bail!("Relay 地址不能带查询串或片段");
        }
        Ok(())
    }

    fn validate_bitrate(&self, label: &str, kbps: u64, zero_means_auto: bool) -> anyhow::Result<()> {
        if kbps == 0 {
            if zero_means_auto {
                return Ok(());
            }
            bail!("{label}必须大于 0 kbps");
        }
        if kbps > MAX_BITRATE_KBPS {
            bail!("{label} {kbps} kbps 过大：上限 {MAX_BITRATE_KBPS} kbps");
        }
        // 码率在 agent 内部以 bps 传输；这里显式拦溢出，不让它在别处悄悄回绕。
        if kbps.checked_mul(1000).is_none() {
            bail!("{label} {kbps} kbps 换算成 bps 会溢出");
        }
        Ok(())
    }

    /// 默认配置文件路径：`<用户目录>/.shell-remote/ui-config.json`。
    ///
    /// 用户目录沿用 `agent::home_dir()` 的语义（`HOME` → `%USERPROFILE%`），
    /// 与日志目录、文件管理默认根目录是同一条规则，不随启动 cwd 漂移。
    /// 两者都没有时退化成相对路径（与日志目录的既有边界一致，已在文档注明）。
    pub fn default_path() -> PathBuf {
        default_path_from(
            std::env::var("HOME").ok(),
            std::env::var("USERPROFILE").ok(),
        )
    }

    /// 读取配置。
    ///
    /// - 文件不存在 → 返回默认配置（首次启动的正常路径，不创建任何文件）；
    /// - 内容损坏 / 缺 schema_version / 版本不认识 → 返回**可见错误**，
    ///   且绝不回写（用户原文件保持不变，可以自己修或备份）。
    pub fn load(path: &Path) -> anyhow::Result<UiConfig> {
        let raw = match std::fs::read(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(UiConfig::default()),
            Err(e) => bail!("读取配置失败 {}: {e}", path.display()),
        };
        if raw.iter().all(u8::is_ascii_whitespace) {
            bail!("配置文件为空：{}", path.display());
        }
        let value: serde_json::Value = serde_json::from_slice(&raw)
            .map_err(|e| anyhow!("配置文件不是合法 JSON（{}）: {e}", path.display()))?;
        let version = value
            .get("schema_version")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                anyhow!(
                    "配置文件缺少 schema_version 字段（{}）：本程序只认版本 {SCHEMA_VERSION}",
                    path.display()
                )
            })?;
        if version != SCHEMA_VERSION as u64 {
            bail!(
                "配置文件 schema 版本 {version} 不受支持（本程序只认 {SCHEMA_VERSION}）：{}",
                path.display()
            );
        }
        let file: ConfigFile = serde_json::from_value(value)
            .map_err(|e| anyhow!("配置内容不合法（{}）: {e}", path.display()))?;
        Ok(file.settings)
    }

    /// 校验后原子保存非密钥设置。
    ///
    /// 校验失败直接返回错误且**不动**磁盘上的原文件；写入走"同目录临时文件
    /// （0600）+ rename"，读端要么看到旧文件、要么看到完整的新文件。
    /// 不读也不改 `HOME` / `CODEX_HOME` 等环境变量。
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        self.validate()?;
        let body = serde_json::to_vec_pretty(&ConfigFile {
            schema_version: SCHEMA_VERSION,
            // `key` 是 `#[serde(skip)]`，序列化时自动消失，无需手动清空。
            settings: self.clone(),
        })
        .map_err(|e| anyhow!("无法序列化配置: {e}"))?;

        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow!("创建配置目录失败 {}: {e}", dir.display()))?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(FILE_NAME);
        let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);

        let write = (|| -> std::io::Result<()> {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut file = opts.open(&tmp)?;
            std::io::Write::write_all(&mut file, &body)?;
            file.sync_all()
        })();
        if let Err(e) = write {
            let _ = std::fs::remove_file(&tmp);
            return Err(anyhow!("写入配置失败 {}: {e}", tmp.display()));
        }
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            anyhow!("替换配置文件失败 {}: {e}", path.display())
        })?;
        // 目录项也要落盘，否则掉电后可能只剩临时文件的残留名。
        #[cfg(unix)]
        if let Ok(d) = std::fs::File::open(&dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }
}

/// 落盘结构：`schema_version` 包在外面，字段增删只动 `settings`。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    schema_version: u32,
    settings: UiConfig,
}

fn default_path_from(home: Option<String>, userprofile: Option<String>) -> PathBuf {
    PathBuf::from(crate::agent::home_dir_from(home, userprofile))
        .join(".shell-remote")
        .join(FILE_NAME)
}

#[cfg(all(test, feature = "desktop"))]
mod desktop_cfg_tests {
    use super::*;

    #[test]
    fn test_desktop_config_matches_cli_mapping() {
        let cfg = UiConfig {
            desktop_capture: "none".into(),
            desktop_codec: "h264".into(),
            desktop_fps: 15.0,
            desktop_capture_fps: 12.0,
            desktop_min_bitrate: 200,
            desktop_max_bitrate: 4000,
            desktop_quality: "best".into(),
            desktop_display: Some(":0".into()),
            desktop_lan_port: 39516,
            ..Default::default()
        };
        let d = cfg.to_desktop_config();
        assert_eq!(d.capture, "none");
        assert_eq!(d.codec, "h264");
        assert_eq!(d.fps, 15.0);
        assert_eq!(d.capture_fps, 12.0);
        // CLI 的 kbps → bps 换算必须一致（bin 里是 `* 1000`）。
        assert_eq!(d.min_bps, 200_000);
        assert_eq!(d.max_bps, 4_000_000);
        assert_eq!(d.quality, crate::agent::desktop::encoder::QUALITY_BEST);
        assert_eq!(d.display.as_deref(), Some(":0"));
        assert_eq!(d.lan_port, 39516);
        // UI 里没有灰度开关；与现有 UI 二进制一样固定 false。
        assert!(!d.monochrome);
        assert_eq!(d.lan_addr, None);
    }

    #[test]
    fn test_desktop_quality_unknown_falls_back_like_cli() {
        // CLI 的 match 是 `_ => BALANCED`；validate 会先拦住非法值，
        // 这里保证映射函数本身与 CLI 同构（GUI 尚未校验时也不会 panic）。
        let cfg = UiConfig {
            desktop_quality: "weird".into(),
            ..Default::default()
        };
        assert_eq!(
            cfg.to_desktop_config().quality,
            crate::agent::desktop::encoder::QUALITY_BALANCED
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个能通过 validate 的基准配置。
    fn valid() -> UiConfig {
        UiConfig {
            relay_url: "https://relay.example.com".into(),
            ..Default::default()
        }
    }

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn test_defaults_match_ui_cli() {
        let d = UiConfig::default();
        assert_eq!(d.relay_url, "", "界面用 placeholder，不默认连 localhost");
        assert!(!d.relay_insecure);
        assert_eq!(d.key, None);
        assert_eq!(d.root, None);
        assert_eq!(d.token_type, "rw");
        assert_eq!(d.session_id, None);
        assert!(!d.no_cmd_log);
        assert_eq!(d.desktop_capture, "auto");
        assert_eq!(d.desktop_codec, "av1");
        assert_eq!(d.desktop_fps, 30.0);
        assert_eq!(d.desktop_capture_fps, 0.0);
        assert_eq!(d.desktop_max_bitrate, 0);
        assert_eq!(d.desktop_quality, "balanced");
        assert_eq!(d.desktop_min_bitrate, 80);
        assert_eq!(d.desktop_display, None);
        assert_eq!(d.desktop_lan_port, 0);
        // 平台 shell 默认与 UI CLI 的 `default_value` 一致。
        if cfg!(windows) {
            assert_eq!(d.shell, "cmd.exe");
        } else {
            let expected = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
            assert_eq!(d.shell, expected);
        }
        assert!(!d.shell.is_empty());
    }

    #[test]
    fn test_all_fields_roundtrip_and_key_is_not_persisted() {
        let dir = tempdir();
        let path = dir.path().join("ui-config.json");
        let cfg = UiConfig {
            relay_url: "https://relay.example.com:8443/prefix".into(),
            relay_insecure: true,
            key: Some("s3cr3t-key-material".into()),
            root: Some(dir.path().to_string_lossy().into_owned()),
            token_type: "both".into(),
            shell: "/bin/sh".into(),
            session_id: Some("mysdev01".into()),
            no_cmd_log: true,
            desktop_capture: "x11".into(),
            desktop_codec: "h264".into(),
            desktop_fps: 12.5,
            desktop_capture_fps: 45.0,
            desktop_max_bitrate: 4000,
            desktop_quality: "speed".into(),
            desktop_min_bitrate: 120,
            desktop_display: Some(":1".into()),
            desktop_lan_port: 39516,
        };
        cfg.save(&path).expect("valid config must save");

        let raw = std::fs::read_to_string(&path).expect("read back");
        assert!(
            !raw.contains("s3cr3t-key-material"),
            "密钥绝不能写进配置文件: {raw}"
        );
        assert!(
            !raw.contains("\"key\""),
            "连 key 字段都不该出现在 JSON 里: {raw}"
        );
        assert!(raw.contains("\"schema_version\": 1"));

        let loaded = UiConfig::load(&path).expect("load");
        assert_eq!(loaded.key, None, "密钥不落盘，读回来必须为空");
        // 其余字段逐个往返（key 除外）。
        let mut expect = cfg.clone();
        expect.key = None;
        assert_eq!(loaded, expect);
    }

    #[test]
    fn test_missing_file_returns_default_without_creating_it() {
        let dir = tempdir();
        let path = dir.path().join("nested").join("ui-config.json");
        let cfg = UiConfig::load(&path).expect("missing file is not an error");
        assert_eq!(cfg, UiConfig::default());
        assert!(!path.exists(), "load 不得创建文件");
    }

    #[test]
    fn test_corrupt_file_is_visible_error_and_never_overwritten() {
        for body in [
            "not json at all",
            "{\"schema_version\": 1, \"settings\": {}}",
            "{\"schema_version\": 1, \"settings\": {\"desktop_fps\": \"thirty\"}}",
            "",
            "   \n",
        ] {
            let dir = tempdir();
            let path = dir.path().join("ui-config.json");
            std::fs::write(&path, body).unwrap();
            let err = UiConfig::load(&path)
                .err()
                .unwrap_or_else(|| panic!("损坏内容必须报错: {body:?}"));
            assert!(
                !err.to_string().trim().is_empty(),
                "错误要有可读原因（{body:?}）"
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                body,
                "load 绝不能回写/修复原文件"
            );
        }
    }

    #[test]
    fn test_unknown_schema_version_rejected() {
        let dir = tempdir();
        let path = dir.path().join("ui-config.json");
        let body = serde_json::json!({
            "schema_version": 999,
            "settings": valid()
        })
        .to_string();
        std::fs::write(&path, &body).unwrap();
        let err = UiConfig::load(&path).err().expect("unknown schema");
        assert!(err.to_string().contains("999"), "错误要带上实际版本: {err}");
        assert!(err.to_string().contains("schema"), "错误要说明是 schema 问题: {err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
    }

    #[test]
    fn test_missing_schema_version_rejected() {
        let dir = tempdir();
        let path = dir.path().join("ui-config.json");
        std::fs::write(&path, "{\"settings\": {}}").unwrap();
        let err = UiConfig::load(&path).err().expect("missing version");
        assert!(
            err.to_string().contains("schema_version"),
            "错误要指明缺什么: {err}"
        );
    }

    #[test]
    fn test_unknown_field_rejected() {
        let dir = tempdir();
        let path = dir.path().join("ui-config.json");
        let mut value = serde_json::to_value(ConfigFile {
            schema_version: SCHEMA_VERSION,
            settings: valid(),
        })
        .unwrap();
        value["settings"]["unknown_setting"] = serde_json::json!(1);
        std::fs::write(&path, value.to_string()).unwrap();
        assert!(
            UiConfig::load(&path).is_err(),
            "同版本下的未知字段（多半是拼写错误）必须可见报错"
        );
    }

    #[test]
    fn test_save_rejects_invalid_and_keeps_existing_file() {
        let dir = tempdir();
        let path = dir.path().join("ui-config.json");
        valid().save(&path).expect("seed");
        let before = std::fs::read_to_string(&path).unwrap();

        let mut bad = valid();
        bad.desktop_fps = 500.0;
        let err = bad.save(&path).err().expect("invalid config must not save");
        assert!(err.to_string().contains("帧率"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "校验失败不得改动原文件"
        );
    }

    #[test]
    fn test_save_creates_parent_dir_and_leaves_no_temp_files() {
        let dir = tempdir();
        let path = dir.path().join("a").join("b").join("ui-config.json");
        valid().save(&path).expect("save into a fresh tree");
        assert!(path.is_file());
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "不得留下临时文件: {leftovers:?}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "配置文件按用户私有权限落地");
        }
    }

    #[test]
    fn test_save_overwrites_atomically_and_load_sees_new_value() {
        let dir = tempdir();
        let path = dir.path().join("ui-config.json");
        valid().save(&path).unwrap();
        let mut cfg = valid();
        cfg.relay_url = "http://127.0.0.1:3000".into();
        cfg.desktop_fps = 24.0;
        cfg.save(&path).unwrap();
        let loaded = UiConfig::load(&path).unwrap();
        assert_eq!(loaded.relay_url, "http://127.0.0.1:3000");
        assert_eq!(loaded.desktop_fps, 24.0);
    }

    #[test]
    fn test_default_path_is_under_user_home_not_cwd() {
        // HOME 语义：与日志目录同一条规则，钉死在用户目录下。
        let expected = PathBuf::from("/home/u")
            .join(".shell-remote")
            .join(FILE_NAME);
        assert_eq!(
            default_path_from(Some("/home/u".into()), Some("/ignored".into())),
            expected
        );
        // Windows 常态：只有 USERPROFILE。
        assert_eq!(
            default_path_from(None, Some(r"C:\Users\u".into())),
            PathBuf::from(r"C:\Users\u").join(".shell-remote").join(FILE_NAME)
        );
        // 运行时路径：必须绝对（本机 HOME 存在），且以 .shell-remote/ui-config.json 结尾。
        let path = UiConfig::default_path();
        assert!(path.ends_with(FILE_NAME));
        assert!(path.to_string_lossy().contains(".shell-remote"));
        if std::env::var("HOME").is_ok() || std::env::var("USERPROFILE").is_ok() {
            assert!(path.is_absolute(), "用户目录存在时不得退化成相对路径: {path:?}");
        }
    }

    #[test]
    fn test_validate_accepts_realistic_configs() {
        valid().validate().expect("https + 默认值");
        let plain = UiConfig {
            relay_url: "http://127.0.0.1:3000".into(),
            ..Default::default()
        };
        plain.validate().expect("http 本机地址");
        let with_path = UiConfig {
            relay_url: "https://mu.zztweb.top/sr".into(),
            ..Default::default()
        };
        with_path.validate().expect("允许路径前缀");
        let no_desktop = UiConfig {
            relay_url: "https://relay.example.com".into(),
            desktop_capture: "none".into(),
            desktop_max_bitrate: 80,
            desktop_min_bitrate: 80,
            ..Default::default()
        };
        no_desktop.validate().expect("关闭桌面 + 码率相等");
    }

    #[test]
    fn test_validate_rejects_bad_relay_urls() {
        for url in [
            "",
            "   ",
            "relay.example.com:3000",
            "ws://relay.example.com",
            "wss://relay.example.com",
            "ftp://relay.example.com",
            "https://",
            "https:///path",
            "https://user:pass@relay.example.com",
            "https://user@relay.example.com",
            "https://relay.example.com/?x=1",
            "https://relay.example.com/#frag",
            " https://relay.example.com",
            "https://relay.example.com ",
        ] {
            let cfg = UiConfig {
                relay_url: url.into(),
                ..Default::default()
            };
            let err = cfg
                .validate()
                .err()
                .unwrap_or_else(|| panic!("非法 relay 地址必须被拦下: {url:?}"));
            let msg = err.to_string();
            assert!(!msg.trim().is_empty(), "要有可读原因: {url:?}");
            // 内嵌凭据的 URL 本身就是秘密，错误文本不得回显它。
            if url.contains('@') {
                assert!(
                    !msg.contains("pass") && !msg.contains("user:"),
                    "错误不得回显内嵌凭据: {msg}"
                );
            }
        }
    }

    #[test]
    fn test_validate_rejects_bad_session_id_and_token_type() {
        for sid in ["ab", "abcd", "a1b2c3d4e5f6g7h8i9j0k", "ab cd", "dev_01", "  "] {
            let cfg = UiConfig {
                session_id: Some(sid.into()),
                ..valid()
            };
            assert!(cfg.validate().is_err(), "非法会话 id 必须拦下: {sid:?}");
        }
        assert!(UiConfig {
            session_id: Some("mysdev01".into()),
            ..valid()
        }
        .validate()
        .is_ok());

        for tt in ["", "RW", "readonly", "rw "] {
            let cfg = UiConfig {
                token_type: tt.into(),
                ..valid()
            };
            assert!(cfg.validate().is_err(), "非法权限必须拦下: {tt:?}");
        }
        for tt in ["rw", "ro", "both"] {
            let cfg = UiConfig {
                token_type: tt.into(),
                ..valid()
            };
            cfg.validate().unwrap_or_else(|e| panic!("{tt} 应合法: {e}"));
        }
    }

    #[test]
    fn test_validate_rejects_bad_desktop_enum_values() {
        for (field, values) in [
            (
                "capture",
                vec!["", "AUTO", "desktop", "vnc", "x11 "],
            ),
            ("codec", vec!["", "AV1", "vp9", "vp8", "h265"]),
            ("quality", vec!["", "BEST", "high", "balanced "]),
        ] {
            for v in values {
                let mut cfg = valid();
                match field {
                    "capture" => cfg.desktop_capture = v.into(),
                    "codec" => cfg.desktop_codec = v.into(),
                    "quality" => cfg.desktop_quality = v.into(),
                    _ => unreachable!(),
                }
                assert!(
                    cfg.validate().is_err(),
                    "{field}={v:?} 必须被拦下（不做大小写/空白静默修正）"
                );
            }
        }
    }

    #[test]
    fn test_validate_fps_and_bitrate_bounds() {
        for fps in [0.0, 0.5, 60.5, 1000.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let cfg = UiConfig {
                desktop_fps: fps,
                ..valid()
            };
            assert!(cfg.validate().is_err(), "帧率 {fps} 必须被拦下");
        }
        for fps in [1.0, 30.0, 60.0] {
            let cfg = UiConfig {
                desktop_fps: fps,
                ..valid()
            };
            cfg.validate().unwrap_or_else(|e| panic!("帧率 {fps} 应合法: {e}"));
        }

        let mut nan_capture = valid();
        nan_capture.desktop_capture_fps = f64::NAN;
        assert!(nan_capture.validate().is_err(), "抓帧上限 NaN 必须拦下");
        for cap in [0.0, 1.0, 240.0] {
            let cfg = UiConfig {
                desktop_capture_fps: cap,
                ..valid()
            };
            cfg.validate().unwrap_or_else(|e| panic!("抓帧上限 {cap} 应合法: {e}"));
        }
        for cap in [-1.0, 240.5, 10000.0] {
            let cfg = UiConfig {
                desktop_capture_fps: cap,
                ..valid()
            };
            assert!(cfg.validate().is_err(), "抓帧上限 {cap} 必须拦下");
        }

        // 最小码率：0 不行（它有具体含义）；超大不行。
        let zero_min = UiConfig {
            desktop_min_bitrate: 0,
            ..valid()
        };
        assert!(zero_min.validate().is_err());
        let huge = UiConfig {
            desktop_min_bitrate: MAX_BITRATE_KBPS + 1,
            ..valid()
        };
        assert!(huge.validate().is_err());
        let huge_max = UiConfig {
            desktop_max_bitrate: MAX_BITRATE_KBPS + 1,
            ..valid()
        };
        assert!(huge_max.validate().is_err());
        // 最大 < 最小 = 单位/输入错误，明确拒绝而不是悄悄对调。
        let inverted = UiConfig {
            desktop_min_bitrate: 4000,
            desktop_max_bitrate: 800,
            ..valid()
        };
        assert!(inverted.validate().is_err());
        // 0 = 自动，合法。
        let auto = UiConfig {
            desktop_max_bitrate: 0,
            ..valid()
        };
        auto.validate().expect("0 表示自动码率");
    }

    #[test]
    fn test_validate_root_shell_and_key() {
        let dir = tempdir();
        let ok = UiConfig {
            root: Some(dir.path().to_string_lossy().into_owned()),
            shell: "/bin/sh".into(),
            key: Some("k".into()),
            ..valid()
        };
        ok.validate().expect("存在的目录 + 非空 shell/key");

        let empty_root = UiConfig {
            root: Some("  ".into()),
            ..valid()
        };
        assert!(empty_root.validate().is_err());
        let missing_root = UiConfig {
            root: Some(dir.path().join("nope").to_string_lossy().into_owned()),
            ..valid()
        };
        assert!(missing_root.validate().is_err(), "不存在的根目录要提前拦下");
        let empty_shell = UiConfig {
            shell: "  ".into(),
            ..valid()
        };
        assert!(empty_shell.validate().is_err());
        let empty_key = UiConfig {
            key: Some("   ".into()),
            ..valid()
        };
        assert!(empty_key.validate().is_err());
    }

    #[test]
    fn test_validation_errors_never_leak_the_key() {
        let secret = "TOP-SECRET-KEY-9f3a";
        // 挑几种会失败的组合：错误文本里都不许出现密钥。
        let cases = [
            UiConfig {
                relay_url: "nonsense".into(),
                key: Some(secret.into()),
                ..Default::default()
            },
            UiConfig {
                desktop_fps: 999.0,
                key: Some(secret.into()),
                ..valid()
            },
            UiConfig {
                token_type: "root".into(),
                key: Some(secret.into()),
                ..valid()
            },
        ];
        for cfg in cases {
            let err = cfg.validate().err().expect("必须失败");
            let msg = format!("{err:#}");
            assert!(!msg.contains(secret), "错误文本泄露了密钥: {msg}");
            // Debug 也必须脱敏。
            let dbg = format!("{cfg:?}");
            assert!(!dbg.contains(secret), "Debug 泄露了密钥: {dbg}");
            assert!(dbg.contains("redacted"), "Debug 应标明密钥已遮蔽: {dbg}");
        }
    }
}

#[cfg(feature = "desktop")]
impl UiConfig {
    /// 转成 agent 侧的桌面配置。映射与 `src/bin/shell-remote-ui.rs` 现有
    /// `Command::Agent` 分支逐字段一致（kbps → bps、质量档常量）；
    /// GUI 接入时直接调用，避免两处漂移。
    pub fn to_desktop_config(&self) -> crate::agent::desktop::DesktopConfig {
        crate::agent::desktop::DesktopConfig {
            capture: self.desktop_capture.clone(),
            codec: self.desktop_codec.clone(),
            fps: self.desktop_fps,
            capture_fps: self.desktop_capture_fps,
            min_bps: self.desktop_min_bitrate * 1000,
            max_bps: self.desktop_max_bitrate * 1000,
            quality: match self.desktop_quality.as_str() {
                "speed" => crate::agent::desktop::encoder::QUALITY_SPEED,
                "best" => crate::agent::desktop::encoder::QUALITY_BEST,
                _ => crate::agent::desktop::encoder::QUALITY_BALANCED,
            },
            display: self.desktop_display.clone(),
            // UI 里没有灰度开关（与现有 UI 二进制一致）。
            monochrome: false,
            lan_port: self.desktop_lan_port,
            // 由 run_session 在 LanDesktop::spawn 之后注入。
            lan_addr: None,
        }
    }
}
