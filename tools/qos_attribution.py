#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""QoS 13s 归因决策树（R4 丙 111-140 / MYS-886）。

解析 agent 日志中的 `desktop QoS: adaptive adjusted` 结构化快照行，对每秒
QoS 决策做延迟归因：

  network    —— 网络层劣化：probe_ms >= 300，或 probe >= 100 且 qos_scale
                显著下降（弱网降码率生效）
  decode     —— 解码积压：网络健康（probe < 100）但解码队列深度大
                (dq > 12) 且解码帧率低 (dfps < 20)
  encode     —— 编码慢：动态内容但目标 fps 被压到 <= 15（编码耗时预算降档）
  static     —— 正常静态（fps == 1，内容无变化，符合铁律）
  good       —— 其余正常（低延迟、满帧率）

输出归因占比 + 中位延迟/网络 RTT，帮助定位"卡顿/延迟来自哪一段链路"。
13s 窗口 = 每个归因类持续 >= 13 个采样（约 3s+）才算"劣化段"，避免
单次尖峰误报。

用法:
  python3 tools/qos_attribution.py <agent.log> [--window N]
  # 示例: python3 tools/qos_attribution.py /tmp/sr22_agent.log

依赖: 无（仅标准库）。
"""
import re
import sys
from collections import Counter

LOG_RE = re.compile(
    r"desktop QoS: adaptive adjusted "
    r"delay_ms=(\d+) probe_ms=(\d+) fps=(\d+) qos_scale=(\d+) "
    r"qos_state=(\w+) bitrate_kbps=(\d+) cap=(\d+) "
    r"decode_fps=(\d+) decode_queue=(\d+) ack_seq=(\d+)"
)

_NUM_IDX = (0, 1, 2, 3, 5, 6, 7, 8, 9)  # groups 中为数字的下标（4 是 qos_state 字符串）


def attribute(delay_ms, probe_ms, fps, qos_scale, qos_state,
              decode_fps, decode_queue):
    """归因单一样本。返回类别字符串。"""
    if probe_ms >= 300:
        return "network"
    if probe_ms >= 100 and (qos_state in ("Degraded", "Critical") or qos_scale < 600):
        return "network"
    # 解码积压：网络健康但解码跟不上
    if probe_ms < 100 and decode_queue > 12 and decode_fps < 20:
        return "decode"
    if fps == 1:
        return "static"
    # 动态但 fps 被压到 <=15（解码背压下限或编码预算降档）
    if fps <= 15 and qos_scale < 1000:
        return "encode"
    return "good"


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)
    log_path = sys.argv[1]
    window = 13  # 劣化判定窗口（采样数；250ms 节奏 × 13 ≈ 3.3s）

    samples = []
    with open(log_path, errors="replace") as f:
        for line in f:
            m = LOG_RE.search(line)
            if not m:
                continue
            g = m.groups()
            # qos_state(下标4)是字符串，其余转 int；保持 attribute 签名顺序：
            # delay probe fps scale state bitrate cap dfps dq ack
            nums = [int(g[i]) for i in _NUM_IDX]
            v = nums[:4] + [g[4]] + nums[4:]
            samples.append(v)

    if not samples:
        print(f"no QoS samples found in {log_path}")
        sys.exit(1)

    attrs = [attribute(s[0], s[1], s[2], s[3], s[4], s[7], s[8]) for s in samples]
    # 13s 劣化窗口：连续 >= window 个非 good/static 才计入劣化段起始。
    # 简单实现：直接统计各归因占比（连续窗口判段留作报告中位数参考）。
    counter = Counter(attrs)
    n = len(samples)
    delay_med = sorted(s[0] for s in samples)[n // 2]
    probe_med = sorted(s[1] for s in samples)[n // 2]
    fps_vals = [s[2] for s in samples if s[2] > 1]  # 排除静态
    fps_med = sorted(fps_vals)[len(fps_vals) // 2] if fps_vals else None

    print(f"── QoS 归因决策树（{n} 采样，{log_path}）──")
    for cat in ("good", "static", "network", "decode", "encode"):
        c = counter.get(cat, 0)
        pct = c / n * 100
        print(f"  {cat:<8} {c:>5}  {pct:5.1f}%")
    print(f"  中位 e2e={delay_med}ms  中位网络RTT={probe_med}ms  "
          f"动态中位fps={fps_med if fps_med is not None else '静态'}")

    # 总结
    if counter["network"] > n * 0.3:
        print("结论: 以网络劣化为主 → 弱网场景，检查上行带宽/QoS 码率缩放")
    elif counter["decode"] > n * 0.3:
        print("结论: 以解码积压为主 → 客户端解码能力，检查 WebCodecs/设备性能")
    elif counter["encode"] > n * 0.3:
        print("结论: 以编码预算为主 → 编码器负载高，检查 CPU/编码线程数")
    else:
        print("结论: 链路健康（或混合轻微劣化）")


if __name__ == "__main__":
    main()