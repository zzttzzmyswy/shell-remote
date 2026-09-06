#!/usr/bin/env python3
"""timeline_merge.py — 统一时间线聚合（R4 丙 统一时间线遥测 最小可验证子集）。

把 agent 日志、relay 日志、admin KPI 样本按**墙钟时间戳**合并成一条统一
时间线（标来源进程），弱网问题排查时可在同一时间轴上看三端因果：
  - agent 日志：tracing ISO8601（tracing::info/warn 的 QoS 快照、capture、
    congested 等）
  - relay 日志：tracing ISO8601（desktop stream 生命周期、背压回传等）
  - KPI 样本：admin `/api/session/kpi/:sid` 导出的 JSON（samples[].at_unix_ms）

用法:
  python3 tools/timeline_merge.py --agent agent.log [--relay relay.log] [--kpi kpi.json] [--tail 200]

输出: t_ms | source | 事件行（按时间升序）；`--tail` 限制行数（默认全量）。
"""
import argparse
import datetime
import json
import re
import sys

def parse_iso_to_ms(s: str) -> int | None:
    """ISO8601（带 Z/±HH:MM 或 naive）→ unix ms。解析失败返回 None。"""
    s = s.strip()
    try:
        if s.endswith("Z"):
            dt = datetime.datetime.fromisoformat(s[:-1] + "+00:00")
        else:
            dt = datetime.datetime.fromisoformat(s)
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=datetime.timezone.utc)
        return int(dt.timestamp() * 1000)
    except ValueError:
        return None


# tracing 日志行: "2026-09-06T05:38:28.075910Z  INFO shell_remote::agent: msg"
def parse_log(path: str, source: str, out: list):
    try:
        with open(path, "r", errors="replace") as fh:
            for line in fh:
                # [ts, LEVEL, "module: msg"]（模块可能含 `::`）
                parts = line.split(None, 2)
                if len(parts) < 3:
                    continue
                ts = parse_iso_to_ms(parts[0])
                if ts is None:
                    continue
                rest = parts[2]
                # 取最后一个 ": " 后的消息（tracing "module: msg"，module 可含冒号）
                idx = rest.rfind(": ")
                msg = rest[idx + 2:].strip() if idx >= 0 else rest.strip()
                out.append((ts, source, msg))
    except OSError as e:
        print(f"warning: 无法读 {path}: {e}", file=sys.stderr)


def parse_kpi(path: str, source: str, out: list):
    try:
        with open(path, "r", errors="replace") as fh:
            data = json.load(fh)
        for s in data.get("samples", []):
            ts = s.get("at_unix_ms")
            if not ts:
                continue
            fps = s.get("fps", "?")
            active = s.get("active", "?")
            bp = s.get("bp_count", "?")
            out.append((int(ts), source, f"KPI fps={fps} active={active} bp={bp} rss_kb={s.get('rss_kb','?')}"))
    except OSError as e:
        print(f"warning: 无法读 {path}: {e}", file=sys.stderr)
    except (ValueError, json.JSONDecodeError) as e:
        print(f"warning: {path} 非合法 JSON: {e}", file=sys.stderr)


def main():
    ap = argparse.ArgumentParser(description="统一时间线聚合（agent/relay/KPI）")
    ap.add_argument("--agent", help="agent 日志路径")
    ap.add_argument("--relay", help="relay 日志路径")
    ap.add_argument("--kpi", help="admin KPI JSON 导出路径")
    ap.add_argument("--tail", type=int, default=0, help="只输出最后 N 行（0=全量）")
    args = ap.parse_args()

    events = []
    if args.agent:
        parse_log(args.agent, "agent", events)
    if args.relay:
        parse_log(args.relay, "relay", events)
    if args.kpi:
        parse_kpi(args.kpi, "kpi", events)

    if not events:
        print("无可用事件（至少需要 --agent 或 --relay 或 --kpi 之一）", file=sys.stderr)
        return 1

    events.sort(key=lambda e: e[0])
    if args.tail > 0:
        events = events[-args.tail:]

    for ts, source, ev in events:
        print(f"{ts:>14} | {source:<6} | {ev}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
