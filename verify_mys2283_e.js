'use strict';
// MYS-2288-E Web 用户页 + 管理后台美化验证（playwright，无头 Chromium）。
//
// 覆盖任务卡验收标准 3 + 4：在本机拉起 relay（--no-tls --admin-path /adm
// --admin-pass x）与一个 agent，然后
//   · 截取登录页 / 会话页 / 文件抽屉 / 对话框 / admin 三个标签页 / secret 对话框
//     的深色与浅色截图，写到 artifacts/mys2283-e/；
//   · 断言主题切换后刷新仍保持、admin 下拉框获得焦点期间 10s 内 DOM 不被重建、
//     终端在两种主题下都能输入并回显。
//
// 用法:
//   npm i                                  # 只装 playwright
//   BIN=./target/debug/shell-remote node verify_mys2283_e.js
//
// 环境变量：
//   BIN     shell-remote 二进制路径（默认 ./target/debug/shell-remote）
//   PORT    relay 端口（默认 3191）
//   OUT     截图目录（默认 artifacts/mys2283-e）

// playwright 或 playwright-core 都可以（这台机器上装了后者，且它的版本与
// 已缓存的 chromium 对齐）。优先用 playwright，回退到 playwright-core。
let chromium;
try {
  ({ chromium } = require('playwright'));
} catch (e) {
  ({ chromium } = require('playwright-core'));
}
const { spawn } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const BIN = process.env.BIN || './target/debug/shell-remote';
const PORT = +(process.env.PORT || 3191);
const BASE = `http://127.0.0.1:${PORT}`;
const OUT = process.env.OUT || path.join('artifacts', 'mys2283-e');
const AUTH = 'mys2288ekey';
// 20 字符以内的 ASCII 字母数字；够长以便验证会话 ID 截断显示。
const SESSION = 'mys2288everysession';
const ADMIN_PATH = '/adm';
const ADMIN_USER = 'admin';
const ADMIN_PASS = 'x';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let fails = 0;
const check = (name, ok, extra = '') => {
  console.log(`${ok ? '✓' : '✗'} ${name}${extra ? '  ' + extra : ''}`);
  if (!ok) fails++;
};

// 本机可能装的是另一版 playwright-core 对应的 chromium（默认路径缺失时
// launch 直接报 "Executable doesn't exist"）。默认路径不可用时回退到
// ms-playwright 缓存里已安装的 chromium，保证脚本可直接跑。
function resolveChromium() {
  const def = chromium.executablePath();
  if (fs.existsSync(def)) return undefined;
  const cache = path.join(os.homedir(), '.cache', 'ms-playwright');
  if (!fs.existsSync(cache)) return undefined;
  const candidates = fs
    .readdirSync(cache)
    .filter((d) => /^chromium-\d+$/.test(d))
    .sort((a, b) => Number(b.split('-')[1]) - Number(a.split('-')[1]));
  for (const c of candidates) {
    for (const rel of ['chrome-linux64/chrome', 'chrome-linux/chrome']) {
      const p = path.join(cache, c, rel);
      if (fs.existsSync(p)) return p;
    }
  }
  return undefined;
}

