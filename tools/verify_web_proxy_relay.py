#!/usr/bin/env python3
"""MYS-2383 阶段2 真实验证：relay 网页流式代理 + 短期资源凭据（ticket）。

一条命令跑完整链路：**真 relay 二进制 + 真 agent 二进制 + 只监听 127.0.0.1
的测试站**，只走公开 HTTP 接口（不 mock relay 内部状态）。

agent 用 `--key` 固定密钥启动，因此 rw token 就是该密钥、ro 侧另起一个
`--token-type ro` 的 agent —— 脚本无需从 agent 进程里取随机会话 token。

覆盖：
  1. 能力协商：浏览器 SSE 握手补发 `session:capabilities`（只含 capabilities）。
  2. `POST /agent/web/ticket`：口令 + 会话 token 鉴权、ro/rw 均可、
     ticket 随机且不可从会话 token 推导、有效期上报。
  3. `GET /agent/web/proxy`：状态/headers 透传、`X-SR-Final-URL`、body 完整、
     >64KiB 分块还原、目标站 404/重定向原样透传。
  4. 拒绝码：无凭据 401 / 坏 ticket 403 / 旧 agent 404 / 非法参数 400。
  5. 隔离性：慢响应在途时终端控制 POST 仍被立即接受。
  6. 客户端断开 → relay 向 agent 发 `web:cancel`（测试站观测到传输停止）。
  7. 并发上限：同会话 8 个在途 → 第 9 个 429。
  8. agent 断线：ticket 撤销（403）、代理返回 503、在途请求结束。

用法（先构建）：cargo build --bin shell-remote && python3 tools/verify_web_proxy_relay.py
环境变量：BIN / RELAY_PORT / SITE_PORT / RELAY_PASSWORD。
"""

import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("BIN", os.path.join(ROOT, "target", "debug", "shell-remote"))
RELAY_PORT = int(os.environ.get("RELAY_PORT", "18820"))
SITE_PORT = int(os.environ.get("SITE_PORT", "18821"))
RELAY_PASSWORD = os.environ.get("RELAY_PASSWORD", "e2e-pass")
BASE = f"http://127.0.0.1:{RELAY_PORT}"
SITE = f"http://127.0.0.1:{SITE_PORT}"
# 测试站自己声明的 CORS 值：relay 必须**不**透传它。
SITE_ORIGIN = "https://target-origin.example"

RW_KEY = "e2erwkey01"
RO_KEY = "e2erokey01"
RW_SID = "e2eagent01"
RO_SID = "e2eagent02"

FAILS = []
CHECKS = [0]


def check(name, cond, detail=""):
    CHECKS[0] += 1
    if cond:
        print(f"  PASS {name}")
    else:
        print(f"  FAIL {name}" + (f"   <- {detail}" if detail else ""))
        FAILS.append(name)


# ───────────────────── 只监听 127.0.0.1 的测试站（= 设备自身页面）─────────────────────

BIG = bytes(bytearray((i % 256) for i in range(200 * 1024)))
SLOW_CHUNKS = 300          # × 0.1s ≈ 30s，足够在测试窗口内制造「在途」
_slow = {"started": 0, "written": 0}
_slow_lock = threading.Lock()
_slow_reset_at = [0]


class SiteHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_GET(self):
        if self.path.startswith("/big"):
            self._send(200, BIG, "application/octet-stream")
        elif self.path.startswith("/slow"):
            self._slow()
        elif self.path.startswith("/redirect"):
            self.send_response(302)
            self.send_header("Location", "/final")
            self.send_header("Content-Length", "0")
            self.end_headers()
        elif self.path.startswith("/final"):
            self._send(200, b"<html><body>final-target</body></html>",
                       "text/html; charset=utf-8")
        elif self.path.startswith("/notfound"):
            self._send(404, b"<html>missing</html>", "text/html; charset=utf-8")
        else:
            self._send(200,
                       b"<html><head><title>t</title></head>"
                       b"<body>device-local-page</body></html>",
                       "text/html; charset=utf-8")

    def _send(self, code, body, ctype):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        # 这些头必须被 agent 剔除，不得出现在 relay 回包里：
        self.send_header("Set-Cookie", "secret=leak")
        self.send_header("Access-Control-Allow-Origin", SITE_ORIGIN)
        self.send_header("X-Powered-By", "test")
        self.end_headers()
        self.wfile.write(body)

    def _slow(self):
        """缓慢持续吐字节，用来观测「客户端/agent 断开后是否停发」。

        `?n=<块数>` 控制长度：短请求（隔离性用例）用完即走，长请求（断开
        用例）留出观测窗口，两者互不污染全局计数器。
        """
        n = SLOW_CHUNKS
        if "?" in self.path:
            q = urllib.parse.parse_qs(self.path.split("?", 1)[1])
            try:
                n = int(q.get("n", [SLOW_CHUNKS])[0])
            except ValueError:
                pass
        with _slow_lock:
            _slow["started"] += 1
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        try:
            for i in range(n):
                payload = b"x" * 1024
                self.wfile.write(b"%x\r\n" % len(payload) + payload + b"\r\n")
                self.wfile.flush()
                with _slow_lock:
                    _slow["written"] = i + 1
                time.sleep(0.1)
            self.wfile.write(b"0\r\n\r\n")
        except (BrokenPipeError, ConnectionResetError):
            pass


