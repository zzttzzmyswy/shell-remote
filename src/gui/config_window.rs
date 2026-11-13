//! No-argument desktop entry: edit configuration before starting any agent.
use crate::{status::Phase, ui_config::UiConfig, ui_controller::UiController};
use eframe::egui::{self, Color32, RichText};

pub fn configure(restore_flag: Option<&str>) -> anyhow::Result<()> {
    let resume = crate::ui_resume::resolve_gui_resume(restore_flag)?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("shell-remote · 连接设置")
            .with_inner_size([720.0, 660.0])
            .with_min_inner_size([480.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "shell-remote",
        options,
        Box::new(move |cc| {
            super::install_cjk_font(&cc.egui_ctx);
            cc.egui_ctx.all_styles_mut(|style| {
                style
                    .text_styles
                    .insert(egui::TextStyle::Heading, egui::FontId::proportional(24.0));
                style
                    .text_styles
                    .insert(egui::TextStyle::Body, egui::FontId::proportional(16.0));
                style
                    .text_styles
                    .insert(egui::TextStyle::Button, egui::FontId::proportional(15.0));
                style
                    .text_styles
                    .insert(egui::TextStyle::Small, egui::FontId::proportional(13.0));
                style.spacing.item_spacing = egui::vec2(10.0, 8.0);
                style.spacing.interact_size.y = 30.0;
                style.visuals.widgets.noninteractive.fg_stroke.color = if style.visuals.dark_mode {
                    Color32::from_gray(220)
                } else {
                    Color32::from_gray(40)
                };
            });
            Ok(Box::new(ConfigApp::new(resume)?))
        }),
    )
    .map_err(|e| anyhow::anyhow!("窗口初始化失败：{e}"))
}

struct ConfigApp {
    draft: UiConfig,
    controller: UiController,
    error: Option<String>,
    notice: Option<String>,
    show_key: bool,
    show_tokens: bool,
    stopping: bool,
    closing: bool,
    load_failed: bool,
}

impl ConfigApp {
    fn new(resume: Option<crate::ui_resume::GuiResume>) -> anyhow::Result<Self> {
        let mut controller = UiController::new();
        let (draft, error, notice) = if let Some(resume) = resume {
            let config = resume.config_with_key();
            controller.connect_restored(config.clone(), resume.identity)?;
            (config, None, Some("正在恢复升级前的连接…".into()))
        } else {
            match UiConfig::load(&UiConfig::default_path()) {
                Ok(config) => (config, None, None),
                Err(e) => (UiConfig::default(), Some(format!("{e:#}")), None),
            }
        };
        Ok(Self {
            load_failed: error.is_some(),
            draft,
            error,
            controller,
            notice,
            show_key: false,
            show_tokens: false,
            stopping: false,
            closing: false,
        })
    }

    fn message(&mut self, result: anyhow::Result<()>, success: &str) {
        match result {
            Ok(()) => {
                self.error = None;
                self.notice = Some(success.into());
            }
            Err(e) => {
                self.error = Some(crate::agent::redact_secret(
                    &format!("{e:#}"),
                    self.draft.key.as_deref(),
                ));
                self.notice = None;
            }
        }
    }

    fn connection_form(&mut self, ui: &mut egui::Ui) {
        section(ui, "连接", "填写服务器地址，再选择连接方式。");
        field(
            ui,
            "Relay 地址",
            &mut self.draft.relay_url,
            "https://relay.example.com",
        );
        ui.checkbox(&mut self.draft.relay_insecure, "信任此 Relay 的自签证书")
            .on_hover_text("仅在确认服务器可信、且使用自签证书时开启。");
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.label("固定密钥");
            ui.weak("可选");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.checkbox(&mut self.show_key, "显示");
            });
        });
        optional_text(
            ui,
            &mut self.draft.key,
            "留空，由服务器生成访问凭据",
            !self.show_key,
        );
        ui.small("密钥仅用于本次运行，不随设置保存。");
        ui.add_space(12.0);
        ui.columns(2, |cols| {
            cols[0].label("固定会话 ID · 可选");
            optional_text(
                &mut cols[0],
                &mut self.draft.session_id,
                "5–20 位字母或数字",
                false,
            );
            choice(
                &mut cols[1],
                "访问权限",
                &mut self.draft.token_type,
                &[
                    ("rw", "可读写"),
                    ("ro", "只读"),
                    ("both", "分别生成读写与只读凭据"),
                ],
            );
        });
    }

    fn advanced_form(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(RichText::new("高级设置").strong()).show(ui, |ui| {
            ui.add_space(8.0);
            ui.label("文件工作目录 · 可选");
            optional_text(ui, &mut self.draft.root, "留空使用当前用户的主目录", false);
            field(
                ui,
                "Shell",
                &mut self.draft.shell,
                "cmd.exe / powershell.exe / /bin/bash",
            );
            ui.checkbox(&mut self.draft.no_cmd_log, "关闭命令日志");
            ui.add_space(12.0);
            ui.separator();
            section(ui, "桌面共享", "未观看桌面时，不会持续编码视频。");
            ui.columns(2, |cols| {
                choice(
                    &mut cols[0],
                    "抓屏方式",
                    &mut self.draft.desktop_capture,
                    &[
                        ("auto", "自动"),
                        ("none", "关闭桌面共享"),
                        ("dxgi", "Windows · DXGI"),
                        ("gdi", "Windows · GDI"),
                        ("windows", "Windows · 自动"),
                        ("x11", "Linux · X11"),
                        ("wayland", "Linux · Wayland"),
                    ],
                );
                choice(
                    &mut cols[1],
                    "视频编码",
                    &mut self.draft.desktop_codec,
                    &[("av1", "AV1"), ("h264", "H.264")],
                );
            });
            ui.columns(2, |cols| {
                choice(
                    &mut cols[0],
                    "画质",
                    &mut self.draft.desktop_quality,
                    &[("speed", "流畅"), ("balanced", "均衡"), ("best", "清晰")],
                );
                cols[1].label("编码帧率");
                cols[1].add(
                    egui::DragValue::new(&mut self.draft.desktop_fps)
                        .range(1.0..=60.0)
                        .suffix(" fps"),
                );
            });
            ui.add_space(8.0);
            ui.columns(2, |cols| {
                cols[0].label("最低码率");
                cols[0].add(
                    egui::DragValue::new(&mut self.draft.desktop_min_bitrate)
                        .range(1..=1_000_000)
                        .suffix(" kbps"),
                );
                cols[1].label("最高码率 · 0 为自动");
                cols[1].add(
                    egui::DragValue::new(&mut self.draft.desktop_max_bitrate)
                        .range(0..=1_000_000)
                        .suffix(" kbps"),
                );
            });
            ui.add_space(8.0);
            ui.columns(2, |cols| {
                cols[0].label("抓帧上限 · 0 为不限");
                cols[0].add(
                    egui::DragValue::new(&mut self.draft.desktop_capture_fps)
                        .range(0.0..=240.0)
                        .suffix(" fps"),
                );
                cols[1].label("局域网端口 · 0 为关闭");
                cols[1]
                    .add(egui::DragValue::new(&mut self.draft.desktop_lan_port).range(0..=65535));
            });
            ui.add_space(8.0);
            ui.label("X11 Display · 可选");
            optional_text(
                ui,
                &mut self.draft.desktop_display,
                "留空使用系统 DISPLAY",
                false,
            );
        });
    }

    fn status_panel(&mut self, ui: &mut egui::Ui) {
        let Some(status) = self.controller.status() else {
            return;
        };
        ui.separator();
        section(ui, "连接状态", "连接期间设置已锁定；断开后可以修改。");
        ui.label(format!("Relay：{}", status.relay_url()));
        if status.phase() == Phase::Connected {
            super::credential_row(ui, "会话 ID", &status.session_id(), true);
            if let Some(ms) = status.latency_ms() {
                ui.weak(format!("延迟 {ms} ms"));
            }
            ui.horizontal(|ui| {
                ui.strong("访问凭据");
                ui.checkbox(&mut self.show_tokens, "显示");
            });
            for (i, (token, permission)) in status.tokens().into_iter().enumerate() {
                ui.push_id(i, |ui| {
                    super::credential_row(
                        ui,
                        if permission == "ro" {
                            "只读 Token"
                        } else {
                            "读写 Token"
                        },
                        &token,
                        self.show_tokens,
                    );
                });
            }
            if ui
                .small_button("重新生成访问凭据")
                .on_hover_text("旧凭据将失效，会话 ID 保持不变。")
                .clicked()
            {
                status.request_refresh();
                self.show_tokens = false;
            }
        }
    }
}

