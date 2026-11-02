#!/usr/bin/env python3
"""跨版本兼容矩阵的 HTTP 探针（MYS-2562）。

只用标准库，只走 relay 的公开 HTTP 接口，不 mock 任何内部状态：

  wait-sse       等浏览器 SSE 握手返回 200 —— relay 只在该会话的 agent 通道
                 已建立时才放行（旧版提前返回 503），因此它就是「agent 在线」。
  session-online 登 admin 面板问 overview：该 session 在列表里且 online=true，
                 这是 relay 侧的权威视角（不只是 agent 自认为注册成功）。
  mcp-call       按 MCP 的 SSE + messages 传输跑一次 `shell_remote` 工具调用，
                 把结果文本原样打印（供 shell 侧做一字不差的比较）。
  term-echo      建浏览器 SSE → 取 `session:tab_list` 里的 tab → POST
                 `terminal:input` 发 `echo <marker>` → 在 `terminal:output` 里
                 剥掉 OSC/CSI 转义后等到一行恰好等于 marker 的输出。

所有端点、参数、协议细节都取自 relay 的公开接口，因此同一份脚本既能探新
版本也能探旧版本。
"""

import argparse
import base64
import http.cookiejar
import json
import re
import socket
import sys
import time
import urllib.error
import urllib.parse
import urllib.request


def _cookiejar():
    return http.cookiejar.CookieJar()


def _url(base, path):
    return base.rstrip("/") + path


def _open(base, path, accept="text/event-stream", headers=None, timeout=30):
    hdrs = {"Accept": accept}
    if headers:
        hdrs.update(headers)
    req = urllib.request.Request(_url(base, path), headers=hdrs)
    return urllib.request.urlopen(req, timeout=timeout)


def _iter_events(resp, deadline):
    """逐条产出 (event, data)；只解析 `event:` / `data:` 行。"""
    buf = b""
    while time.time() < deadline:
        try:
            chunk = resp.read(1)
        except (socket.timeout, TimeoutError):
            continue
        if not chunk:
            return
        buf += chunk
        while b"\n\n" in buf:
            block, buf = buf.split(b"\n\n", 1)
            event, data = None, []
            for line in block.split(b"\n"):
                if line.startswith(b"event:"):
                    event = line[6:].strip().decode("utf-8", "replace")
                elif line.startswith(b"data:"):
                    data.append(line[5:].lstrip().decode("utf-8", "replace"))
            if data:
                yield event, "\n".join(data)


def _auth_query(auth):
    return urllib.parse.urlencode({"auth": auth}) if auth else ""


def _post_json(url, obj, auth, timeout=20):
    data = json.dumps(obj).encode()
    headers = {"Content-Type": "application/json"}
    if auth:
        headers["X-Auth"] = auth
    req = urllib.request.Request(url, data=data, headers=headers)
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        # 202 Accepted 是正常应答：真正的 JSON-RPC 响应从 SSE 回来。
        resp.read()
        return resp.status


# ── wait-sse ────────────────────────────────────────────────────────────────


def cmd_wait_sse(args):
    """轮询浏览器 SSE 握手，200 = 该会话的 agent 链路已建立。"""
    query = _auth_query(args.auth)
    path = "/agent/session/sse?" + urllib.parse.urlencode(
        {"token": args.token}
    ) + ("&" + query if query else "")
    deadline = time.time() + args.timeout
    last = "unknown"
    while time.time() < deadline:
        try:
            resp = _open(args.base, path, timeout=5)
            code = resp.status
            resp.close()
            if code == 200:
                return 0
            last = "HTTP %d" % code
        except urllib.error.HTTPError as exc:
            last = "HTTP %d" % exc.code
        except (urllib.error.URLError, OSError) as exc:
            last = str(exc)
        time.sleep(0.5)
    print("SSE 握手始终未成功：%s" % last, file=sys.stderr)
    return 1


# ── mcp-call ────────────────────────────────────────────────────────────────


def _mcp_session(base, auth, timeout):
    """建 MCP SSE 会话，返回 (endpoint_abs_url, resp)。"""
    query = _auth_query(auth)
    path = "/agent/mcp/sse" + ("?" + query if query else "")
    resp = _open(base, path, timeout=15)
    deadline = time.time() + timeout
    for event, data in _iter_events(resp, deadline):
        if event == "endpoint":
            endpoint = data
            url = _url(base, endpoint)
            return url, resp
        if event == "error":
            raise RuntimeError("MCP SSE 拒绝：%s" % data)
    raise RuntimeError("MCP SSE 未下发 endpoint 事件")


