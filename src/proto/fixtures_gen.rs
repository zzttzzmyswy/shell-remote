//! 夹具生成器：**从真实代码路径**产出每个消息，落盘成 `v1/` 夹具。
//!
//! 只在 `REGEN_FIXTURES=1` 时跑（`#[ignore]` + 显式 `--ignored`）。`v0/` 永不
//! 重新生成——那是已发布的线协议，改动等于伪造历史。
//!
//! 凡是生产代码里有独立构造函数（`build_register_message` /
//! `build_register_response` / `build_upgrade_message` / `build_mcp_exec_message` /
//! `build_fs_read_message`）的，一律调用它；其余类型复刻各模块 `json!({...})`
//! 里**逐字相同**的字段集，并在注释里标出源位置，便于核对。
//!
//! 字段取值刻意选「有内容但不依赖运行环境」的样本（固定 hostname / 固定 token、
//! 不写时间戳），这样夹具是可复现的、diff 可读的。

use serde_json::{json, Value};

use crate::proto::fixtures::{
    fixture_filename, fixture_rel_path, sha256_hex, AGENT_TO_RELAY, CURRENT_VERSION_DIR,
    RELAY_TO_AGENT,
};

/// 夹具里统一用的会话 id（合法自定义 id：5-20 位字母数字）。
const SID: &str = "goldensession1";

/// 一帧 fMP4 片段的替身字节：夹具里不放真实视频，只保留可校验的长度与内容。
fn sample_video_fragment() -> Vec<u8> {
    let mut v = Vec::with_capacity(64);
    v.extend_from_slice(b"\x00\x00\x00\x18ftypiso5\x00\x00\x00\x01iso5iso6mp41");
    v.extend_from_slice(b"\x00\x00\x00\x0cmoof\x00\x00\x00\x00");
    v
}

/// 固定的 device 对象（不跑 `device::probe()`——那会把测试机的内核版本写进
/// 夹具，让夹具随机器变化）。
pub fn sample_device() -> crate::proto::DeviceInfo {
    crate::proto::DeviceInfo {
        hostname: Some("golden-host".to_string()),
        platform: Some("linux".to_string()),
        arch: Some("x86_64".to_string()),
        os: Some("Linux".to_string()),
        kernel: Some("5.15.0-golden".to_string()),
        cpu_model: Some("Golden CPU @ 2.00GHz".to_string()),
    }
}

/// 构造一个 relay 下发的升级任务（用于 `agent:upgrade` 夹具与契约断言）。
///
/// 字段与 `relay::artifact_store::UpgradeTask` 一一对应；`relay_url` 与
/// `official_path` 都取**通过 agent 侧校验**的合法形态，否则回放时会因为
/// 「不安全路径」被判成坏任务，掩盖真正的兼容问题。
pub fn sample_upgrade_task() -> crate::relay::artifact_store::UpgradeTask {
    crate::relay::artifact_store::UpgradeTask {
        upgrade_id: "up-golden-0001".to_string(),
        target_version: "0.59.12".to_string(),
        filename: "shell-remote-x86_64".to_string(),
        size: 12_345_678,
        sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
        official_path: Some(
            "/zzttzzmyswy/shell-remote/releases/download/v0.59.12/shell-remote-x86_64".to_string(),
        ),
        cdn_base_urls: vec![
            "https://github.com".to_string(),
            "https://gh-proxy.com/https://github.com".to_string(),
        ],
        relay_url: Some("/agent/upgrade/blob/shell-remote-x86_64".to_string()),
        relay_fallback: true,
        source: crate::relay::artifact_store::ArtifactSource::Local,
        official_release: true,
        variant: crate::relay::artifact_store::Variant::Cli,
        arch: "x86_64".to_string(),
        windows: false,
    }
}

/// 一条 `(name, value)` —— 生成器产出的全部夹具，name 是文件名。
pub type Fixture = (String, FixtureBody);

/// 夹具正文：JSON 或二进制（`desktop:video` 的 9 字节头帧）。
pub enum FixtureBody {
    Json(Value),
    Bin(Vec<u8>),
}

/// 序列化：JSON 用 2 空格缩进 + 结尾换行（diff 可读、sha256 稳定）。
fn render_json(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).expect("夹具必须是可序列化的 JSON");
    s.push('\n');
    s
}

// ── agent → relay ──────────────────────────────────────────────────

