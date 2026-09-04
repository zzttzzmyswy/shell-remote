import ssl, time, sys
import websocket

token = sys.argv[1] if len(sys.argv) > 1 else "winshare"
host  = sys.argv[2] if len(sys.argv) > 2 else "127.0.0.1"
port  = sys.argv[3] if len(sys.argv) > 3 else "3902"
limit = int(sys.argv[4]) if len(sys.argv) > 4 else 90

url = "wss://%s:%s/agent/desktop/ws?token=%s" % (host, port, token)
ws = websocket.create_connection(url, sslopt={"cert_reqs": ssl.CERT_NONE}, timeout=15)

arrivals = []
frame_no = 0
t0 = time.time()
try:
    while frame_no < limit:
        try:
            frame = ws.recv()
        except Exception:
            break
        if not isinstance(frame, bytes):
            continue
        frame_no += 1
        arrivals.append(time.time() - t0)
finally:
    try:
        ws.close()
    except Exception:
        pass

print("frames:", frame_no, "duration: %.2fs" % (time.time() - t0))
if len(arrivals) >= 2:
    gaps = [arrivals[i + 1] - arrivals[i] for i in range(len(arrivals) - 1)]
    print("fps: %.1f" % (len(arrivals) / max(arrivals[-1] - arrivals[0], 1e-6)))
    print("gaps ms: [%s]" % ", ".join("%.0f" % (g * 1000) for g in gaps[:60]))
    bad = [g for g in gaps if g > 0.150]
    print("max gap ms: %.0f  gaps>150ms: %d/%d" % (max(gaps) * 1000, len(bad), len(gaps)))