// v0.38.0 QoS 修复冒烟：真浏览器连本地 relay + Xvfb 动态桌面，
// 采样指标面板 25s：端到端延时 / 渲染帧率 / 当前码率。
// 通过标准：动态内容下 e2e 读数 ≈ 管线延时（远小于 600ms，不触发降级），
// fps 保持在配置上限附近（不塌到 1）。
'use strict';
const { chromium } = require('playwright');

const BASE = 'http://127.0.0.1:3100';
const TOKEN = 'smokekey';
const RUN_MS = 25000;

(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  await page.goto(BASE + '/');
  await page.evaluate((t) => sessionStorage.setItem('shell-remote-token', t), TOKEN);
  await page.goto(BASE + '/session');

  await page.waitForFunction(() => {
    const b = document.getElementById('toggle-desktop-btn');
    return b && !b.disabled;
  }, { timeout: 15000 });
  console.log('agent 会话已连接，桌面按钮可用');
  await page.click('#toggle-desktop-btn');
  await page.waitForFunction(() => {
    const c = document.getElementById('desktop-container');
    return c && !c.classList.contains('hidden');
  }, { timeout: 15000 });
  console.log('桌面视图已打开，等待首帧…');
  await page.waitForFunction(() => {
    const el = document.getElementById('desktop-status');
    return el && el.textContent.includes('已连接');
  }, { timeout: 20000 }).catch(() => console.log('状态提示未出现，继续采样'));

  await page.click('#desktop-metrics-btn').catch(() => {});
  await page.waitForTimeout(2000);

  const readings = [];
  const start = Date.now();
  while (Date.now() - start < RUN_MS) {
    const r = await page.evaluate(() => {
      const t = (id) => { const e = document.getElementById(id); return e ? e.textContent.trim() : null; };
      return { lag: t('metric-lag'), fps: t('metric-fps'), kbps: t('metric-bitrate'),
               res: t('metric-res'), codec: t('metric-encoder'), decoder: t('metric-decoder') };
    });
    readings.push(r);
    await page.waitForTimeout(1000);
  }

  const lag = readings.filter(r => r.lag && r.lag !== '-').map(r => parseFloat(r.lag));
  const fps = readings.filter(r => r.fps).map(r => parseInt(r.fps, 10));
  const kbps = readings.filter(r => r.kbps).map(r => parseFloat(r.kbps));
  const avg = (a) => a.length ? a.reduce((x, y) => x + y, 0) / a.length : NaN;
  const min = (a) => a.length ? Math.min(...a) : NaN;
  const max = (a) => a.length ? Math.max(...a) : NaN;
  console.log('── 采样结果 ──');
  console.log(`lag  中位数均值 ${avg(lag).toFixed(0)}ms  范围 [${min(lag).toFixed(0)}, ${max(lag).toFixed(0)}]  (样本 ${lag.length})`);
  console.log(`fps  中位数均值 ${avg(fps).toFixed(1)}    范围 [${min(fps)}, ${max(fps)}]`);
  console.log(`kbps 中位数均值 ${avg(kbps).toFixed(0)}  范围 [${min(kbps).toFixed(0)}, ${max(kbps).toFixed(0)}]`);
  const last = readings[readings.length - 1];
  console.log(`最后样本: lag=${last.lag} fps=${last.fps} kbps=${last.kbps} res=${last.res} codec=${last.codec} decoder=${last.decoder}`);
  await browser.close();

  const lagMed = avg(lag);
  const fpsMed = avg(fps);
  const okLag = !Number.isNaN(lagMed) && lagMed < 600;
  const okFps = !Number.isNaN(fpsMed) && fpsMed >= 10;
  console.log(okLag && okFps ? 'PASS: 动态内容 e2e 未触发降档、fps 未塌底' : 'FAIL');
  process.exit(okLag && okFps ? 0 : 1);
})();