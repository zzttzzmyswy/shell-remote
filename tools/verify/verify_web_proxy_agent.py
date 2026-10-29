#!/usr/bin/env python3
"""MYS-2383 阶段1 真实验证：agent 侧网页代理（web:proxy-v1）。

一条命令跑完 agent 侧的全部验收场景：

  1. 在 127.0.0.1 上起一个**只监听本机**的测试站（等价于设备自身的页面服务）；
  2. 起一个"记录型假 relay"：`/agent/events` 是真 SSE 下行，`/agent/send` 是真
     HTTP 上行，收下的每条 agent 上行原样保存 —— 看到的就是阶段2 relay 将要
     消费的真实线协议；
  3. 起**真 agent 二进制**（默认 `target-lean/debug/shell-remote`，即
     `--no-default-features` 的 lean 构建，用于证明能力声明与 desktop feature
     无关），下发 `web:request` / `web:cancel`，断言回包。

覆盖：能力声明、127.0.0.1 页面抓取、响应头过滤、>64KiB 分块与 seq 连续性、
慢响应不阻塞控制消息、取消/重复取消、非法 scheme/method、连接失败、
并发上限 8 与取消后配额归还。

用法（先构建 lean agent）：
    cargo build --no-default-features --bin shell-remote
    python3 tools/verify/verify_web_proxy_agent.py

可用环境变量覆盖：AGENT_BIN / RELAY_PORT / SITE_PORT。
"""

import base64
import json
import os
import queue
import subprocess
import sys
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

RELAY_PORT = int(os.environ.get("RELAY_PORT", "18800"))
SITE_PORT = int(os.environ.get("SITE_PORT", "18801"))
AGENT_BIN = os.environ.get(
    "AGENT_BIN",
    os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(
        os.path.abspath(__file__)))), "target-lean", "debug", "shell-remote"),
)

FAILS = []


def check(name, cond, detail=""):
    print(("  PASS " if cond else "  FAIL ") + name + (f"   <- {detail}" if detail and not cond else ""))
    if not cond:
        FAILS.append(name)


# ────────────────────────── 测试站（只监听 127.0.0.1）──────────────────────────

class SiteHandler(BaseHTTPRequestHandler):
    BIG = bytes(bytearray((i % 256) for i in range(200 * 1024)))

    def do_GET(self):
        if self.path.startswith("/big"):
            body, ctype = self.BIG, "application/octet-stream"
        elif self.path.startswith("/slow"):
            time.sleep(40)
            body, ctype = b"too late", "text/plain"
        else:
            body = b"<html><head><title>t</title></head><body>device-local-page</body></html>"
            ctype = "text/html; charset=utf-8"
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        # 这些头必须被 agent 丢弃：
        self.send_header("Set-Cookie", "secret=leak")
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("X-Powered-By", "test")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


# ────────────────────────── 记录型假 relay ──────────────────────────

class Hub:
    def __init__(self):
        self.uplink = []
        self.down = queue.Queue()
        self.registered = None
        self.lock = threading.Lock()


HUB = Hub()


class RelayHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _json(self, obj, code=200):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        try:
            msg = json.loads(self.rfile.read(n))
        except Exception:
            return self._json({"error": "bad json"}, 400)
        if msg.get("type") == "agent:register":
            HUB.registered = msg
            return self._json({
                "type": "agent:registered",
                "session_id": msg.get("session_id") or "e2eagent",
                "payload": {"tokens": [{"token": "e2e-rw-token", "permission": "rw"}]},
            })
        with HUB.lock:
            HUB.uplink.append(msg)
        return self._json({"ok": True})

    def do_GET(self):
        if not self.path.startswith("/agent/events"):
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        try:
            while True:
                try:
                    msg = HUB.down.get(timeout=1.0)
                except queue.Empty:
                    continue
                data = b"data: " + json.dumps(msg).encode() + b"\n\n"
                self.wfile.write(b"%x\r\n" % len(data) + data + b"\r\n")
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            return