/// 生成 `v1/agent_to_relay/` 的全部夹具。
///
/// 顺序即写盘顺序；覆盖率断言（`tests::test_fixture_covers_every_registered_type`）
/// 会核对这里是不是真的覆盖了 `AGENT_TO_RELAY_TYPES` 的每一项。
pub fn agent_to_relay_v1() -> Vec<Fixture> {
    let mut out: Vec<Fixture> = Vec::new();
    let sid = SID;

    // ── 注册握手：三种变体 ────────────────────────────────────────
    // 新注册（带 key / token_type）。真实路径：
    // `agent::client::build_register_message`（`connect_http` 用它发注册）。
    out.push((
        fixture_filename("agent:register", Some("new")),
        FixtureBody::Json(crate::agent::client::build_register_message(
            Some("golden-fixed-key".to_string()),
            "rw",
            Some(sid),
            None,
            None,
            &sample_device(),
        )),
    ));
    // 带 tokens 重连：relay 复用这批 token 而不是新铸。
    out.push((
        fixture_filename("agent:register", Some("reconnect_tokens")),
        FixtureBody::Json(crate::agent::client::build_register_message(
            None,
            "rw",
            Some(sid),
            Some(&[
                ("golden-rw-token".to_string(), "rw".to_string()),
                ("golden-ro-token".to_string(), "ro".to_string()),
            ]),
            Some("192.168.1.5:43210".to_string()),
            &sample_device(),
        )),
    ));
    // 回滚重绑：等价于旧版普通断线重连。**故意不带** capabilities / device /
    // lan_addr —— relay 侧对这三项是「带了才覆盖」，缺省即保留（MYS-2553 3c）。
    // 真实路径：`agent::update_apply::bind_identity`。
    out.push((
        fixture_filename("agent:register", Some("rollback_rebind")),
        FixtureBody::Json(json!({
            "type": "agent:register",
            "session_id": sid,
            "tokens": [
                {"token": "golden-rw-token", "permission": "rw"},
                {"token": "golden-ro-token", "permission": "ro"}
            ],
            "agent_version": "0.59.11",
            "protocol_version": crate::proto::PROTOCOL_VERSION,
        })),
    ));

    // ── 生命周期 / 保活 ───────────────────────────────────────────
    // `ping` 无 payload（真实心跳就是这个形状，relay 在严格解析前用宽松 JSON
    // 拦截）。源：`agent/mod.rs` sender_loop `heartbeat_tick` 分支。
    out.push((
        fixture_filename("ping", None),
        FixtureBody::Json(json!({
            "type": "ping",
            "session_id": sid,
        })),
    ));
    // 带 KPI 的 ping（desktop 构建的心跳扩展，源同上）。
    out.push((
        fixture_filename("ping", Some("kpi")),
        FixtureBody::Json(json!({
            "type": "ping",
            "session_id": sid,
            "kpi": {
                "running": true,
                "codec": "h264",
                "fps": 30,
                "quality_permille": 670,
                "bitrate_kbps": 1388,
                "encode_ms": 11,
                "active": true,
                "bp_count": 0,
                "rss_kb": 20480,
                "cpu_ms": 12345,
                "cpu_temp": 45.5
            }
        })),
    ));
    // 源：`agent::upgrade::send_frame` / `send_progress`。
    out.push((
        fixture_filename("agent:upgrade_progress", None),
        FixtureBody::Json(json!({
            "type": "agent:upgrade_progress",
            "session_id": sid,
            "payload": {
                "stage": "downloading",
                "version": "0.59.12",
                "percent": 42,
                "detail": "9.9MB/23.5MB 1.8MB/s"
            }
        })),
    ));

    // ── 终端 / 文件 / MCP 回包 ────────────────────────────────────
    // 源：`agent/mod.rs` flush_output（`terminal:output` 的 data 是 base64，
    // 内容已按 UTF-8 解码——MYS-2554 的编码修复就在这条路径上）。
    out.push((
        fixture_filename("terminal:output", None),
        FixtureBody::Json(json!({
            "type": "terminal:output",
            "session_id": sid,
            "payload": {
                "data": crate::agent::fs::encode_b64("golden output\n".as_bytes()),
                "tab_id": "tab-1"
            }
        })),
    ));
    // 源：`src/proto/mod.rs` `FsResultPayload`（`fs:list` 的回包）。
    out.push((
        fixture_filename("fs:result", None),
        FixtureBody::Json(json!({
            "type": "fs:result",
            "session_id": sid,
            "payload": {
                "success": true,
                "entries": [
                    {
                        "name": "note.txt",
                        "path": "/home/golden/note.txt",
                        "type": "file",
                        "size": 1024,
                        "mode": "-rw-r--r--",
                        "owner": "1000:1000",
                        "is_symlink": false
                    }
                ],
                "_mcp_request_id": "req-fs-1"
            }
        })),
    ));
    // fs:mkdir 是**双向**类型：agent 用它广播「目录已建」，源
    // `agent/mod.rs` 的 `"fs:mkdir"` 分支（回的是 fs:result，但该分支同时把
    // 建目录结果送给浏览器；这里录的是 agent→relay 方向的广播形态）。
    out.push((
        fixture_filename("fs:mkdir", None),
        FixtureBody::Json(json!({
            "type": "fs:mkdir",
            "session_id": sid,
            "payload": {
                "path": "/home/golden/newdir",
                "success": true
            }
        })),
    ));
    // 源：`agent/mod.rs` `"mcp:exec"` 分支（`McpResultPayload` + 回传
    // `_mcp_request_id`，relay 靠它唤醒 oneshot）。
    out.push((
        fixture_filename("mcp:result", None),
        FixtureBody::Json(json!({
            "type": "mcp:result",
            "session_id": sid,
            "payload": {
                "stdout": "golden stdout\n",
                "stderr": "",
                "exit_code": 0,
                "_mcp_request_id": "req-mcp-1"
            }
        })),
    ));
    // 源：同上，`mcp:exec_start` 分支（`ExecResultPayload`）。
    out.push((
        fixture_filename("mcp:exec_result", None),
        FixtureBody::Json(json!({
            "type": "mcp:exec_result",
            "session_id": sid,
            "payload": {
                "exec_id": "exec-1",
                "stdout": "done\n",
                "stderr": "",
                "status": "exited",
                "exit_code": 0,
                "_mcp_request_id": "req-mcp-2"
            }
        })),
    ));

    // ── 会话状态 ──────────────────────────────────────────────────
    // 源：`relay/ws.rs` 浏览器 join/leave 广播（`{count}`）。
    out.push((
        fixture_filename("session:users", None),
        FixtureBody::Json(json!({
            "type": "session:users",
            "session_id": sid,
            "payload": {"count": 2}
        })),
    ));
    // 源：`agent/mod.rs` `build_tab_infos`。
    out.push((
        fixture_filename("session:tab_list", None),
        FixtureBody::Json(json!({
            "type": "session:tab_list",
            "session_id": sid,
            "payload": {
                "tabs": [
                    {"tab_id": "tab-1", "title": "bash", "active": true},
                    {"tab_id": "tab-2", "title": "logs", "active": false}
                ]
            }
        })),
    ));
    out.push((
        fixture_filename("session:tab_switched", None),
        FixtureBody::Json(json!({
            "type": "session:tab_switched",
            "session_id": sid,
            "payload": {"tab_id": "tab-2"}
        })),
    ));

    // ── 桌面 ──────────────────────────────────────────────────────
    // `desktop:video` 的 JSON 形态（未协商二进制上行时走这条）。源：
    // `agent/desktop/mod.rs` 编码循环的 init / frag 两条 post。
    out.push((
        fixture_filename("desktop:video", Some("init")),
        FixtureBody::Json(json!({
            "type": "desktop:video",
            "session_id": sid,
            "payload": {
                "kind": "init",
                "codec": "h264",
                "data": crate::agent::fs::encode_b64(sample_video_fragment().as_slice())
            }
        })),
    ));
    out.push((
        fixture_filename("desktop:video", Some("frag")),
        FixtureBody::Json(json!({
            "type": "desktop:video",
            "session_id": sid,
            "payload": {
                "kind": "frag",
                "key": true,
                "data": crate::agent::fs::encode_b64(sample_video_fragment().as_slice()),
                "seq": 1,
                "flags": "key"
            }
        })),
    ));
    // 二进制帧形态（9 字节头 + fMP4 字节）。真实编码器：`proto::encode_bin_frame`。
    out.push((
        "desktop_video__bin_frag.bin".to_string(),
        FixtureBody::Bin(crate::proto::encode_bin_frame(
            crate::proto::BIN_FRAME_FLAG_KEY,
            7,
            &sample_video_fragment(),
        )),
    ));
    // 源：`agent/desktop/mod.rs` `run_desktop_pipeline` 成功路径的 started。
    out.push((
        fixture_filename("desktop:started", None),
        FixtureBody::Json(json!({
            "type": "desktop:started",
            "session_id": sid,
            "payload": {
                "codec": "h264",
                "width": 1920,
                "height": 1080,
                "fps": 30.0,
                "min_kbps": 800,
                "max_kbps": 4000,
                "backend": "x11",
                "display": ":0",
                "displays": [
                    {"name": ":0.0", "width": 1920, "height": 1080, "x": 0, "y": 0}
                ]
            }
        })),
    ));
    // 源：同上 `post_started_error`（启动失败也要回 started + error，
    // 浏览器靠它结束"正在启动"状态）。
    out.push((
        fixture_filename("desktop:started", Some("error")),
        FixtureBody::Json(json!({
            "type": "desktop:started",
            "session_id": sid,
            "payload": {"codec": "h264", "error": "capture unavailable: no display"}
        })),
    ));
    // 源：`agent/desktop/mod.rs` `broadcast_stopped`（payload 为空对象）。
    out.push((
        fixture_filename("desktop:stopped", None),
        FixtureBody::Json(json!({
            "type": "desktop:stopped",
            "session_id": sid,
            "payload": {}
        })),
    ));
    // 源：`agent/desktop/mod.rs` 捕获失败分支。
    out.push((
        fixture_filename("desktop:error", None),
        FixtureBody::Json(json!({
            "type": "desktop:error",
            "session_id": sid,
            "payload": {"error": "capture error, retrying: BadMatch (invalid parameter attributes)"}
        })),
    ));
    // 源：`agent/desktop/mod.rs` `capabilities_json`。
    out.push((
        fixture_filename("desktop:capabilities", None),
        FixtureBody::Json(json!({
            "type": "desktop:capabilities",
            "session_id": sid,
            "payload": {
                "available": true,
                "running": false,
                "codecs": ["h264"],
                "capture": "auto",
                "lan_addrs": []
            }
        })),
    ));
    // 源：`agent/mod.rs` 上行通道切换时插入的 `desktop:uplink`。
    out.push((
        fixture_filename("desktop:uplink", None),
        FixtureBody::Json(json!({
            "type": "desktop:uplink",
            "session_id": sid,
            "payload": {"uplink": "ws"}
        })),
    ));
    // 源：`agent/desktop/mod.rs` `handle_clipboard_get`。
    out.push((
        fixture_filename("desktop:clipboard", None),
        FixtureBody::Json(json!({
            "type": "desktop:clipboard",
            "session_id": sid,
            "payload": {"text": "golden clipboard"}
        })),
    ));
    // 源：`agent/mod.rs` `desktop:test-delay` 分支的回包（qos-ack）。
    out.push((
        fixture_filename("desktop:qos-ack", None),
        FixtureBody::Json(json!({
            "type": "desktop:qos-ack",
            "session_id": sid,
            "payload": {
                "fps": 30,
                "qos_scale": 2,
                "bitrate_kbps": 1388,
                "qos_state": "Good",
                "active": true,
                "bp_count": 0
            }
        })),
    ));
    // 源：`agent/mod.rs` `desktop:codec` / `desktop:quality` / `desktop:gray` /
    // `desktop:select-display` 的共用回执形态（成功与失败两种）。
    out.push((
        fixture_filename("desktop:cmd-ack", None),
        FixtureBody::Json(json!({
            "type": "desktop:cmd-ack",
            "session_id": sid,
            "payload": {"seq": 3, "ok": true}
        })),
    ));
    out.push((
        fixture_filename("desktop:cmd-ack", Some("error")),
        FixtureBody::Json(json!({
            "type": "desktop:cmd-ack",
            "session_id": sid,
            "payload": {"seq": 4, "ok": false, "error": "codec av1 unavailable on this backend"}
        })),
    ));
    // 源：`agent/desktop/mod.rs` `set_cursor_cb` 的光标通道。
    out.push((
        fixture_filename("desktop:cursor", None),
        FixtureBody::Json(json!({
            "type": "desktop:cursor",
            "session_id": sid,
            "payload": {"x": 640, "y": 360, "shown": true}
        })),
    ));
    // 源：`agent/mod.rs` `desktop:test-delay` 的 echo 回包。
    out.push((
        fixture_filename("test-delay-ack", None),
        FixtureBody::Json(json!({
            "type": "test-delay-ack",
            "session_id": sid,
            "payload": {"seq": 9, "t0": 12345.678}
        })),
    ));
    // 源：`agent/p2p.rs` `handle_offer` 的应答。
    out.push((
        fixture_filename("desktop:p2p-answer", None),
        FixtureBody::Json(json!({
            "type": "desktop:p2p-answer",
            "session_id": sid,
            "payload": {
                "sdp": "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n",
                "candidates": ["candidate:1 1 udp 2130706431 192.168.1.5 54321 typ host"]
            }
        })),
    ));
    // 源：`agent/p2p.rs` `send_state`。
    out.push((
        fixture_filename("desktop:p2p-state", None),
        FixtureBody::Json(json!({
            "type": "desktop:p2p-state",
            "session_id": sid,
            "payload": {"state": "connected"}
        })),
    ));

    // ── 网页代理回包（MYS-2383）──────────────────────────────────
    // 源：`agent/web_proxy.rs` 的 head / chunk / end / error 四处 send。
    out.push((
        fixture_filename("web:head", None),
        FixtureBody::Json(json!({
            "type": "web:head",
            "session_id": sid,
            "payload": {
                "request_id": "web-req-1",
                "status": 200,
                "headers": [["content-type", "text/html; charset=utf-8"]],
                "final_url": "http://intranet.golden/"
            }
        })),
    ));
    out.push((
        fixture_filename("web:chunk", None),
        FixtureBody::Json(json!({
            "type": "web:chunk",
            "session_id": sid,
            "payload": {
                "request_id": "web-req-1",
                "seq": 0,
                "data": crate::agent::fs::encode_b64(b"<html>golden</html>")
            }
        })),
    ));
    out.push((
        fixture_filename("web:end", None),
        FixtureBody::Json(json!({
            "type": "web:end",
            "session_id": sid,
            "payload": {"request_id": "web-req-1"}
        })),
    ));
    out.push((
        fixture_filename("web:error", None),
        FixtureBody::Json(json!({
            "type": "web:error",
            "session_id": sid,
            "payload": {
                "request_id": "web-req-2",
                "kind": "timeout",
                "message": "response body idle timeout"
            }
        })),
    ));

    // ── 纯 TCP 管道（MYS-2383 阶段6）─────────────────────────────
    // 源：`agent/tcp_tunnel.rs` 各 send 点。
    out.push((
        fixture_filename("tcp:opened", None),
        FixtureBody::Json(json!({
            "type": "tcp:opened",
            "session_id": sid,
            "payload": {"conn_id": "conn-1"}
        })),
    ));
    out.push((
        fixture_filename("tcp:data", None),
        FixtureBody::Json(json!({
            "type": "tcp:data",
            "session_id": sid,
            "payload": {
                "conn_id": "conn-1",
                "seq": 0,
                "data": crate::agent::fs::encode_b64(b"GET / HTTP/1.1\r\n\r\n")
            }
        })),
    ));
    out.push((
        fixture_filename("tcp:ack", None),
        FixtureBody::Json(json!({
            "type": "tcp:ack",
            "session_id": sid,
            "payload": {"conn_id": "conn-1", "bytes": 18}
        })),
    ));
    out.push((
        fixture_filename("tcp:eof", None),
        FixtureBody::Json(json!({
            "type": "tcp:eof",
            "session_id": sid,
            "payload": {"conn_id": "conn-1"}
        })),
    ));
    out.push((
        fixture_filename("tcp:closed", None),
        FixtureBody::Json(json!({
            "type": "tcp:closed",
            "session_id": sid,
            "payload": {"conn_id": "conn-1"}
        })),
    ));
    out.push((
        fixture_filename("tcp:error", None),
        FixtureBody::Json(json!({
            "type": "tcp:error",
            "session_id": sid,
            "payload": {
                "conn_id": "conn-1",
                "kind": "connect",
                "message": "connect to 10.0.0.9:8080 failed: connection refused"
            }
        })),
    ));

    // ── 通用错误回包 ──────────────────────────────────────────────
    // 源：`proto::ErrorPayload`（`agent/mod.rs` 的 `error` 分支，
    // 例如 UPGRADE_BAD_PAYLOAD）。
    out.push((
        fixture_filename("error", None),
        FixtureBody::Json(json!({
            "type": "error",
            "session_id": sid,
            "payload": {
                "code": "UPGRADE_BAD_PAYLOAD",
                "message": "agent:upgrade missing target_version/version, filename (or url) and a 64-hex sha256"
            }
        })),
    ));

    out
}

