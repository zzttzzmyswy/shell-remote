import ssl, time, sys, struct
import websocket

token = sys.argv[1] if len(sys.argv) > 1 else "winshare"
host  = sys.argv[2] if len(sys.argv) > 2 else "127.0.0.1"
port  = sys.argv[3] if len(sys.argv) > 3 else "3902"

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

url = "wss://%s:%s/agent/desktop/ws?token=%s" % (host, port, token)
ws = websocket.create_connection(url, sslopt={"cert_reqs": ssl.CERT_NONE}, timeout=15)

t0 = time.time()
got_init = False
got_frag = False
init_len = 0
frag_cnt = 0
srtcs = []
try:
    while time.time() - t0 < 8:
        try:
            frame = ws.recv()
        except Exception:
            break
        if not isinstance(frame, bytes):
            continue
        if frame[4:8] == b"moov" or frame[4:8] == b"ftyp":
            if not got_init:
                got_init = True
                init_len = len(frame)
                has_vpcC = b"vpcC" in frame
                has_avcC = b"avcC" in frame
                print("INIT len=%d vpcC=%s avcC=%s" % (init_len, has_vpcC, has_avcC))
                if has_vpcC:
                    i = frame.find(b"vpcC")
                    # vpcC 是 FullBox: [i+4]=version [i+5..7]=flags [i+8]=profile [i+9]=level
                    print("  vpcC payload: profile=%d level=%d" % (frame[i+8], frame[i+9]))
        elif frame[4:8] == b"moof":
            frag_cnt += 1
            got_frag = True
            ts = find_srtc(frame)
            if ts:
                srtcs.append(ts)
        if time.time() - t0 > 0.05 and frag_cnt > 60:
            break
finally:
    try:
        ws.close()
    except Exception:
        pass

print("frags=%d" % frag_cnt)
if len(srtcs) >= 2:
    gaps = [srtcs[i+1] - srtcs[i] for i in range(len(srtcs)-1)]
    print("srtc gaps ms max=%.0f avg=%.0f" % (max(gaps), sum(gaps)/len(gaps)))
print("result:", "VP9 OK" if (got_init and b"vpcC" in b"" and frag_cnt > 0) else "...")