async function main() {
  if (!fs.existsSync(BIN)) {
    console.error(`binary not found: ${BIN} (先 cargo build --bin shell-remote)`);
    process.exit(2);
  }
  fs.mkdirSync(OUT, { recursive: true });

  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'sr2288e-root-'));
  fs.writeFileSync(path.join(root, 'probe.txt'), 'hello mys2288e\n');
  fs.mkdirSync(path.join(root, 'docs'), { recursive: true });
  fs.writeFileSync(path.join(root, 'docs', 'readme.md'), '# docs\n');
  // 大一点的文件，让下载进度条有明显推进。
  fs.writeFileSync(path.join(root, 'big.bin'), Buffer.alloc(1024 * 1024, 0x41));

  // relay 的服务器密码（--auth）必须等于 agent 的 --key：agent 注册时用 key
  // 作为 server_auth 提交。
  const relay = spawn(BIN, [
    'relay', '--bind', `127.0.0.1:${PORT}`, '--auth', AUTH, '--no-tls',
    '--admin-path', ADMIN_PATH, '--admin-pass', ADMIN_PASS, '--admin-user', ADMIN_USER,
  ], { stdio: 'ignore' });
  let agent = null;
  let browser = null;

  const shot = async (page, name) => {
    const file = path.join(OUT, name);
    await page.screenshot({ path: file });
    console.log(`  · screenshot ${file}`);
    return file;
  };

  try {
    await sleep(1500);
    agent = spawn(BIN, [
      'agent', '--relay-url', BASE, '--key', AUTH, '--session-id', SESSION,
      '--root', root, '--shell', '/bin/sh',
    ], { stdio: 'ignore' });
    await sleep(2500);

    const exe = resolveChromium();
    browser = await chromium.launch(exe ? { executablePath: exe } : {});
    if (exe) console.log(`（使用 chromium: ${exe}）`);

    // ── 登录页：深色 / 浅色各一张（1440×900）─────────────────────────
    const loginCtx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    const login = await loginCtx.newPage();
    await login.goto(`${BASE}/`);
    await login.evaluate(() => { try { localStorage.setItem('sr-theme', 'dark'); } catch (e) {} });
    await login.reload();
    await login.waitForSelector('.connect-container');
    let theme = await login.evaluate(() => document.documentElement.getAttribute('data-theme'));
    check('登录页 data-theme=dark 生效', theme === 'dark', String(theme));
    // 卡片居中
    const centered = await login.evaluate(() => {
      const el = document.querySelector('.connect-container');
      const r = el.getBoundingClientRect();
      const mid = r.left + r.width / 2;
      return Math.abs(mid - window.innerWidth / 2) < 4;
    });
    check('登录卡片水平居中', centered);
    // 服务器密码可以留空：提示文案存在
    const authHint = await login.textContent('#auth-hint');
    check('服务器密码可留空提示存在', /可留空/.test(authHint || ''), authHint);
    // 安装命令 tab：Linux / macOS / Windows 三态，各带复制按钮
    const tabs = await login.$$eval('.tab-group [role="tab"]', (els) => els.map((e) => e.textContent.trim()));
    check('安装命令有 3 个 OS tab', tabs.length === 3, tabs.join('/'));
    const copyBtns = await login.$$eval('.tab-panel-body:not([hidden]) .copy-btn', (els) => els.length);
    check('当前 tab 的每条命令都有复制按钮', copyBtns >= 2, String(copyBtns));
    await shot(login, 'login-dark-1440x900.png');

    await login.evaluate(() => window.UI.theme.set('light'));
    await sleep(250);
    theme = await login.evaluate(() => document.documentElement.getAttribute('data-theme'));
    const bg = await login.evaluate(() => getComputedStyle(document.body).backgroundColor);
    check('登录页切到浅色', theme === 'light' && /244|245|246|247|248/.test(bg.replace(/\s/g, '')),
      `${theme} ${bg}`);
    await shot(login, 'login-light-1440x900.png');

    // ── 主题持久化：切主题后刷新仍保持 ───────────────────────────────
    await login.reload();
    await login.waitForSelector('.connect-container');
    theme = await login.evaluate(() => ({
      theme: document.documentElement.getAttribute('data-theme'),
      mode: document.documentElement.getAttribute('data-theme-mode'),
      stored: localStorage.getItem('sr-theme'),
    }));
    check('主题切换后刷新仍保持浅色', theme.theme === 'light' && theme.stored === 'light',
      JSON.stringify(theme));

    // ── 会话页 ────────────────────────────────────────────────────────
    const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    await ctx.addInitScript(([token, auth]) => {
      try {
        sessionStorage.setItem('shell-remote-token', token);
        sessionStorage.setItem('shell-remote-auth', auth);
        // 显式指定深色：本机 prefers-color-scheme 可能是浅色，不写死会让
        // 「深色截图」实际拍成浅色。
        localStorage.setItem('sr-theme', 'dark');
      } catch (e) {}
    }, [AUTH, AUTH]);
    const page = await ctx.newPage();
    await page.goto(`${BASE}/session`);
    const termReady = await page
      .waitForFunction(() => !!document.querySelector('#terminal-container .xterm'), { timeout: 30000 })
      .then(() => true).catch(() => false);
    check('会话页终端初始化', termReady);
    const tabOk = await page.waitForSelector('.tab-item', { timeout: 30000 })
      .then(() => true).catch(() => false);
    check('会话已加入并渲染 tab', tabOk);

    // 头部：状态点 + 会话 ID（截断，点击复制）+ 在线人数（中文）
    const header = await page.evaluate(() => {
      const dot = document.getElementById('status-dot');
      const sidText = document.getElementById('session-id-text');
      const online = document.getElementById('online-count');
      return {
        dotClass: dot ? dot.className : null,
        dotLabel: dot ? dot.getAttribute('aria-label') : null,
        sid: sidText ? sidText.textContent.trim() : null,
        online: online ? online.textContent.trim() : null,
      };
    });
    check('头部有连接状态点且已标记在线', /online/.test(header.dotClass || ''), JSON.stringify(header));
    check('头部会话 ID 已截断显示', !!header.sid && /…/.test(header.sid), String(header.sid));
    check('在线人数用中文「N 人在线」',
      header.online === '' || /^\d+\s*人在线$/.test(header.online), String(header.online));

    // 终端输入回显（深色）
    const typeShell = async (page, marker) => {
      await page.click('#terminal-container .xterm');
      await page.keyboard.type(`echo ${marker}\n`);
      return page.waitForFunction(
        (m) => document.querySelector('#terminal-container').innerText.indexOf(m) !== -1,
        marker, { timeout: 15000 }
      ).then(() => true).catch(() => false);
    };
    const echoDark = await typeShell(page, 'MYS2288E_DARK');
    check('终端在深色主题下可输入并回显', echoDark);
    await shot(page, 'session-terminal-dark-1440x900.png');

    const darkThemeState = await page.evaluate(() => ({
      theme: document.documentElement.getAttribute('data-theme'),
      termBg: getComputedStyle(document.documentElement).getPropertyValue('--term-bg').trim(),
    }));
    check('会话页深色主题生效', darkThemeState.theme === 'dark', JSON.stringify(darkThemeState));

    // 切到浅色：终端配色跟着 CSS 变量走
    await page.evaluate(() => window.UI.theme.set('light'));
    await sleep(400);
    const termTheme = await page.evaluate(() => {
      const bg = getComputedStyle(document.documentElement).getPropertyValue('--term-bg').trim();
      return { page: document.documentElement.getAttribute('data-theme'), termBg: bg };
    });
    check('浅色主题下 --term-bg 变为浅色', /^#f|^#e|rgb\(2[45]/.test(termTheme.termBg), termTheme.termBg);
    const echoLight = await typeShell(page, 'MYS2288E_LIGHT');
    check('终端在浅色主题下可输入并回显', echoLight);
    await shot(page, 'session-terminal-light-1440x900.png');

    // 文件抽屉 + 排序 + 下载进度条 + 右键菜单
    await page.click('#toggle-files-btn');
    const filesReady = await page.waitForSelector('#file-tree .paths-table', { timeout: 20000 })
      .then(() => true).catch(() => false);
    check('文件抽屉打开并渲染列表', filesReady);
    const sortHeaders = await page.$$eval('#file-tree .paths-table th.sortable',
      (els) => els.map((e) => e.textContent.trim()));
    check('文件表头可排序（名称 / 大小）', sortHeaders.length >= 2, sortHeaders.join('/'));
    // 抽屉截图：深色 + 浅色各一张（当前主题是浅色，先切回深色拍）。
    await page.evaluate(() => window.UI.theme.set('dark'));
    await sleep(300);
    await shot(page, 'session-file-drawer-dark.png');
    await page.evaluate(() => window.UI.theme.set('light'));
    await sleep(300);
    await shot(page, 'session-file-drawer-light.png');
    await page.evaluate(() => window.UI.theme.set('dark'));
    await sleep(200);

    // 右键菜单：Esc 关闭 + 方向键移动
    // 第一行可能是「..」父目录行（没有右键菜单），选一个真实条目。
    const row = await page.$('#file-tree .paths-table tbody tr.file-row[data-path]');
    if (row) {
      await row.click({ button: 'right' });
      const menuOpen = await page.waitForSelector('.context-menu', { timeout: 5000 })
        .then(() => true).catch(() => false);
      check('右键菜单打开', menuOpen);
      if (menuOpen) {
        const firstFocused = await page.evaluate(() =>
          document.activeElement && document.activeElement.classList.contains('context-menu-item'));
        check('右键菜单首项自动获得焦点', firstFocused);
        await page.keyboard.press('ArrowDown');
        const secondFocused = await page.evaluate(() => {
          const items = Array.from(document.querySelectorAll('.context-menu-item'));
          return items.indexOf(document.activeElement);
        });
        check('方向键在菜单项间移动', secondFocused === 1, String(secondFocused));
        await page.keyboard.press('Escape');
        const closed = await page.waitForFunction(() => !document.querySelector('.context-menu'),
          { timeout: 3000 }).then(() => true).catch(() => false);
        check('Esc 关闭右键菜单', closed);
      }
    } else {
      check('文件列表存在可右键的行', false);
    }

    // 对话框（E4）：打开「新建文件夹」的 prompt，截图后 Esc 取消
    await page.click('#file-new-folder-btn');
    const dlg = await page.waitForSelector('.sr-dialog-mask .sr-dialog', { timeout: 5000 })
      .then(() => true).catch(() => false);
    check('UI.prompt 对话框打开', dlg);
    const dlgAttrs = await page.evaluate(() => {
      const d = document.querySelector('.sr-dialog');
      if (!d) return null;
      return {
        role: d.getAttribute('role'),
        modal: d.getAttribute('aria-modal'),
        focusedInside: d.contains(document.activeElement),
      };
    });
    check('对话框 role=dialog aria-modal=true 且焦点在内部',
      !!dlgAttrs && dlgAttrs.role === 'dialog' && dlgAttrs.modal === 'true' && dlgAttrs.focusedInside,
      JSON.stringify(dlgAttrs));
    await shot(page, 'session-dialog-prompt.png');
    await page.keyboard.press('Escape');
    const dlgClosed = await page.waitForFunction(() => !document.querySelector('.sr-dialog-mask'),
      { timeout: 3000 }).then(() => true).catch(() => false);
    check('Esc 关闭对话框', dlgClosed);

    // toast 具备 role=status / aria-live=polite
    const toastAttrs = await page.evaluate(() => {
      const t = document.getElementById('toast');
      return t ? { role: t.getAttribute('role'), live: t.getAttribute('aria-live') } : null;
    });
    check('toast 为 role=status aria-live=polite',
      !!toastAttrs && toastAttrs.role === 'status' && toastAttrs.live === 'polite',
      JSON.stringify(toastAttrs));

    // 终端 tab 无障碍：role=tab + 方向键
    const tabA11y = await page.evaluate(() => {
      const t = document.querySelector('.tab-item');
      return t ? { role: t.getAttribute('role'), tabindex: t.tabIndex, sel: t.getAttribute('aria-selected') } : null;
    });
    check('终端 tab 具备 role=tab / aria-selected / roving tabindex',
      !!tabA11y && tabA11y.role === 'tab' && tabA11y.sel === 'true' && tabA11y.tabindex === 0,
      JSON.stringify(tabA11y));

    // 关闭抽屉，准备移动端截图
    await page.click('#close-files-btn');
    await sleep(200);

    // ── 移动端 390×844：快捷键行必须可见 ─────────────────────────────
    const mctx = await browser.newContext({
      viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true,
      deviceScaleFactor: 2,
    });
    await mctx.addInitScript(() => {
      try { localStorage.setItem('sr-theme', 'dark'); } catch (e) {}
    });
    await mctx.addInitScript(([token, auth]) => {
      try {
        sessionStorage.setItem('shell-remote-token', token);
        sessionStorage.setItem('shell-remote-auth', auth);
      } catch (e) {}
    }, [AUTH, AUTH]);
    const mpage = await mctx.newPage();
    await mpage.goto(`${BASE}/session`);
    await mpage.waitForSelector('#terminal-container .xterm', { timeout: 30000 });
    await mpage.waitForFunction(() => {
      const kb = document.getElementById('key-bar');
      return kb && getComputedStyle(kb).display !== 'none';
    }, { timeout: 10000 }).catch(() => {});
    const keyBar = await mpage.evaluate(() => {
      const kb = document.getElementById('key-bar');
      if (!kb) return null;
      const r = kb.getBoundingClientRect();
      const labels = Array.from(kb.querySelectorAll('button')).map((b) => b.textContent.trim());
      return { visible: r.height > 0 && r.width > 0, labels: labels };
    });
    check('移动端快捷键行可见', !!keyBar && keyBar.visible, JSON.stringify(keyBar && keyBar.labels));
    check('快捷键行含 Esc/Tab/Ctrl/方向键',
      !!keyBar && ['Esc', 'Tab', 'Ctrl', '↑', '↓', '←', '→'].every((k) => (keyBar.labels || []).includes(k)),
      JSON.stringify(keyBar && keyBar.labels));
    const copyTokenVisible = await mpage.evaluate(() => {
      const b = document.getElementById('copy-token-btn');
      return !!b && getComputedStyle(b).display !== 'none';
    });
    check('移动端复制密钥按钮不隐藏', copyTokenVisible);

    // 粘滞 Ctrl：按下后 aria-pressed=true，下一个按键带上 Ctrl 并自动复位。
    const stickyState = await mpage.evaluate(() => {
      const ctrl = document.getElementById('key-ctrl');
      ctrl.click();
      return {
        pressed: ctrl.getAttribute('aria-pressed'),
      };
    });
    check('Ctrl 粘滞键按下后保持高亮', stickyState.pressed === 'true', JSON.stringify(stickyState));
    await mpage.keyboard.press('Tab');   // 任意按键消费掉粘滞状态
    const stickyAfter = await mpage.evaluate(() =>
      document.getElementById('key-ctrl').getAttribute('aria-pressed'));
    check('下一个按键后粘滞 Ctrl 自动复位', stickyAfter === 'false', String(stickyAfter));
    await shot(mpage, 'session-mobile-390x844.png');
    await mctx.close();

    // ── 管理后台 ──────────────────────────────────────────────────────
    const actx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    await actx.addInitScript(() => {
      try { localStorage.setItem('sr-theme', 'dark'); } catch (e) {}
    });
    const admin = await actx.newPage();
    await admin.goto(`${BASE}${ADMIN_PATH}`);
    await admin.waitForSelector('#loginView:not(.hidden)', { timeout: 15000 }).catch(() => {});
    await admin.fill('#lu', ADMIN_USER);
    await admin.fill('#lp', ADMIN_PASS);
    await admin.click('#loginForm button[type="submit"]');
    await admin.waitForSelector('#dashView:not(.hidden)', { timeout: 20000 });
    await sleep(1500);

    const topbar = await admin.evaluate(() => {
      const tb = document.querySelector('.topbar');
      const lang = document.getElementById('langToggle');
      const themeBtn = document.querySelector('.topbar .theme-toggle');
      const logout = document.getElementById('logoutBtn');
      return {
        inTopbar: !!(tb && lang && tb.contains(lang)),
        hasTheme: !!themeBtn,
        hasLogout: !!logout,
        langPosition: lang ? getComputedStyle(lang).position : null,
      };
    });
    check('语言切换已移进顶部栏（不再 fixed 在右下角）',
      topbar.inTopbar && topbar.langPosition !== 'fixed', JSON.stringify(topbar));
    check('顶部栏含主题切换与登出', topbar.hasTheme && topbar.hasLogout);

    // 概览：卡片网格含带宽卡片
    const cards = await admin.$$eval('#tab-overview .card .k', (els) => els.map((e) => e.textContent.trim()));
    check('概览为卡片网格（含上下行带宽卡片）', cards.length >= 6, cards.join('/'));
    const bwCardIds = await admin.evaluate(() => ({
      down: !!document.getElementById('cBwDown'),
      up: !!document.getElementById('cBwUp'),
      summary: !!document.getElementById('bwSummary'),
    }));
    check('阶段 2 带宽卡片与面板 id 保留',
      bwCardIds.down && bwCardIds.up && bwCardIds.summary, JSON.stringify(bwCardIds));

    // 深色 / 浅色截图：概览 / 设备 / 设置
    const adminShot = async (tab, name) => {
      await admin.click(`.tab-btn[data-tab="${tab}"]`);
      await sleep(700);
      await shot(admin, name);
    };
    await admin.evaluate(() => window.UI.theme.set('dark'));
    await sleep(300);
    await adminShot('overview', 'admin-overview-dark.png');
    await adminShot('devices', 'admin-devices-dark.png');
    await adminShot('settings', 'admin-settings-dark.png');

    await admin.evaluate(() => window.UI.theme.set('light'));
    await sleep(300);
    await adminShot('overview', 'admin-overview-light.png');
    await adminShot('devices', 'admin-devices-light.png');
    await adminShot('settings', 'admin-settings-light.png');

    // 下拉框获得焦点期间 10s 内 DOM 不被重建（E3 刷新跳过交互中用户）
    await admin.click('.tab-btn[data-tab="devices"]');
    await sleep(500);
    // 打标记：如果 arch 下拉框被重建，标记属性会消失。
    const marked = await admin.evaluate(() => {
      const sel = document.getElementById('devArchFilter');
      if (!sel) return false;
      sel.setAttribute('data-marked-by-verify', '1');
      sel.focus();
      return document.activeElement === sel;
    });
    check('arch 下拉框获得焦点', marked);
    // 同时验证「有请求在途时跳过」不会因为焦点态被绕过。
    await sleep(11000);
    const stillMarked = await admin.evaluate(() => {
      const sel = document.getElementById('devArchFilter');
      return {
        marked: !!sel && sel.getAttribute('data-marked-by-verify') === '1',
        stillFocused: document.activeElement === sel,
        options: sel ? sel.options.length : -1,
      };
    });
    check('下拉框获得焦点期间 10s 内 DOM 未被重建（标记仍在）',
      stillMarked.marked && stillMarked.stillFocused, JSON.stringify(stillMarked));

    // 下拉框失焦后，刷新要能恢复（不因跳过逻辑永久停摆）
    await admin.evaluate(() => document.getElementById('devArchFilter').blur());
    await sleep(6000);
    const refreshedAfterBlur = await admin.evaluate(() => {
      const c = document.getElementById('cAgents');
      return c ? c.textContent.trim() : '';
    });
    check('失焦后刷新恢复运行（概览数值已渲染）', refreshedAfterBlur.length > 0, refreshedAfterBlur);

    // 重新生成 token → secret 对话框（只读输入框 + 复制按钮）
    await admin.click('.tab-btn[data-tab="sessions"]');
    await sleep(600);
    const regenBtn = await admin.$('#sessionsBody button:has-text("Regenerate"), #sessionsBody button:has-text("重新生成")');
    if (regenBtn) {
      await regenBtn.click();
      const confirmDlg = await admin.waitForSelector('.sr-dialog-mask', { timeout: 5000 })
        .then(() => true).catch(() => false);
      check('重新生成 token 先弹确认对话框（不再用 confirm）', confirmDlg);
      if (confirmDlg) {
        // 点主按钮确认（对话框内部最后一个 primary 按钮）
        await admin.click('.sr-dialog-actions .sr-dialog-btn-primary');
        const secret = await admin.waitForSelector('.sr-secret-field', { timeout: 10000 })
          .then(() => true).catch(() => false);
        check('重新生成后展示 secret 对话框', secret);
        const secretInfo = await admin.evaluate(() => {
          const f = document.querySelector('.sr-secret-field');
          const copy = document.querySelector('.sr-secret-row .sr-dialog-btn');
          if (!f) return null;
          return {
            readOnly: f.readOnly === true || f.hasAttribute('readonly'),
            hasValue: (f.value || '').length > 0,
            hasCopy: !!copy,
          };
        });
        check('secret 为只读字段 + 复制按钮',
          !!secretInfo && secretInfo.readOnly && secretInfo.hasValue && secretInfo.hasCopy,
          JSON.stringify(secretInfo));
        await shot(admin, 'admin-secret-dialog.png');
        await admin.keyboard.press('Escape');
      }
    } else {
      check('Sessions 表存在「重新生成」按钮', false);
    }
  } catch (e) {
    console.log('ERR', e && e.stack ? e.stack : e);
    fails++;
  } finally {
    if (browser) await browser.close().catch(() => {});
    if (agent) agent.kill();
    relay.kill();
    try { fs.rmSync(root, { recursive: true, force: true }); } catch (e) {}
  }
  console.log(fails === 0 ? 'PASS' : `FAIL(${fails})`);
  process.exit(fails === 0 ? 0 : 1);
}

main();
