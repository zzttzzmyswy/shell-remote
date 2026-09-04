# 抓取 VP9 fMP4 流（init + 若干 frag）存本地，供浏览器 WebCodecs 解码验证
import ssl, sys
import websocket

token = sys.argv[1]
out   = sys.argv[2]
host  = sys.argv[3] if len(sys.argv) > 3 else "127.0.0.1"
port  = sys.argv[4] if len(sys.argv) > 4 else "3902"

ws = websocket.create_connection(
    "wss://%s:%s/agent/desktop/ws?token=%s" % (host, port, token),
    sslopt={"cert_reqs": ssl.CERT_NONE}, timeout=15)

init = None
frags = []
t0 = __import__("time").time()
while len(frags) < 40 and __import__("time").time() - t0 < 8:
    f = ws.recv()
    if not isinstance(f, bytes):
        continue
    t = f[4:8]
    if t == b"moov" or t == b"ftyp":
        if init is None:
            init = f
    elif t == b"moof":
        frags.append(f)
try:
    ws.close()
except Exception:
    pass

with open(out, "wb") as fh:
    assert init is not None
    fh.write(init)
    for fr in frags:
        fh.write(fr)
print("saved %s: init=%dB frags=%d total=%d" % (out, len(init), len(frags), len(init) + sum(len(x) for x in frags)))