def slow_started():
    with _slow_lock:
        return _slow["started"]


def slow_written():
    with _slow_lock:
        return _slow["written"]


# ────────────────────────────── HTTP 客户端 ──────────────────────────────

def _open(req, timeout):
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, dict(r.headers), r.read()
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read()


def http_get(path, headers=None, timeout=60):
    return _open(urllib.request.Request(BASE + path, headers=headers or {}), timeout)


def http_post(path, body, headers=None, timeout=60):
    data = json.dumps(body).encode() if isinstance(body, (dict, list)) else body
    h = {"Content-Type": "application/json"}
    h.update(headers or {})
    return _open(urllib.request.Request(BASE + path, data=data, headers=h, method="POST"),
                 timeout)


def wait_relay(timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.create_connection(("127.0.0.1", RELAY_PORT), timeout=0.5):
                return True
        except OSError:
            time.sleep(0.1)
    return False


def get_ticket(token, password=RELAY_PASSWORD):
    return http_post("/agent/web/ticket", {}, {"X-SR-Token": token, "X-Auth": password})


def proxy(ticket=None, url=None, token=None, password=RELAY_PASSWORD, timeout=60):
    q = []
    if ticket is not None:
        q.append(("ticket", ticket))
    if url is not None:
        q.append(("url", url))
    path = "/agent/web/proxy"
    if q:
        path += "?" + urllib.parse.urlencode(q)
    h = {}
    if password is not None:
        h["X-Auth"] = password
    if token is not None:
        h["X-SR-Token"] = token
    return http_get(path, h, timeout=timeout)


def sse_events(token, want=3, timeout=15):
    """读浏览器事件流的前 want 条事件。"""
    path = "/agent/session/sse?" + urllib.parse.urlencode(
        {"auth": RELAY_PASSWORD, "token": token})
    req = urllib.request.Request(BASE + path, headers={"Accept": "text/event-stream"})
    out, buf = [], b""
    with urllib.request.urlopen(req, timeout=timeout) as r:
        end = time.time() + timeout
        while len(out) < want and time.time() < end:
            chunk = r.read(1)
            if not chunk:
                break
            buf += chunk
            while b"\n\n" in buf:
                block, buf = buf.split(b"\n\n", 1)
                payload = "\n".join(
                    line[5:].lstrip().decode() if line.startswith(b"data:") else ""
                    for line in block.split(b"\n")).strip()
                if not payload:
                    continue
                try:
                    out.append(json.loads(payload))
                except json.JSONDecodeError:
                    pass
                if len(out) >= want:
                    break
    return out


def wait_agent_online(token, sid, timeout=40):
    """轮询浏览器 SSE 直到 200（= 该会话的 agent 通道已建立）。

    SSE 是**永不结束**的流：不能直接 `http_get`（它会一直读到超时并抛
    ReadTimeout），只发一个只读头的请求、拿到状态码就断开。
    """
    end = time.time() + timeout
    path = "/agent/session/sse?" + urllib.parse.urlencode(
        {"auth": RELAY_PASSWORD, "token": token})
    while time.time() < end:
        try:
            with socket.create_connection(("127.0.0.1", RELAY_PORT), timeout=3) as s:
                req = (f"GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{RELAY_PORT}\r\n"
                       f"Accept: text/event-stream\r\nConnection: close\r\n\r\n")
                s.sendall(req.encode())
                head = s.recv(64).decode("latin-1", "replace")
                if head.startswith("HTTP/1.1 200"):
                    return True
        except OSError:
            pass
        time.sleep(0.3)
    return False


# ────────────────────────────── 断言流程 ──────────────────────────────

def run_checks(rw, ro, agent_procs):
    print("\n[1] 能力协商：浏览器 SSE 握手补发 session:capabilities")
    evs = sse_events(rw, want=3)
    types = [e.get("type") for e in evs]
    check("connected 事件在最前", bool(types) and types[0] == "browser:connected", str(types))
    caps_ev = next((e for e in evs if e.get("type") == "session:capabilities"), None)
    check("收到 session:capabilities", caps_ev is not None, str(types))
    if caps_ev:
        check("payload 只含 capabilities 数组",
              set(caps_ev["payload"]) == {"capabilities"}, str(caps_ev["payload"]))
        check("新 agent 声明 web:proxy-v1",
              "web:proxy-v1" in caps_ev["payload"]["capabilities"],
              str(caps_ev["payload"]))

    print("\n[2] POST /agent/web/ticket 鉴权与 ticket 属性")
    code, _, _ = get_ticket(rw, password="wrong")
    check("错误服务器口令 → 401", code == 401, str(code))
    code, _, _ = http_post("/agent/web/ticket", {},
                           {"X-SR-Token": "bogus", "X-Auth": RELAY_PASSWORD})
    check("无效会话 token → 401", code == 401, str(code))
    code, _, _ = http_post("/agent/web/ticket", {}, {"X-Auth": RELAY_PASSWORD})
    check("缺会话 token → 401", code == 401, str(code))

    code, _, data = get_ticket(rw)
    check("rw token 可签发", code == 200, str(code))
    v = json.loads(data)
    t_rw = v.get("ticket", "")
    check("响应含 expires_in=600", v.get("expires_in") == 600, str(v))
    check("ticket 为 64 位 hex",
          len(t_rw) == 64 and all(c in "0123456789abcdef" for c in t_rw), t_rw)
    check("ticket 不等于会话 token", t_rw != rw)
    check("响应体不含任何会话 token", rw.encode() not in data and ro.encode() not in data)

    code, _, data = get_ticket(ro)
    check("ro token 可签发（页面 GET 只读）", code == 200, str(code))
    t_ro = json.loads(data).get("ticket", "")
    check("两次签发的 ticket 不同（随机）", t_ro != t_rw)

    print("\n[3] GET /agent/web/proxy 正常路径")
    code, hdrs, body = proxy(ticket=t_rw, url=SITE + "/", password=None)
    check("ticket 路径无需 X-Auth（签发时已鉴权）", code == 200, str(code))
    check("body 为远端页面", b"device-local-page" in body, body[:120])
    low = {k.lower() for k in hdrs}
    check("content-type 透传", "text/html" in hdrs.get("content-type", ""),
          str(hdrs.get("content-type")))
    check("X-SR-Final-URL 为请求地址", hdrs.get("x-sr-final-url") == SITE + "/",
          str(hdrs.get("x-sr-final-url")))
    check("Set-Cookie 未漏给浏览器", "set-cookie" not in low, str(sorted(low)))
    # relay 有一条**既有**的全局 CorsLayer（`access-control-allow-origin: *`，
    # 对所有路由生效，MCP/SSE 也一样），不是本卡引入的。这里断言的是：目标站
    # 自己的 CORS 值没有被透传（agent 必须剔除它）。
    check("目标站自己的 CORS 值未透传",
          SITE_ORIGIN not in hdrs.get("access-control-allow-origin", ""),
          f"ACAO={hdrs.get('access-control-allow-origin')!r} 应不含 {SITE_ORIGIN}")
    check("X-Powered-By 未漏给浏览器", "x-powered-by" not in low)

    code, hdrs, body = proxy(url=SITE + "/", token=ro)
    check("ro token 走 header 鉴权也能读页面",
          code == 200 and b"device-local-page" in body, f"{code} {body[:120]}")
    check("ro 路径同样带 X-SR-Final-URL", hdrs.get("x-sr-final-url") == SITE + "/")

    code, _, body = proxy(url=SITE + "/notfound", token=rw)
    check("目标站 404 原样透传（浏览器要看得到错误页）", code == 404, str(code))

    code, hdrs, body = proxy(url=SITE + "/redirect", token=rw)
    check("重定向后 X-SR-Final-URL 指向最终地址",
          hdrs.get("x-sr-final-url") == SITE + "/final",
          str(hdrs.get("x-sr-final-url")))
    check("重定向后 body 正确", b"final-target" in body, body[:120])

    print("\n[4] >64KiB 分块按序完整还原")
    code, _, body = proxy(url=SITE + "/big", token=rw, timeout=120)
    check("200KiB 响应 status=200", code == 200, str(code))
    check("字节逐字节一致", body == BIG, f"{len(body)} vs {len(BIG)}")

    print("\n[5] 拒绝路径状态码")
    code, _, _ = proxy(ticket="deadbeef" * 8, url=SITE + "/", password=None)
    check("无效 ticket → 403", code == 403, str(code))
    code, _, _ = proxy(url=SITE + "/")
    check("无任何凭据 → 401", code == 401, str(code))
    code, _, _ = proxy(url=SITE + "/", token="bogus")
    check("无效会话 token → 401", code == 401, str(code))
    code, _, _ = proxy(url="/relative", token=rw)
    check("相对 URL → 400", code == 400, str(code))
    code, _, _ = proxy(url="ftp://127.0.0.1/", token=rw)
    check("非法 scheme → 400", code == 400, str(code))
    code, _, _ = proxy(url="http://127.0.0.1/" + "a" * 4100, token=rw)
    check("URL 超 4096 → 400", code == 400, str(code))
    code, _, _ = http_get(
        "/agent/web/proxy?" + urllib.parse.urlencode(
            [("url", SITE + "/"), ("url", SITE + "/big")]),
        {"X-Auth": RELAY_PASSWORD, "X-SR-Token": rw})
    check("url 参数重复 → 400", code == 400, str(code))

    print("\n[6] 隔离性：慢响应在途时终端控制 POST 仍及时接受")
    slow_result = {}
    th = threading.Thread(
        target=lambda: slow_result.update(r=proxy(url=SITE + "/slow?n=12", token=rw,
                                                 timeout=60)))
    th.start()
    time.sleep(0.6)
    t0 = time.time()
    code, _, _ = http_post(
        "/agent/send",
        {"type": "terminal:input", "session_id": RW_SID, "payload": {"data": "echo hi\n"}},
        {"X-Auth": RELAY_PASSWORD, "X-SR-Token": rw}, timeout=10)
    elapsed = time.time() - t0
    check("终端控制 POST 仍被接受（200）", code == 200, str(code))
    check("终端控制 POST 延迟 < 2s", elapsed < 2.0, f"{elapsed:.2f}s")
    check("慢响应在途时终端 POST 未被串行阻塞", th.is_alive())
    th.join(timeout=30)
    check("短慢请求正常完成（隔离性用例自身收尾）",
          slow_result.get("r", (0,))[0] in (200,), str(slow_result.get("r", (0,))[0]))

    print("\n[7] 客户端断开 → relay 向 agent 发 web:cancel（传输停止）")
    conn = urllib.request.urlopen(
        urllib.request.Request(
            BASE + "/agent/web/proxy?" + urllib.parse.urlencode(
                [("url", SITE + "/slow"), ("ticket", t_rw)]),
            headers={}), timeout=30)
    first = conn.read(1024)
    check("断开前已收到远端 body 数据", len(first) > 0, str(len(first)))
    before = slow_written()
    conn.close()          # 客户端断开
    time.sleep(4.0)
    after = slow_written()
    check("客户端断开后 agent 停止读取（web:cancel 生效）", after - before <= 5,
          f"断开后又写了 {after - before} 块")

    print("\n[8] 并发上限：同会话 8 个在途 → 第 9 个 429")
    # 每个占位请求都留足观测窗口（> 本步骤 + 后续步骤的等待时间），用完由
    # agent 断线统一取消；`n=200` ≈ 20s。
    threads = [threading.Thread(
        target=lambda: proxy(url=SITE + "/slow?n=200", token=rw, timeout=180),
        daemon=True)
        for _ in range(8)]
    for t in threads:
        t.start()
    time.sleep(2.0)
    code, _, _ = proxy(url=SITE + "/", token=rw, timeout=15)
    check("第 9 个并发请求 → 429", code == 429, str(code))

    print("\n[9] 旧 agent（未声明能力）：404 且不触发网络请求")
    old_sid = "oldagent01"
    code, _, data = http_post(
        "/agent/send",
        {"type": "agent:register", "token_type": "rw", "session_id": old_sid})
    check("旧 agent 会话注册成功", code == 200, str(code))
    old_rw = json.loads(data)["payload"]["tokens"][0]["token"]
    ev = subprocess.Popen(
        ["curl", "-sN", f"{BASE}/agent/events?session={old_sid}",
         "-H", f"X-SR-Agent-Token: {old_rw}"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        time.sleep(1.2)
        code, _, _ = get_ticket(old_rw)
        check("旧 agent：POST ticket → 404", code == 404, str(code))
        started_before = slow_started()
        code, _, _ = proxy(url=SITE + "/slow", token=old_rw, timeout=15)
        check("旧 agent：GET proxy → 404", code == 404, str(code))
        time.sleep(1.0)
        check("旧 agent：未触发任何网络请求（测试站未被打到）",
              slow_started() == started_before,
              f"{slow_started() - started_before} 次")
    finally:
        ev.terminate()
        ev.wait(timeout=5)

    print("\n[10] agent 断线：ticket 撤销 + 代理 503 + 在途请求结束")
    code, _, _ = proxy(ticket=t_ro, url=SITE + "/", password=None, timeout=20)
    check("断线前 ticket 可用", code == 200, str(code))
    # 两个 agent 都要杀掉：t_ro 属于 ro agent 的会话，只杀 rw agent 的话它
    # 仍然有效（这条断言曾因此假失败）。
    for p in agent_procs:
        p.terminate()
    for p in agent_procs:
        try:
            p.wait(timeout=15)
        except subprocess.TimeoutExpired:
            p.kill()
    # relay 的断线清理在 SSE Drop 里 tokio::spawn，给一点收敛时间。
    time.sleep(3.0)
    code, _, _ = proxy(ticket=t_ro, url=SITE + "/", password=None, timeout=20)
    check("断线后旧 ticket → 403（已撤销）", code == 403, str(code))
    code, _, _ = proxy(url=SITE + "/", token=rw, timeout=20)
    check("断线后代理 → 503（agent 离线）", code == 503, str(code))


def main():
    if not os.path.exists(BIN):
        sys.exit(f"找不到二进制 {BIN}；先跑 cargo build --bin shell-remote，或用 BIN 指定")

    site = ThreadingHTTPServer(("127.0.0.1", SITE_PORT), SiteHandler)
    site.daemon_threads = True
    threading.Thread(target=site.serve_forever, daemon=True).start()

    workdir = tempfile.mkdtemp(prefix="sr2383-relay-")
    relay_log = open(os.path.join(workdir, "relay.log"), "w")
    relay = subprocess.Popen(
        [BIN, "relay", "--bind", f"127.0.0.1:{RELAY_PORT}", "--auth", RELAY_PASSWORD,
         "--no-tls", "--settings-file", os.path.join(workdir, "settings.json")],
        stdout=relay_log, stderr=subprocess.STDOUT)
    agents = []
    try:
        if not wait_relay():
            sys.exit(f"relay 未在 {RELAY_PORT} 就绪（日志 {workdir}/relay.log）")
        print(f"relay {BASE} / 测试站 {SITE} / 工作目录 {workdir}")

        # rw agent：--key 固定密钥 → rw token 就是密钥本身（脚本可见）。
        rw_agent = subprocess.Popen(
            [BIN, "agent", "--relay-url", BASE, "--key", RW_KEY, "--session-id", RW_SID,
             "--token-type", "both", "--shell", "/bin/bash", "--root", "/tmp", "--no-cmd-log"],
            stdout=open(os.path.join(workdir, "agent-rw.log"), "w"),
            stderr=subprocess.STDOUT)
        agents.append(rw_agent)
        # ro agent：--token-type ro → 唯一 token 是 ReadOnly。
        ro_agent = subprocess.Popen(
            [BIN, "agent", "--relay-url", BASE, "--key", RO_KEY, "--session-id", RO_SID,
             "--token-type", "ro", "--shell", "/bin/bash", "--root", "/tmp", "--no-cmd-log"],
            stdout=open(os.path.join(workdir, "agent-ro.log"), "w"),
            stderr=subprocess.STDOUT)
        agents.append(ro_agent)

        print("\n[0] 真 agent 上线")
        check("rw agent 注册并建立通道", wait_agent_online(RW_KEY, RW_SID), RW_SID)
        check("ro agent 注册并建立通道", wait_agent_online(RO_KEY, RO_SID), RO_SID)
        if FAILS:
            sys.exit("agent 未上线，后续用例无意义")

        run_checks(RW_KEY, RO_KEY, [rw_agent, ro_agent])
    finally:
        for a in agents:
            if a.poll() is None:
                a.terminate()
                try:
                    a.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    a.kill()
        relay.terminate()
        try:
            relay.wait(timeout=10)
        except subprocess.TimeoutExpired:
            relay.kill()
        relay_log.close()
        site.shutdown()

    print()
    if FAILS:
        print(f"FAIL：{len(FAILS)}/{CHECKS[0]} 项未通过")
        for f in FAILS:
            print(f"  - {f}")
        return 1
    print(f"PASS：{CHECKS[0]}/{CHECKS[0]} 项全部通过")
    return 0


if __name__ == "__main__":
    sys.exit(main())