def wait_for(pred, timeout=20, label=""):
    end = time.time() + timeout
    while time.time() < end:
        with HUB.lock:
            r = pred(HUB.uplink)
            if r:
                return r
        time.sleep(0.02)
    raise AssertionError(f"timeout waiting for {label}")


def send_web_request(url, method="GET", rid=None):
    rid = rid or str(uuid.uuid4())
    HUB.down.put({"type": "web:request", "session_id": "e2eagent",
                  "payload": {"request_id": rid, "url": url, "method": method}})
    return rid


def send_web_cancel(rid):
    HUB.down.put({"type": "web:cancel", "session_id": "e2eagent",
                  "payload": {"request_id": rid}})


def collect(rid, timeout=30):
    def done(up):
        ms = [m for m in up if m.get("payload", {}).get("request_id") == rid]
        return ms if ms and ms[-1]["type"] in ("web:end", "web:error") else None
    return wait_for(done, timeout, f"terminal message for {rid}")


def rebuild(ms):
    chunks = [m for m in ms if m["type"] == "web:chunk"]
    seqs = [c["payload"]["seq"] for c in chunks]
    assert seqs == list(range(len(seqs))), f"seq 不连续: {seqs[:12]}"
    for c in chunks:
        assert len(base64.b64decode(c["payload"]["data"])) <= 65536, "块超过 64KiB"
    return b"".join(base64.b64decode(c["payload"]["data"]) for c in chunks)


