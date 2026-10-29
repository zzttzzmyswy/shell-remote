import ssl, time, sys, struct
import websocket

token = sys.argv[1] if len(sys.argv) > 1 else "winshare"
host  = sys.argv[2] if len(sys.argv) > 2 else "127.0.0.1"
port  = sys.argv[3] if len(sys.argv) > 3 else "3902"
limit = int(sys.argv[4]) if len(sys.argv) > 4 else 300

def find_srtc(buf):
    m = buf.find(b"moof")
    if m < 0 or m < 4:
        return None
    moof_sz = struct.unpack(">I", buf[m - 4:m])[0]
    if moof_sz < 8:
        return None
    m_end = m - 4 + moof_sz
    i = m + 4
    pend, psize = None, 0
    while i + 8 <= m_end:
        size, typ = struct.unpack(">I4s", buf[i:i + 8])
        if typ == b"traf":
            pend, psize = i + 8, size
            break
        if size < 8:
            break
        i += size
    if pend is None:
        return None
    j = pend
    tend = pend + psize - 8
    while j + 8 <= tend:
        size, typ = struct.unpack(">I4s", buf[j:j + 8])
        if typ == b"srtc" and j + 16 <= len(buf):
            return struct.unpack(">Q", buf[j + 8:j + 16])[0]
        if size < 8:
            break
        j += size
    return None

def frame_bytes(buf):
    # 帧总字节近似 = mdat box 大小 + 少量 moof；直接量测整框大小
    return len(buf)

url = "wss://%s:%s/agent/desktop/ws?token=%s" % (host, port, token)
ws = websocket.create_connection(url, sslopt={"cert_reqs": ssl.CERT_NONE}, timeout=15)

t0 = time.time()
arrivals = []
srtcs = []
fbytes = []
frame_no = 0
try:
    while frame_no < limit:
        try:
            frame = ws.recv()
        except Exception:
            break
        if not isinstance(frame, bytes):
            continue
        ts = find_srtc(frame)
        if ts is None:
            continue  # init
        frame_no += 1
        arrivals.append(time.time() - t0)
        srtcs.append(ts)
        fbytes.append(len(frame))
finally:
    try:
        ws.close()
    except Exception:
        pass

print("frags: %d  wall %.2fs" % (frame_no, time.time() - t0))
if frame_no >= 2:
    sgap = [srtcs[i + 1] - srtcs[i] for i in range(frame_no - 1)]
    srange = max(srtcs[-1] - srtcs[0], 1)
    print("capture fps(srtc): %.1f" % ((frame_no - 1) * 1000.0 / srange))
    big = [g for g in sgap if g > 150]
    print("srtc max gap: %.0f ms  gaps>150ms: %d/%d" % (max(sgap), len(big), len(sgap)))
    # 每帧即时码率: frame_bytes*8/frame_interval_ms * 1000, 对流上的字节测
    total_bytes = sum(fbytes)
    dur_s = max(srtcs[-1] - srtcs[0], 1) / 1000.0
    print("avg kbps(frame bytes): %.0f   total %.1f KB   avg_bytes/f %.1f" %
          (total_bytes * 8 / dur_s / 1000, total_bytes / 1024, total_bytes / frame_no))
    inst = []
    for i in range(1, frame_no):
        ms = max(srtcs[i] - srtcs[i - 1], 1)
        inst.append(fbytes[i] * 8.0 / ms)  # kbps of this frame
    inst.sort()
    print("instant kbps p50/p95/max: %.0f / %.0f / %.0f" %
          (inst[len(inst)//2], inst[int(len(inst)*0.95)], inst[-1]))
    top = sorted(range(len(fbytes)), key=lambda i: fbytes[i], reverse=True)[:8]
    print("largest frames KB:", ["%.0f" % (fbytes[i]/1024) for i in top])
    print("frame KB head:", ", ".join("%.1f" % (b/1024) for b in fbytes[:24]))