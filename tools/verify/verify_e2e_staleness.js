// 验证 e2e 测量修复：fps 低时"帧陈旧度"不再污染延时读数（MYS-886 死锁根因）。
//
// 模拟"动态页面但编码只输出 1fps"的真实崩溃现场（用户实拍：端到端 811ms、
// 渲染帧率 1）。对比旧逻辑（tick 时刻 now−旧采集）与新逻辑（解码到达时刻
// 测定即时管线延时），断言新逻辑在 1fps 下读数 ≈ 真实管线延时（如 50ms），
// 旧逻辑会被陈旧度抬到数百 ms 并喂给 QoS 压 fps 形成自锁。
'use strict';

const PIPELINE_MS = 50;   // 采集→解码的真实管线延时代入
const FPS = 1;            // 编码被 QoS 压到 1fps 的最坏场景
const FRAME_MS = 1000 / FPS;
const CLOCK_OFFSET = 0;   // 双端已校准到同一时基

// ── 帧事件流：每 FRAME_MS 解码一帧，srtc=采集时刻，decodeAt=采集+PIPELINE
const frames = [];
for (let t = 1000; t <= 12000; t += FRAME_MS) {
  frames.push({ captureMs: t, decodeAt: t + PIPELINE_MS });
}

// 旧逻辑：tick 时刻用"最近解码帧的采集时刻"算 e2e（含帧陈旧度）
const oldLastCaptureByTime = [];
for (const f of frames) oldLastCaptureByTime.push({ t: f.decodeAt, cap: f.captureMs });
function oldE2eAt(tickAt) {
  const recent = oldLastCaptureByTime.filter(s => s.t <= tickAt);
  if (!recent.length) return undefined;
  return Math.max(0, tickAt + CLOCK_OFFSET - recent[recent.length - 1].cap);
}

// 新逻辑：解码到达时刻测定即时管线延时，tick 只转发最近样本
const newSamples = [];
for (const f of frames) {
  newSamples.push({ t: f.decodeAt, e2e: Math.max(0, f.decodeAt + CLOCK_OFFSET - f.captureMs) });
}
function newE2eAt(tickAt) {
  const recent = newSamples.filter(s => s.t <= tickAt && tickAt - s.t <= 1500);
  return recent.length ? recent[recent.length - 1].e2e : undefined;
}

// 指标面板：每秒 tick 一次
const oldReadings = [], newReadings = [];
for (let tick = 2000; tick <= 13000; tick += 1000) {
  const o = oldE2eAt(tick);
  const n = newE2eAt(tick);
  if (o !== undefined) oldReadings.push(o);
  if (n !== undefined) newReadings.push(n);
}

const oldAvg = oldReadings.reduce((a, b) => a + b, 0) / oldReadings.length;
const newAvg = newReadings.reduce((a, b) => a + b, 0) / newReadings.length;

console.log(`pipeline=${PIPELINE_MS}ms fps=${FPS} 采样 12s`);
console.log(`旧逻辑（tick 算 now−旧采集）: 读数均值 ${oldAvg.toFixed(0)}ms   → ${oldAvg > 600 ? 'QoS 判差网、压 fps → 自锁' : 'OK'}`);
console.log(`新逻辑（解码到达时刻测定） : 读数均值 ${newAvg.toFixed(0)}ms   → ${Math.abs(newAvg - PIPELINE_MS) <= 40 ? '≈ 真实管线延时，反馈正确' : '偏离!'}`);

if (Math.abs(newAvg - PIPELINE_MS) > 40) { console.error('FAIL'); process.exit(1); }
if (oldAvg <= 600) { console.error('FAIL: 旧逻辑应读出高延时以复现故障'); process.exit(1); }
console.log('PASS');