def main():
    if not os.path.exists(AGENT_BIN):
        sys.exit(f"找不到 agent 二进制 {AGENT_BIN}；先跑 "
                 f"cargo build --no-default-features --bin shell-remote，"
                 f"或用 AGENT_BIN 指定")

    site = ThreadingHTTPServer(("127.0.0.1", SITE_PORT), SiteHandler)
    site.daemon_threads = True
    threading.Thread(target=site.serve_forever, daemon=True).start()

    relay = ThreadingHTTPServer(("127.0.0.1", RELAY_PORT), RelayHandler)
    relay.daemon_threads = True
    threading.Thread(target=relay.serve_forever, daemon=True).start()

    agent = subprocess.Popen(
        [AGENT_BIN, "agent", "--relay-url", f"http://127.0.0.1:{RELAY_PORT}",
         "--session-id", "e2eagent", "--shell", "/bin/bash",
         "--root", "/tmp", "--no-cmd-log"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        run_checks()
    finally:
        agent.terminate()
        try:
            agent.wait(timeout=10)
        except subprocess.TimeoutExpired:
            agent.kill()
        site.shutdown()
        relay.shutdown()


def run_checks():
    reg = wait_for(lambda _up: HUB.registered, 25, "agent registration")
    caps = reg.get("capabilities") or []
    check("注册能力声明包含 web:proxy-v1", "web:proxy-v1" in caps, str(caps))
    # lean 二进制（--no-default-features）也是同一答案 → 没有被 desktop feature 门控。
    check("lean 构建同样声明（与 desktop feature 无关）", "web:proxy-v1" in caps, str(caps))
    print(f"    session={reg.get('session_id')} version={reg.get('agent_version')}")
    time.sleep(0.8)

    print("\n[1] 访问设备自身 127.0.0.1 页面")
    rid = send_web_request(f"http://127.0.0.1:{SITE_PORT}/")
    ms = collect(rid)
    ks = [m["type"] for m in ms]
    check("序列 head → chunk* → end", ks[0] == "web:head" and ks[-1] == "web:end", str(ks))
    head = ms[0]["payload"]
    check("status=200", head["status"] == 200, str(head.get("status")))
    names = [h[0] for h in head["headers"]]
    allowed = {"content-type", "content-length", "cache-control", "etag",
               "last-modified", "content-disposition"}
    check("只转发白名单响应头", set(names) <= allowed, str(names))
    check("保留 content-type", "content-type" in names, str(names))
    check("剔除 set-cookie / CORS / X-Powered-By",
          not any(n in names for n in ("set-cookie", "access-control-allow-origin",
                                       "x-powered-by")), str(names))
    check("final_url 为请求地址", head["final_url"] == f"http://127.0.0.1:{SITE_PORT}/",
          str(head.get("final_url")))
    check("页面内容正确", b"device-local-page" in rebuild(ms))

    print("\n[2] >64KiB 响应按序完整还原")
    ms = collect(send_web_request(f"http://127.0.0.1:{SITE_PORT}/big"))
    body = rebuild(ms)
    expected = bytes(bytearray((i % 256) for i in range(200 * 1024)))
    check("字节逐字节一致", body == expected, f"{len(body)} vs {len(expected)}")
    nchunks = len([m for m in ms if m["type"] == "web:chunk"])
    check("拆成 ≥3 块且每块 ≤64KiB", nchunks >= 3, f"{nchunks} chunks")
    check("终态 end", ms[-1]["type"] == "web:end", ms[-1]["type"])

    print("\n[3] 慢响应期间控制消息分发不阻塞")
    rid_slow = send_web_request(f"http://127.0.0.1:{SITE_PORT}/slow")
    time.sleep(0.5)
    t0 = time.time()
    ms = collect(send_web_request(f"http://127.0.0.1:{SITE_PORT}/"), timeout=15)
    fast_ms = int((time.time() - t0) * 1000)
    check("慢页面在途时第二个请求立即完成",
          ms[-1]["type"] == "web:end" and fast_ms < 3000, f"{fast_ms}ms")
    check("快请求内容正确", b"device-local-page" in rebuild(ms))
    check("慢请求仍在途", not any(m.get("payload", {}).get("request_id") == rid_slow
                              and m["type"] in ("web:end", "web:error") for m in HUB.uplink))

    print("\n[4] 取消在途请求")
    send_web_cancel(rid_slow)
    ms = collect(rid_slow, timeout=10)
    check("只回一个 cancelled 终态",
          ms[-1]["type"] == "web:error" and ms[-1]["payload"]["kind"] == "cancelled",
          str(ms[-1]))
    check("取消后没有 end", all(m["type"] != "web:end" for m in ms))
    send_web_cancel(rid_slow)
    send_web_cancel("no-such-id")
    time.sleep(0.5)
    check("重复/未知 cancel 安全忽略",
          len([m for m in HUB.uplink if m.get("payload", {}).get("request_id") == rid_slow
               and m["type"] == "web:error"]) == 1)

    print("\n[5] 非法参数只回一个错误终态（且不先发 head）")
    for url, method, expect in [
        ("ftp://127.0.0.1/etc/passwd", "GET", "invalid_url"),
        ("file:///etc/passwd", "GET", "invalid_url"),
        ("javascript:alert(1)", "GET", "invalid_url"),
        (f"http://127.0.0.1:{SITE_PORT}/", "POST", "unsupported_method"),
        ("http://127.0.0.1:1/", "GET", "connect"),
    ]:
        ms = collect(send_web_request(url, method), timeout=20)
        ks = [m["type"] for m in ms]
        ok = ks == ["web:error"] and ms[0]["payload"]["kind"] == expect
        check(f"{method} {url} → {expect}", ok,
              f"{ks} {ms[0]['payload'] if ms else ''}")

    print("\n[6] 并发上限 8 → busy；取消后配额归还")
    slow = [send_web_request(f"http://127.0.0.1:{SITE_PORT}/slow") for _ in range(8)]
    time.sleep(1.0)
    ms = collect(send_web_request(f"http://127.0.0.1:{SITE_PORT}/"), timeout=10)
    check("第 9 个并发请求回 busy",
          len(ms) == 1 and ms[0]["type"] == "web:error"
          and ms[0]["payload"]["kind"] == "busy", str(ms))
    for r in slow:
        send_web_cancel(r)
    time.sleep(1.0)
    ms = collect(send_web_request(f"http://127.0.0.1:{SITE_PORT}/"), timeout=15)
    check("取消后配额归还，新请求正常完成", ms[-1]["type"] == "web:end", str(ms[-1]))

    print()
    if FAILS:
        print(f"FAILED ({len(FAILS)}): {FAILS}")
        sys.exit(1)
    print("ALL CHECKS PASSED")


if __name__ == "__main__":
    main()