/// 生成 `v1/relay_to_agent/` 的全部夹具。
pub fn relay_to_agent_v1() -> Vec<Fixture> {
    let mut out: Vec<Fixture> = Vec::new();
    let sid = SID;

    // ── 注册响应 ──────────────────────────────────────────────────
    // v1 是当前 relay 的真实输出（真实路径 `relay::ws::build_register_response`）。
    // 同名的 v0 形态在 `fixtures/v0/relay_to_agent/agent_registered.json`：
    // 没有 protocol_version / relay_version / relay_capabilities，只有
    // desktop_binary + tokens。新 agent 必须能照常注册并按 desktop_binary 回退。
    out.push((
        fixture_filename("agent:registered", None),
        FixtureBody::Json(crate::relay::ws::build_register_response(
            sid,
            false,
            vec![
                json!({"token": "golden-rw-token", "permission": "rw"}),
                json!({"token": "golden-ro-token", "permission": "ro"}),
            ],
        )),
    ));
    // ── 终端 / 会话 ──────────────────────────────────────────────
    // 源：`relay/ws.rs` `browser_send_handler` 的转发形态（浏览器发来的
    // `type`/`payload` 原样转发，session_id 由 relay 填）。
    out.push((
        fixture_filename("terminal:input", None),
        FixtureBody::Json(json!({
            "type": "terminal:input",
            "session_id": sid,
            "payload": {
                "data": crate::agent::fs::encode_b64(b"echo golden\n"),
                "tab_id": "tab-1"
            }
        })),
    ));
    out.push((
        fixture_filename("terminal:resize", None),
        FixtureBody::Json(json!({
            "type": "terminal:resize",
            "session_id": sid,
            "payload": {"cols": 120, "rows": 40, "tab_id": "tab-1"}
        })),
    ));
    // 源：`relay/ws.rs` 浏览器 SSE 建立时的 session:join。
    out.push((
        fixture_filename("session:join", None),
        FixtureBody::Json(json!({
            "type": "session:join",
            "session_id": sid,
            "payload": {"user_id": "user-golden", "permission": "rw"}
        })),
    ));
    out.push((
        fixture_filename("session:leave", None),
        FixtureBody::Json(json!({
            "type": "session:leave",
            "session_id": sid,
            "payload": {"user_id": "user-golden", "permission": "rw"}
        })),
    ));
    // 浏览器 tab 操作（源：`web/session.js` 的 send 调用，经 relay 转发）。
    out.push((
        fixture_filename("session:tab_switch", None),
        FixtureBody::Json(json!({
            "type": "session:tab_switch",
            "session_id": sid,
            "payload": {"tab_id": "tab-2", "_user_id": "user-golden"}
        })),
    ));
    out.push((
        fixture_filename("session:tab_create", None),
        FixtureBody::Json(json!({
            "type": "session:tab_create",
            "session_id": sid,
            "payload": {"_user_id": "user-golden"}
        })),
    ));
    out.push((
        fixture_filename("session:tab_close", None),
        FixtureBody::Json(json!({
            "type": "session:tab_close",
            "session_id": sid,
            "payload": {"tab_id": "tab-2", "_user_id": "user-golden"}
        })),
    ));

    // ── 文件 ──────────────────────────────────────────────────────
    // 源：`web/files.js` 的 `send('fs:list', {path})` 等；`fs:read` 用真实构造
    // 路径 `relay::file_transfer::build_fs_read_message`（带 Range 与不带两种）。
    out.push((
        fixture_filename("fs:list", None),
        FixtureBody::Json(json!({
            "type": "fs:list",
            "session_id": sid,
            "payload": {"path": "/home/golden"}
        })),
    ));
    out.push((
        fixture_filename("fs:read", None),
        FixtureBody::Json(crate::relay::file_transfer::build_fs_read_message(
            sid,
            "/home/golden/big.log",
            "c-1",
            None,
        )),
    ));
    out.push((
        fixture_filename("fs:read", Some("range")),
        FixtureBody::Json(crate::relay::file_transfer::build_fs_read_message(
            sid,
            "/home/golden/big.log",
            "c-2",
            Some((1024, Some(2047))),
        )),
    ));
    out.push((
        fixture_filename("fs:read_cancel", None),
        FixtureBody::Json(json!({
            "type": "fs:read_cancel",
            "session_id": sid,
            "payload": {"_mcp_request_id": "c-1"}
        })),
    ));
    out.push((
        fixture_filename("fs:write", None),
        FixtureBody::Json(json!({
            "type": "fs:write",
            "session_id": sid,
            "payload": {
                "path": "/home/golden/note.txt",
                "content": crate::agent::fs::encode_b64(b"hello\n"),
                "_mcp_request_id": "req-fs-2"
            }
        })),
    ));
    // 源：`relay/file_transfer.rs` 的分块上传。
    out.push((
        fixture_filename("fs:upload", None),
        FixtureBody::Json(json!({
            "type": "fs:upload",
            "session_id": sid,
            "payload": {
                "upload_id": "up-1",
                "final_path": "/home/golden/up.bin",
                "content": crate::agent::fs::encode_b64(b"chunk-0"),
                "chunk_index": 0,
                "total_chunks": 2
            }
        })),
    ));
    // 上传中途取消（源：`relay/file_transfer.rs:630` 的 abort 帧）。
    out.push((
        fixture_filename("fs:upload", Some("abort")),
        FixtureBody::Json(json!({
            "type": "fs:upload",
            "session_id": sid,
            "payload": {"upload_id": "up-1", "aborted": true}
        })),
    ));
    out.push((
        fixture_filename("fs:delete", None),
        FixtureBody::Json(json!({
            "type": "fs:delete",
            "session_id": sid,
            "payload": {"path": "/home/golden/old.txt"}
        })),
    ));
    out.push((
        fixture_filename("fs:rename", None),
        FixtureBody::Json(json!({
            "type": "fs:rename",
            "session_id": sid,
            "payload": {"from": "/home/golden/a.txt", "to": "/home/golden/b.txt"}
        })),
    ));
    // `fs:mkdir` 的**请求**方向（浏览器 → agent）。与 agent→relay 的同名广播
    // 共用一个类型名，这是已登记的双向类型之一。
    out.push((
        fixture_filename("fs:mkdir", Some("request")),
        FixtureBody::Json(json!({
            "type": "fs:mkdir",
            "session_id": sid,
            "payload": {"path": "/home/golden/newdir"}
        })),
    ));

    // ── MCP ───────────────────────────────────────────────────────
    // 真实路径：`relay::mcp::build_mcp_exec_message`。
    out.push((
        fixture_filename("mcp:exec", None),
        FixtureBody::Json(serde_json::to_value(
            crate::relay::mcp::build_mcp_exec_message(
                sid,
                "uname -a",
                30_000,
                Some("auto"),
                Some("/home/golden"),
                "req-mcp-1",
            ),
        )
        .expect("ProtoMessage 必须可序列化")),
    ));
    // 无 shell / cwd 的旧形态（老 relay 只发这三个键，agent 必须照常处理）。
    out.push((
        fixture_filename("mcp:exec", Some("legacy")),
        FixtureBody::Json(serde_json::to_value(
            crate::relay::mcp::build_mcp_exec_message(
                sid,
                "df -h",
                30_000,
                None,
                None,
                "req-mcp-legacy",
            ),
        )
        .expect("ProtoMessage 必须可序列化")),
    ));
    // 源：`web/` 的 MCP 流式执行调用（exec_start / input / close / list）。
    out.push((
        fixture_filename("mcp:exec_start", None),
        FixtureBody::Json(json!({
            "type": "mcp:exec_start",
            "session_id": sid,
            "payload": {"cmd": "tail -f /var/log/syslog", "_mcp_request_id": "req-exec-1"}
        })),
    ));
    out.push((
        fixture_filename("mcp:exec_input", None),
        FixtureBody::Json(json!({
            "type": "mcp:exec_input",
            "session_id": sid,
            "payload": {
                "exec_id": "exec-1",
                "data_b64": crate::agent::fs::encode_b64(b"y\n"),
                "_mcp_request_id": "req-exec-2"
            }
        })),
    ));
    out.push((
        fixture_filename("mcp:exec_close", None),
        FixtureBody::Json(json!({
            "type": "mcp:exec_close",
            "session_id": sid,
            "payload": {"exec_id": "exec-1", "_mcp_request_id": "req-exec-3"}
        })),
    ));
    out.push((
        fixture_filename("mcp:exec_list", None),
        FixtureBody::Json(json!({
            "type": "mcp:exec_list",
            "session_id": sid,
            "payload": {"_mcp_request_id": "req-exec-4"}
        })),
    ));

    // ── 桌面控制 ──────────────────────────────────────────────────
    // 源：`web/session.js` / `web/desktop.js` 的 send 调用。
    out.push((
        fixture_filename("desktop:start", None),
        FixtureBody::Json(json!({
            "type": "desktop:start",
            "session_id": sid,
            "payload": {}
        })),
    ));
    out.push((
        fixture_filename("desktop:stop", None),
        FixtureBody::Json(json!({
            "type": "desktop:stop",
            "session_id": sid,
            "payload": {}
        })),
    ));
    out.push((
        fixture_filename("desktop:codec", None),
        FixtureBody::Json(json!({
            "type": "desktop:codec",
            "session_id": sid,
            "payload": {"codec": "h264", "seq": 3}
        })),
    ));
    out.push((
        fixture_filename("desktop:quality", None),
        FixtureBody::Json(json!({
            "type": "desktop:quality",
            "session_id": sid,
            "payload": {"quality": "balanced", "bitrate_kbps": 1500, "seq": 4}
        })),
    ));
    out.push((
        fixture_filename("desktop:bitrate", None),
        FixtureBody::Json(json!({
            "type": "desktop:bitrate",
            "session_id": sid,
            "payload": {"kbps": 2200}
        })),
    ));
    out.push((
        fixture_filename("desktop:gray", None),
        FixtureBody::Json(json!({
            "type": "desktop:gray",
            "session_id": sid,
            "payload": {"enabled": true, "seq": 5}
        })),
    ));
    out.push((
        fixture_filename("desktop:reqkey", None),
        FixtureBody::Json(json!({
            "type": "desktop:reqkey",
            "session_id": sid,
            "payload": {}
        })),
    ));
    out.push((
        fixture_filename("desktop:test-delay", None),
        FixtureBody::Json(json!({
            "type": "desktop:test-delay",
            "session_id": sid,
            "payload": {"seq": 9, "t0": 12345.678}
        })),
    ));
    // 源：`web/desktop.js` 的键鼠注入（`parse_mouse` / `parse_key` 逐字段读）。
    out.push((
        fixture_filename("desktop:mouse", None),
        FixtureBody::Json(json!({
            "type": "desktop:mouse",
            "session_id": sid,
            "payload": {"type": "move", "x": 640, "y": 360}
        })),
    ));
    out.push((
        fixture_filename("desktop:key", None),
        FixtureBody::Json(json!({
            "type": "desktop:key",
            "session_id": sid,
            "payload": {"code": "KeyA", "down": true}
        })),
    ));
    out.push((
        fixture_filename("desktop:clipboard:get", None),
        FixtureBody::Json(json!({
            "type": "desktop:clipboard:get",
            "session_id": sid,
            "payload": {}
        })),
    ));
    out.push((
        fixture_filename("desktop:clipboard:set", None),
        FixtureBody::Json(json!({
            "type": "desktop:clipboard:set",
            "session_id": sid,
            "payload": {"text": "clipboard from browser"}
        })),
    ));
    out.push((
        fixture_filename("desktop:select-display", None),
        FixtureBody::Json(json!({
            "type": "desktop:select-display",
            "session_id": sid,
            "payload": {"display": ":0.1", "seq": 6}
        })),
    ));
    // 源：`web/desktop.js` 的 QoS 上报。
    out.push((
        fixture_filename("desktop:qos", None),
        FixtureBody::Json(json!({
            "type": "desktop:qos",
            "session_id": sid,
            "payload": {
                "delay_ms": 42,
                "probe_ms": 18,
                "dfps": 30,
                "dq": 0,
                "lseq": 128
            }
        })),
    ));
    // 源：`relay/ws.rs` `notify_congested`。
    out.push((
        fixture_filename("desktop:congested", None),
        FixtureBody::Json(json!({
            "type": "desktop:congested",
            "session_id": sid,
            "payload": {"dropped": 3}
        })),
    ));
    // 源：`web/desktop.js` 的 P2P 信令（offer / trickle candidate）。
    out.push((
        fixture_filename("desktop:p2p-offer", None),
        FixtureBody::Json(json!({
            "type": "desktop:p2p-offer",
            "session_id": sid,
            "payload": {
                "sdp": "v=0\r\no=- 2 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n",
                "candidates": []
            }
        })),
    ));
    out.push((
        fixture_filename("desktop:p2p-candidate", None),
        FixtureBody::Json(json!({
            "type": "desktop:p2p-candidate",
            "session_id": sid,
            "payload": {
                "candidate": "candidate:1 1 udp 2130706431 192.168.1.9 54322 typ host"
            }
        })),
    ));

    // ── 网页代理 / TCP 请求 ──────────────────────────────────────
    // 源：`relay/web_proxy.rs:883` 的 web:request、`:611` 的 web:cancel。
    out.push((
        fixture_filename("web:request", None),
        FixtureBody::Json(json!({
            "type": "web:request",
            "session_id": sid,
            "payload": {
                "request_id": "web-req-1",
                "url": "http://intranet.golden/",
                "method": "GET"
            }
        })),
    ));
    out.push((
        fixture_filename("web:cancel", None),
        FixtureBody::Json(json!({
            "type": "web:cancel",
            "session_id": sid,
            "payload": {"request_id": "web-req-1"}
        })),
    ));
    // 源：`relay/tcp_tunnel.rs:417` 的 tcp:open、`:467` 的 tcp:close。
    out.push((
        fixture_filename("tcp:open", None),
        FixtureBody::Json(json!({
            "type": "tcp:open",
            "session_id": sid,
            "payload": {"conn_id": "conn-1", "host": "10.0.0.9", "port": 8080}
        })),
    ));
    out.push((
        fixture_filename("tcp:close", None),
        FixtureBody::Json(json!({
            "type": "tcp:close",
            "session_id": sid,
            "payload": {"conn_id": "conn-1"}
        })),
    ));
    // `tcp:data` / `tcp:ack` / `tcp:eof` 是双向类型：这里录 relay 发起的形态
    // （源：`relay/tcp_tunnel.rs:501` 起的 WriteCmd 序列化）。与 agent→relay
    // 方向的同名夹具字段一致，但 session 语义相反（请求 vs 回包）。
    out.push((
        fixture_filename("tcp:data", Some("outbound")),
        FixtureBody::Json(json!({
            "type": "tcp:data",
            "session_id": sid,
            "payload": {
                "conn_id": "conn-1",
                "seq": 0,
                "data": crate::agent::fs::encode_b64(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            }
        })),
    ));
    out.push((
        fixture_filename("tcp:ack", Some("outbound")),
        FixtureBody::Json(json!({
            "type": "tcp:ack",
            "session_id": sid,
            "payload": {"conn_id": "conn-1", "bytes": 27}
        })),
    ));
    out.push((
        fixture_filename("tcp:eof", Some("outbound")),
        FixtureBody::Json(json!({
            "type": "tcp:eof",
            "session_id": sid,
            "payload": {"conn_id": "conn-1"}
        })),
    ));

    // ── 自升级 ────────────────────────────────────────────────────
    // 真实路径：`relay::admin::build_upgrade_message`（含 `wire_payload()` 给
    // 存量 agent 补的 `version` / `url` 别名字段）。
    out.push((
        fixture_filename("agent:upgrade", None),
        FixtureBody::Json(crate::relay::admin::build_upgrade_message(
            sid,
            &sample_upgrade_task(),
        )),
    ));
    // 旧别名字段形态：老 relay 只发 version / url / sha256（没有
    // target_version / official_path / cdn_base_urls / relay_url）。
    // 新 agent 的 `UpgradeTask::from_payload` 必须把它解析成同一个任务。
    out.push((
        fixture_filename("agent:upgrade", Some("legacy_alias")),
        FixtureBody::Json(json!({
            "type": "agent:upgrade",
            "session_id": sid,
            "payload": {
                "version": "0.59.12",
                "url": "/agent/upgrade/blob/shell-remote-x86_64",
                "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            }
        })),
    ));

    out
}

/// 生成 `v0/` 的全部夹具：协议版本标记出现之前的线协议（≤ v0.59.11）。
///
/// 与 `v1` 的差别**只有**协议标记本身：注册消息没有 `protocol_version`，
/// 注册响应没有 `protocol_version` / `relay_version` / `relay_capabilities`。
/// 其余消息类型逐字相同（那时它们已经在线上跑了），所以这里直接复用 `v1` 的
/// 构造再摘掉标记字段——**保证 v0 与 v1 的差异可审计**，不多不少就是那几个键。
///
/// 这个函数只在 `v0/` 目录缺失时用（一次性落盘）；落盘后 `v0/` 就被冻结清单
/// 钉住，重生成会在测试里被拒绝。
pub fn derive_v0_from_v1(v1: &[Fixture]) -> Vec<Fixture> {
    v1.iter()
        .filter_map(|(name, body)| {
            let FixtureBody::Json(v) = body else {
                return None; // .bin 夹具与版本无关，v0 不需要重复一份
            };
            let mut v = v.clone();
            let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
            match ty {
                "agent:register" => {
                    // v0 的注册消息没有 protocol_version。
                    v.as_object_mut().map(|o| o.remove("protocol_version"));
                }
                "agent:registered" => {
                    // v0 的注册响应只有 desktop_binary + payload.tokens。
                    if let Some(o) = v.as_object_mut() {
                        o.remove("protocol_version");
                        o.remove("relay_version");
                        o.remove("relay_capabilities");
                    }
                }
                _ => {}
            }
            Some((name.clone(), FixtureBody::Json(v)))
        })
        .collect()
}

// ── 落盘 ────────────────────────────────────────────────────────────

/// 把一批夹具写进 `v<N>/<direction>/`，并返回冻结清单条目 `(sha256, rel_path)`。
fn write_direction(version: &str, direction: &str, fixtures: &[Fixture]) -> Vec<(String, String)> {
    use crate::proto::fixtures::FIXTURE_DIR;
    let mut entries = Vec::new();
    for (name, body) in fixtures {
        let rel = fixture_rel_path(version, direction, name);
        let full = format!("{FIXTURE_DIR}/{rel}");
        if let Some(parent) = std::path::Path::new(&full).parent() {
            std::fs::create_dir_all(parent).unwrap_or_else(|e| {
                panic!("建不出夹具目录 {}: {e}", parent.display())
            });
        }
        let bytes = match body {
            FixtureBody::Json(v) => render_json(v).into_bytes(),
            FixtureBody::Bin(b) => b.clone(),
        };
        std::fs::write(&full, &bytes)
            .unwrap_or_else(|e| panic!("写不了夹具 {full}: {e}"));
        entries.push((sha256_hex(&bytes), rel));
    }
    entries
}

/// 生成 `v1/` 全量夹具并重写 `FROZEN.sha256`。`v0/` 不动。
///
/// 返回 `(文件数, 清单全文)`，供调用方打印统计。
pub fn regen_v1() -> (usize, String) {
    let a2r = agent_to_relay_v1();
    let r2a = relay_to_agent_v1();

    // 先算 v0 的清单条目（不重写文件）：`v0/` 已在磁盘上，直接按现状计算，
    // 保证重生成 v1 之后冻结清单仍然覆盖 v0。
    let mut entries = v0_frozen_entries();

    entries.extend(write_direction(CURRENT_VERSION_DIR, AGENT_TO_RELAY, &a2r));
    entries.extend(write_direction(CURRENT_VERSION_DIR, RELAY_TO_AGENT, &r2a));

    // 清单按路径排序：diff 稳定，便于 review。
    entries.sort_by(|a, b| a.1.cmp(&b.1));
    let mut text = String::from(
        "# 冻结的协议金样清单（MYS-2553 兼容 2）。\n\
         # 每行：<sha256>  <相对 src/proto/fixtures 的路径>\n\
         # 已发布版本的夹具不可修改：要变更协议，请新增 fixtures/v<N+1>/，\n\
         # 并保持旧版本夹具继续通过。见 docs/protocol.md「夹具与冻结规则」。\n",
    );
    for (hash, path) in &entries {
        text.push_str(&format!("{hash}  {path}\n"));
    }
    let full = format!(
        "{}/{}",
        crate::proto::fixtures::FIXTURE_DIR,
        crate::proto::fixtures::FROZEN_FILE
    );
    std::fs::write(&full, text.as_bytes())
        .unwrap_or_else(|e| panic!("写不了冻结清单 {full}: {e}"));
    (entries.len(), text)
}

/// 盘点磁盘上 `v0/` 现有文件，算出它们的 sha256 条目（**不重写文件**）。
fn v0_frozen_entries() -> Vec<(String, String)> {
    use crate::proto::fixtures::{list_fixtures, V0};
    let mut out = Vec::new();
    for direction in [AGENT_TO_RELAY, RELAY_TO_AGENT] {
        let dir = format!("{}/{V0}/{direction}", crate::proto::fixtures::FIXTURE_DIR);
        if !std::path::Path::new(&dir).is_dir() {
            continue;
        }
        for name in list_fixtures(V0, direction) {
            let rel = fixture_rel_path(V0, direction, &name);
            let full = format!("{}/{rel}", crate::proto::fixtures::FIXTURE_DIR);
            let bytes = std::fs::read(&full)
                .unwrap_or_else(|e| panic!("读不了夹具 {full}: {e}"));
            out.push((sha256_hex(&bytes), rel));
        }
    }
    out
}

/// 一次性落盘 `v0/`（只在 `v0/` 尚不存在时用；之后 `v0/` 由冻结清单保护）。
pub fn bootstrap_v0() {
    let v0 = derive_v0_from_v1(&agent_to_relay_v1());
    let v0r = derive_v0_from_v1(&relay_to_agent_v1());
    write_direction(crate::proto::fixtures::V0, AGENT_TO_RELAY, &v0);
    write_direction(crate::proto::fixtures::V0, RELAY_TO_AGENT, &v0r);
}