def cmd_mcp_call(args):
    endpoint, resp = _mcp_session(args.base, args.auth, args.timeout)
    auth_q = {"auth": args.auth} if args.auth else {}
    sep = "&" if urllib.parse.urlparse(endpoint).query else "?"

    _post_json(
        endpoint + sep + urllib.parse.urlencode(auth_q),
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "compat-matrix", "version": "1"},
            },
        },
        args.auth,
    )
    want_id = 2
    call = {
        "jsonrpc": "2.0",
        "id": want_id,
        "method": "tools/call",
        "params": {
            "name": "shell_remote",
            "arguments": {"token": args.token, "cmd": args.cmd},
        },
    }
    try:
        _post_json(endpoint + sep + urllib.parse.urlencode(auth_q), call, args.auth)
    except urllib.error.HTTPError as exc:
        print("tools/call 被拒：HTTP %d" % exc.code, file=sys.stderr)
        return 1

    deadline = time.time() + args.timeout
    for event, data in _iter_events(resp, deadline):
        if event != "message":
            continue
        try:
            msg = json.loads(data)
        except json.JSONDecodeError:
            continue
        if msg.get("id") != want_id:
            continue
        if "error" in msg:
            print("JSON-RPC 错误：%s" % json.dumps(msg["error"], ensure_ascii=False),
                  file=sys.stderr)
            return 1
        result = msg.get("result") or {}
        parts = [
            c.get("text", "")
            for c in result.get("content", [])
            if c.get("type") == "text"
        ]
        text = "\n".join(parts)
        if result.get("isError"):
            print("工具调用失败：%s" % text, file=sys.stderr)
            return 1
        sys.stdout.write(text)
        if args.expect is not None and text != args.expect:
            print(
                "\n期望 %r，实得 %r" % (args.expect, text),
                file=sys.stderr,
            )
            return 1
        return 0
    print("等待 tools/call 响应超时", file=sys.stderr)
    return 1


# ── session-online ──────────────────────────────────────────────────────────


def cmd_session_online(args):
    """问 relay 的 admin overview：该 session 是否在会话列表里且在线。

    这是 relay 侧的权威视角（不只是「agent 自认为注册了」）。旧版本的
    admin 面板同样有这个接口，字段名一致。
    """
    opener = urllib.request.build_opener(
        urllib.request.HTTPCookieProcessor(_cookiejar())
    )
    login = json.dumps({"user": args.admin_user, "pass": args.admin_pass}).encode()
    req = urllib.request.Request(
        _url(args.base, args.admin_path + "/login"),
        data=login,
        headers={"Content-Type": "application/json"},
    )
    with opener.open(req, timeout=15) as resp:
        resp.read()
        if resp.status != 200:
            print("admin 登录失败：HTTP %d" % resp.status, file=sys.stderr)
            return 1

    req = urllib.request.Request(_url(args.base, args.admin_path + "/api/overview"))
    with opener.open(req, timeout=15) as resp:
        overview = json.loads(resp.read().decode("utf-8", "replace"))

    for sess in overview.get("sessions") or []:
        if sess.get("session_id") != args.sid:
            continue
        if sess.get("online"):
            return 0
        print("会话在列表里但不在线：%s" % args.sid, file=sys.stderr)
        return 1
    print(
        "relay 会话列表里没有 %s（现有：%s）"
        % (
            args.sid,
            ",".join(s.get("session_id", "?") for s in overview.get("sessions") or [])
            or "空",
        ),
        file=sys.stderr,
    )
    return 1


# ── term-echo ───────────────────────────────────────────────────────────────

# 终端输出带 OSC（shell 集成标记，如 `ESC ] 3008 ; … ESC \`）与 CSI（颜色/
# 光标）转义。判定「回显到了」必须在剥掉这些控制序列之后按行比较，否则
# `term-ok` 永远和 OSC 粘在同一行里，一字不差的比较必然失败。
_OSC = re.compile(rb"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")
_CSI = re.compile(rb"\x1b\[[0-9;?]*[ -/]*[@-~]")
_ESC = re.compile(rb"\x1b[@-Z\\-_]")


def _strip_escapes(data):
    data = _OSC.sub(b"", data)
    data = _CSI.sub(b"", data)
    return _ESC.sub(b"", data)