impl eframe::App for ConfigApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.controller.poll();
        let running = self.controller.is_running();
        if self.controller.status().map(|s| s.phase()) == Some(Phase::Connected)
            && matches!(
                self.notice.as_deref(),
                Some("正在连接，请稍候…" | "正在恢复升级前的连接…")
            )
        {
            self.notice = None;
        }
        if !running {
            self.stopping = false;
        }
        if ctx.input(|i| i.viewport().close_requested()) && running {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.closing = true;
            self.stopping = true;
            self.controller.disconnect();
        }
        if self.closing && !running {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let (label, color) = if self.stopping {
            ("正在断开", Color32::from_rgb(230, 156, 26))
        } else {
            match self.controller.status().map(|s| s.phase()) {
                Some(Phase::Connected) => ("已连接", Color32::from_rgb(46, 158, 68)),
                Some(Phase::Reconnecting) => ("正在重连", Color32::from_rgb(230, 156, 26)),
                Some(_) => ("正在连接", Color32::from_rgb(26, 143, 200)),
                None => ("未连接", ctx.style().visuals.weak_text_color()),
            }
        };
        egui::TopBottomPanel::top("header")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(24, 12)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("shell-remote");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.colored_label(color, format!("● {label}"));
                    });
                });
                ui.horizontal(|ui| {
                    ui.weak("远程终端与桌面共享");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        for (theme, name) in [
                            (egui::ThemePreference::Light, "浅色"),
                            (egui::ThemePreference::Dark, "深色"),
                            (egui::ThemePreference::System, "系统"),
                        ] {
                            if ui
                                .selectable_label(
                                    ctx.options(|o| o.theme_preference) == theme,
                                    name,
                                )
                                .clicked()
                            {
                                ctx.set_theme(theme);
                            }
                        }
                    });
                });
                ui.add_space(8.0);
            });
        egui::TopBottomPanel::bottom("actions")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(24, 12)),
            )
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let button = egui::Button::new(RichText::new("连接").strong())
                        .min_size(egui::vec2(88.0, 32.0));
                    if ui.add_enabled(!running, button).clicked() {
                        self.show_tokens = false;
                        let result = self.controller.connect(self.draft.clone());
                        self.message(result, "正在连接，请稍候…");
                    }
                    if ui
                        .add_enabled(
                            running && !self.stopping,
                            egui::Button::new("断开").min_size(egui::vec2(72.0, 32.0)),
                        )
                        .clicked()
                    {
                        self.controller.disconnect();
                        self.stopping = true;
                        self.notice = None;
                    }
                    if ui
                        .add_enabled(
                            !running && !self.load_failed,
                            egui::Button::new("保存设置").min_size(egui::vec2(88.0, 32.0)),
                        )
                        .clicked()
                    {
                        let result = self.draft.save(&UiConfig::default_path());
                        self.message(result, "设置已保存；密钥不会写入文件。");
                    }
                    if ui
                        .add_enabled(
                            !running,
                            egui::Button::new("恢复默认").min_size(egui::vec2(88.0, 32.0)),
                        )
                        .clicked()
                    {
                        self.draft = UiConfig::default();
                        self.show_key = false;
                        self.notice = Some("已恢复窗口内的默认值，未更改已保存文件。".into());
                    }
                });
                ui.add_space(6.0);
                ui.small(format!(
                    "v{} · 普通启动不自动连接",
                    env!("CARGO_PKG_VERSION")
                ));
                ui.add_space(6.0);
            });
        egui::CentralPanel::default().frame(egui::Frame::central_panel(&ctx.style()).inner_margin(24)).show(ctx, |ui| {
            ui.spacing_mut().scroll.floating = false;
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.add_space(8.0);
                if let Some(error) = self.error.as_deref().or(self.controller.last_error()) {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                        if self.load_failed { ui.small("原配置文件已保留。请修复文件后重启；当前仍可填写设置连接。"); }
                    });
                    ui.add_space(8.0);
                } else if let Some(notice) = &self.notice { ui.weak(notice); ui.add_space(8.0); }
                ui.add_enabled_ui(!running, |ui| {
                    self.connection_form(ui);
                    ui.add_space(16.0);
                    ui.separator();
                    self.advanced_form(ui);
                });
                self.status_panel(ui);
            });
        });
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }
}

