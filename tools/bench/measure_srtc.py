import ssl, time, sys, struct
import websocket

token = sys.argv[1] if len(sys.argv) > 1 else "winshare"
host  = sys.argv[2] if len(sys.argv) > 2 else "127.0.0.1"
port  = sys.argv[3] if len(sys.argv) > 3 else "3902"
limit = int(sys.argv[4]) if len(sys.argv) > 4 else 120

def find_srtc(buf):
    m = buf.find(b"moof")
    if m < 0 or m < 4:
        return None
    moof_sz = struct.unpack(">I", buf[m - 4:m])[0]
    if moof_sz < 8:
        return None
    m_end = m - 4 + moof_sz
    # moof 内部 children: mfhd / traf(含 srtc)。从 moof payload(m+4) 起遍历。
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
    tend = pend + psize - 8  # traf box 末尾
    while j + 8 <= tend:
        size, typ = struct.unpack(">I4s", buf[j:j + 8])
        if typ == b"srtc" and j + 16 <= len(buf):
            return struct.unpack(">Q", buf[j + 8:j + 16])[0]
        if size < 8:
            break
        j += size
    return None

url = "wss://%s:%s/agent/desktop/ws?token=%s" % (host, port, token)
ws = websocket.create_connection(url, sslopt={"cert_reqs": ssl.CERT_NONE}, timeout=15)

t0 = time.time()          # wall epoch (s)
recv = []                 # wall ms each frag received
srtcs = []                # agent capture epoch ms carried by each frag
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
            continue
        frame_no += 1
        recv.append(time.time() * 1000.0 - t0 * 1000.0)  # ms offset from t0
        srtcs.append(ts)
finally:
    try:
        ws.close()
    except Exception:
        pass

print("frags:", frame_no, "wall: %.2fs" % (time.time() - t0))
if frame_no >= 2:
    sgap = [srtcs[i + 1] - srtcs[i] for i in range(frame_no - 1)]
    srange = max(srtcs[-1] - srtcs[0], 1)
    print("capture fps(srtc): %.1f" % ((frame_no - 1) * 1000.0 / srange))
    big = [g for g in sgap if g > 150]
    print("srtc gap ms head: [%s]" % ", ".join("%.0f" % g for g in sgap[:40]))
    print("srtc max gap: %.0f ms   srtc gaps>150ms: %d/%d" % (max(sgap), len(big), len(sgap)))
    # 同机近似管道延迟: viewer 收帧时刻(epoch) - 采集时刻(srtc)
    pipe = [recv[i] + t0 * 1000.0 - srtcs[i] for i in range(frame_no)]
    print("pipe ms: [%s]" % ", ".join("%.0f" % p for p in pipe[1:20]))
    print("pipe ms median: %.0f  min: %.0f  max: %.0f" %
          (sorted(pipe)[len(pipe) // 2], min(pipe), max(pipe)))