def cmd_term_echo(args):
    query = _auth_query(args.auth)
    path = "/agent/session/sse?" + urllib.parse.urlencode(
        {"token": args.token}
    ) + ("&" + query if query else "")
    resp = _open(args.base, path, timeout=15)
    if resp.status != 200:
        print("浏览器 SSE 握手失败：HTTP %d" % resp.status, file=sys.stderr)
        return 1

    deadline = time.time() + args.timeout
    tab_id = None
    sent = False
    seen_output_before_send = False
    pending = b""
    transcript = bytearray()
    # 逐条读事件：先拿 tab，等第一条 terminal:output（= shell 已就绪、回放
    # 结束）再发输入；之后按行比对输出。
    for event, data in _iter_events(resp, deadline):
        try:
            msg = json.loads(data)
        except json.JSONDecodeError:
            continue
        mtype = msg.get("type")
        if mtype == "session:tab_list" and tab_id is None:
            tabs = (msg.get("payload") or {}).get("tabs") or []
            if not tabs:
                continue
            active = next((t for t in tabs if t.get("active")), tabs[0])
            tab_id = active.get("tab_id")
            continue
        if mtype != "terminal:output":
            continue
        try:
            raw = base64.b64decode((msg.get("payload") or {}).get("data") or "")
        except (ValueError, TypeError):
            continue
        transcript += raw
        pending += _strip_escapes(raw)

        if tab_id and not sent:
            # 已经收到输出 → shell 活着；这时发输入不会打在空终端上。
            if not seen_output_before_send:
                seen_output_before_send = True
                payload = {
                    "data": base64.b64encode(
                        ("echo %s\n" % args.marker).encode()
                    ).decode(),
                    "tab_id": tab_id,
                }
                status = _post_browser_send(args, "terminal:input", payload)
                if status not in (200, 202):
                    print("terminal:input 被拒：HTTP %d" % status, file=sys.stderr)
                    return 1
                sent = True
                continue

        while b"\n" in pending:
            line, pending = pending.split(b"\n", 1)
            if line.strip(b"\r").strip() == args.marker.encode():
                return 0
    print(
        "等待终端输出 %r 超时（tab=%s sent=%s）；输出尾部：\n%s"
        % (
            args.marker,
            tab_id,
            sent,
            _strip_escapes(bytes(transcript))[-600:].decode("utf-8", "replace"),
        ),
        file=sys.stderr,
    )
    return 1


def _post_browser_send(args, mtype, payload):
    body = json.dumps(
        {"token": args.token, "type": mtype, "payload": payload}
    ).encode()
    headers = {"Content-Type": "application/json"}
    if args.auth:
        headers["X-Auth"] = args.auth
    req = urllib.request.Request(
        _url(args.base, "/agent/session/send"), data=body, headers=headers
    )
    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            resp.read()
            return resp.status
    except urllib.error.HTTPError as exc:
        return exc.code


# ── CLI ─────────────────────────────────────────────────────────────────────


def main(argv):
    ap = argparse.ArgumentParser(description="shell-remote 兼容矩阵探针")
    sub = ap.add_subparsers(dest="cmd", required=True)

    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--base", required=True, help="relay 根地址，如 http://127.0.0.1:3000")
    common.add_argument("--auth", default="", help="relay --auth 服务器口令")
    common.add_argument("--token", required=True, help="会话 token（agent --key）")
    common.add_argument("--timeout", type=float, default=30.0, help="整体超时秒数")

    p = sub.add_parser("wait-sse", parents=[common], help="等 agent 在线（SSE 200）")
    p.set_defaults(func=cmd_wait_sse)

    p = sub.add_parser(
        "session-online", parents=[common], help="问 relay admin：该会话是否在线"
    )
    p.add_argument("--sid", required=True)
    p.add_argument("--admin-path", required=True)
    p.add_argument("--admin-user", default="admin")
    p.add_argument("--admin-pass", required=True)
    p.set_defaults(func=cmd_session_online)

    p = sub.add_parser("mcp-call", parents=[common], help="跑一次 MCP shell_remote 调用")
    p.add_argument("--cmd", required=True)
    p.add_argument("--expect", default=None, help="结果文本必须与之完全相等")
    p.set_defaults(func=cmd_mcp_call)

    p = sub.add_parser("term-echo", parents=[common], help="网页终端回显验证")
    p.add_argument("--marker", default="term-ok")
    p.set_defaults(func=cmd_term_echo)

    args = ap.parse_args(argv)
    try:
        return args.func(args)
    except (urllib.error.URLError, OSError, RuntimeError) as exc:
        print("探针失败：%s" % exc, file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
