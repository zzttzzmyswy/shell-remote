//! 原生窗口状态面板（eframe/egui，glow 渲染 + winit X11）。
//!
//! 与 TUI 共享同一份 [`AgentStatus`]：展示连接状态/会话 id/token/relay 地址/
//! relay 延迟(心跳 RTT)/已运行时长；按钮"刷新 token"（会话 id 不变）与"退出"。
//!
//! 构建为 musl 全静态（X11 协议走纯 Rust 的 x11rb，libxkbcommon/libGL 运行时
//! dlopen，无链接期系统依赖）。仅在设备有显示环境（DISPLAY/Wayland）时由
//! shell-remote-ui 按 `--view` 选择启动。

use std::sync::Arc;

use eframe::egui;

use crate::status::{AgentStatus, Phase};

mod config_window;
pub use config_window::configure;

/// 以独立线程运行窗口事件循环（阻塞；Linux/Windows 均支持非主线程 winit）。
pub fn run(status: AgentStatus) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 400.0])
            .with_min_inner_size([420.0, 300.0])
            .with_title("shell-remote-ui"),
        ..Default::default()
    };
    eframe::run_native(
        "shell-remote-ui",
        options,
        Box::new(move |cc| {
            install_cjk_font(&cc.egui_ctx);
            Ok(Box::new(GuiApp {
                status,
                show_tokens: false,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}

struct GuiApp {
    status: AgentStatus,
    show_tokens: bool,
}

/// 凭据完整显示并可复制；长值换行，复制反馈只保存时间，不另存凭据。
fn credential_row(ui: &mut egui::Ui, label: &str, value: &str, visible: bool) {
    ui.push_id(label, |ui| {
        let id = ui.id().with("copied_at");
        let now = ui.input(|i| i.time);
        let copied = ui
            .ctx()
            .data(|d| d.get_temp::<f64>(id))
            .map(|at| now - at < 2.0)
            .unwrap_or(false);
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.strong(label);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            !value.is_empty(),
                            egui::Button::new(if copied { "已复制" } else { "复制" }),
                        )
                        .on_hover_text(format!("复制{label}"))
                        .clicked()
                    {
                        ui.ctx().copy_text(value.to_owned());
                        ui.ctx().data_mut(|d| d.insert_temp(id, now));
                        ui.ctx()
                            .request_repaint_after(std::time::Duration::from_secs(2));
                    }
                });
            });
            let text = if value.is_empty() {
                "—"
            } else if visible {
                value
            } else {
                "••••••••••••"
            };
            ui.add(egui::Label::new(egui::RichText::new(text).monospace()).wrap());
        });
    });
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let st = &self.status;
        let phase = st.phase();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.spacing_mut().scroll.floating = false;
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(4.0);
                ui.heading("shell-remote-ui 状态");
                ui.separator();

                let (phase_text, phase_color) = match phase {
                    Phase::Connected => (phase.label(), egui::Color32::from_rgb(0x2e, 0x9e, 0x44)),
                    Phase::Reconnecting => {
                        (phase.label(), egui::Color32::from_rgb(0xe6, 0x9c, 0x1a))
                    }
                    Phase::Connecting => (phase.label(), egui::Color32::from_rgb(0x1a, 0x8f, 0xc8)),
                    Phase::Starting => (phase.label(), egui::Color32::GRAY),
                };
                ui.horizontal(|ui| {
                    ui.label("状态：");
                    ui.colored_label(phase_color, phase_text);
                });
                credential_row(ui, "会话 ID", &st.session_id(), true);
                ui.label(format!("Relay：{}", st.relay_url()));
                ui.label(format!("延迟：{}", latency_text(st)));
                ui.label(format!("已运行：{}", uptime_text(st)));
                ui.separator();

                ui.horizontal(|ui| {
                    ui.strong("连接 Token");
                    ui.checkbox(&mut self.show_tokens, "显示");
                });
                let tokens = st.tokens();
                if tokens.is_empty() {
                    ui.weak("（未注册）");
                } else {
                    for (i, (tok, perm)) in tokens.iter().enumerate() {
                        ui.push_id(i, |ui| {
                            credential_row(
                                ui,
                                if perm == "ro" {
                                    "只读 Token"
                                } else {
                                    "读写 Token"
                                },
                                tok,
                                self.show_tokens,
                            );
                        });
                    }
                }
                ui.separator();

                ui.horizontal(|ui| {
                    if ui
                        .button("刷新 token（会话 id 不变）")
                        .on_hover_text("重新注册获取新 token，旧 token 立即失效")
                        .clicked()
                    {
                        tracing::info!("GUI: token refresh requested");
                        st.request_refresh();
                        self.show_tokens = false;
                    }
                    if ui
                        .button("退出")
                        .on_hover_text("停止 agent 并退出程序")
                        .clicked()
                    {
                        tracing::info!("GUI: shutdown requested");
                        st.request_shutdown();
                        // 关闭窗口 → run_native 返回 → 主线程等 agent 收尾后退出。
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.add_space(4.0);
                ui.weak("提示：仅本机查看用状态面板；终端/桌面转发照常经 relay 提供。");
            });
        });

        // 状态持续变化（延迟每心跳更新、重连状态）→ 500ms 重绘。
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
}