fn section(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.add_space(6.0);
    ui.strong(title);
    ui.weak(subtitle);
    ui.add_space(8.0);
}
fn field(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str) {
    ui.label(label);
    ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(hint)
            .desired_width(f32::INFINITY),
    );
    ui.add_space(6.0);
}
fn optional_text(ui: &mut egui::Ui, value: &mut Option<String>, hint: &str, secret: bool) {
    let mut text = value.clone().unwrap_or_default();
    if ui
        .add(
            egui::TextEdit::singleline(&mut text)
                .hint_text(hint)
                .password(secret)
                .desired_width(f32::INFINITY),
        )
        .changed()
    {
        *value = if text.is_empty() { None } else { Some(text) };
    }
}
fn choice(ui: &mut egui::Ui, label: &str, value: &mut String, choices: &[(&str, &str)]) {
    ui.label(label);
    let selected = choices
        .iter()
        .find(|(key, _)| *key == value)
        .map(|(_, name)| *name)
        .unwrap_or(value);
    egui::ComboBox::from_id_salt(label)
        .width(ui.available_width().max(100.0))
        .selected_text(selected)
        .show_ui(ui, |ui| {
            for (key, text) in choices {
                ui.selectable_value(value, (*key).into(), *text);
            }
        });
    ui.add_space(6.0);
}