fn latency_text(st: &AgentStatus) -> String {
    st.latency_ms()
        .map(|ms| format!("{ms} ms（经心跳 ping，每 15s 更新）"))
        .unwrap_or_else(|| "—".to_string())
}

fn uptime_text(st: &AgentStatus) -> String {
    st.connected_secs()
        .map(|s| {
            let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
            format!("{h:02}:{m:02}:{sec:02}")
        })
        .unwrap_or_else(|| "—".to_string())
}

/// 加载系统 CJK 字体（egui 默认字体不含中文）。
///
/// 策略：先试固定候选路径（Windows 微软雅黑/宋体、常见发行版 Noto/WQY/文鼎），
/// 再递归扫描 `/usr/share/fonts` 按文件名关键词匹配（cjk/wqy/uming/ukai/zenhei/
/// han/hei/song/sarasa/droid fallback 等），命中第一个即注入字体族。
/// TTF/TTC/OTF 均可由 egui/ab_glyph 解析（ttc 取第一个 face）。
/// 找不到时中文显示为方块并告警。
fn install_cjk_font(ctx: &egui::Context) {
    const KEYWORDS: &[&str] = &[
        "cjk", "wqy", "uming", "ukai", "zenhei", "microhei", "sourcehan", "noto sans sc",
        "noto serif sc", "sarasa", "droid sans fallback", "simhei", "simsun", "msyh",
    ];

    let mut candidates: Vec<String> = vec![
        // Windows
        r"C:\Windows\Fonts\msyh.ttc".into(),
        r"C:\Windows\Fonts\msyhbd.ttc".into(),
        r"C:\Windows\Fonts\simsun.ttc".into(),
        // Linux：常见 Noto CJK 路径
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc".into(),
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc".into(),
        "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf".into(),
        // Linux：文泉驿/文鼎
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc".into(),
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc".into(),
        "/usr/share/fonts/truetype/arphic/uming.ttc".into(),
        "/usr/share/fonts/opentype/arphic/ukai.ttc".into(),
    ];

    // 递归扫描常见字体目录，按文件名关键词匹配兜底。
    let mut scanned: Vec<String> = Vec::new();
    for root in ["/usr/share/fonts", "/usr/local/share/fonts"] {
        scan_font_dirs(std::path::Path::new(root), &mut scanned, 3);
    }
    candidates.extend(
        scanned
            .into_iter()
            .filter(|p| {
                let name = p.to_lowercase();
                KEYWORDS.iter().any(|k| name.contains(k))
            })
            .take(4),
    );

    for path in candidates {
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => {
                let mut fonts = egui::FontDefinitions::default();
                fonts
                    .font_data
                    .insert("cjk".to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
                for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                    if let Some(list) = fonts.families.get_mut(&family) {
                        // 中文优先，其次保留默认拉丁字体做兜底。
                        list.push("cjk".to_owned());
                    }
                }
                ctx.set_fonts(fonts);
                tracing::info!("GUI: loaded CJK font from {}", path);
                return;
            }
            _ => {}
        }
    }
    tracing::warn!("GUI: no CJK system font found — 中文可能显示为方块");
}

/// 有界递归扫描目录下的字体文件（max_depth 限制，避免大目录耗时）。
fn scan_font_dirs(dir: &std::path::Path, out: &mut Vec<String>, max_depth: u32) {
    if max_depth == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            scan_font_dirs(&p, out, max_depth - 1);
        } else if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
            if matches!(ext.to_ascii_lowercase().as_str(), "ttf" | "otf" | "ttc") {
                out.push(p.to_string_lossy().into_owned());
            }
        }
    }
}
