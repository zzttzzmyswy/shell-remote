(function() {
    const token = sessionStorage.getItem('shell-remote-token');

    if (!token) {
        window.location.href = '/';
        return;
    }

    let activeTabId = null;
    let pendingTabSwitch = null;
    let onlineUsers = 0;
    let tabs = [];

    // Desktop view state. 桌面默认关闭：仅在点击"桌面"按钮后才开启。
    let desktopEnabled = false;   // agent 能力可用
    let desktopActive = false;    // 当前处于桌面视图
    let desktopStarting = false;  // 已发 desktop:start 等待回复
    const term = new TerminalManager('terminal-container');
    const files = new FileManager('file-tree');
    const desktopView = new DesktopView();

    const onlineCountEl = document.getElementById('online-count');
    const sessionNameEl = document.getElementById('session-name');
    const toggleDesktopBtn = document.getElementById('toggle-desktop-btn');
    const terminalContainer = document.getElementById('terminal-container');
    const desktopContainer = document.getElementById('desktop-container');
    const tabBarEl = document.getElementById('tab-bar');
    const disconnectOverlay = document.getElementById('disconnect-overlay');
    const disconnectText = document.getElementById('disconnect-text');
    const toast = document.getElementById('toast');
    const fileDrawer = document.getElementById('file-drawer');
    const fileResizer = document.getElementById('file-resizer');
    const tabListEl = document.getElementById('tab-list');
    const tabNewBtn = document.getElementById('tab-new-btn');

    function showToast(msg, cls) {
        toast.textContent = msg;
        toast.className = 'toast ' + cls;
        setTimeout(() => { toast.classList.add('hidden'); }, 3000);
    }

    // Join-ack watchdog: once connected, the agent must answer quickly
    // (session:users / session:tab_list / any terminal output). If nothing
    // arrives, the join was silently lost somewhere — surface it and reconnect
    // instead of leaving a permanently blank terminal (the classic
    // "registered + token ok, but web stays empty and the agent logs nothing").
    let joinAckTimer = null;
    const JOIN_ACK_TIMEOUT = 8000;
    function armJoinWatchdog() {
        clearJoinWatchdog();
        joinAckTimer = setTimeout(() => {
            joinAckTimer = null;
            showToast('设备无响应，正在自动重连…', 'error');
            window.shellRemote.reconnect();
        }, JOIN_ACK_TIMEOUT);
    }
    function clearJoinWatchdog() {
        if (joinAckTimer) {
            clearTimeout(joinAckTimer);
            joinAckTimer = null;
        }
    }

    function updateOnlineCount() {
        onlineCountEl.textContent = onlineUsers + ' online';
    }

    // ── Desktop view control ──────────────────────────────────

    function showDesktopView() {
        desktopActive = true;
        terminalContainer.classList.add('hidden');
        desktopContainer.classList.remove('hidden');
        tabBarEl.classList.add('hidden');
        toggleDesktopBtn.textContent = '终端';
        // 编码/码率控件仅桌面模式显示（MYS-886）。
        const bar = document.getElementById('desktop-ctrl-bar');
        if (bar) bar.classList.remove('hidden');
    }

    function showTerminalView() {
        desktopActive = false;
        terminalContainer.classList.remove('hidden');
        desktopContainer.classList.add('hidden');
        tabBarEl.classList.remove('hidden');
        toggleDesktopBtn.textContent = '桌面';
        const bar = document.getElementById('desktop-ctrl-bar');
        if (bar) bar.classList.add('hidden');
        setTimeout(() => { term.resize(); }, 50);
    }

    function setDesktopEnabled(available) {
        desktopEnabled = !!available;
        // 无桌面捕获能力（无头/未启用）时：桌面按钮与相关控件直接不显示。
        if (!desktopEnabled) {
            toggleDesktopBtn.classList.add('hidden');
        } else {
            toggleDesktopBtn.classList.remove('hidden');
        }
        toggleDesktopBtn.disabled = !desktopEnabled;
        if (!desktopEnabled) {
            toggleDesktopBtn.title = '设备不支持桌面共享（未启用桌面捕获）';
        } else {
            toggleDesktopBtn.title = '打开桌面画面（默认关闭）';
        }
    }

    // 编码方案切换：发送 desktop:codec，agent 重建桌面流。
    const codecSelect = document.getElementById('desktop-codec-select');
    if (codecSelect) {
        codecSelect.addEventListener('change', function() {
            const codec = this.value;
            if (!desktopEnabled || !window.shellRemote) return;
            // 切换中标志：agent stop→start 重建流期间, desktop:stopped 不
            // 触发退出桌面视图, 等 desktop:started 到来后自动重连新流。
            window.__codecSwitchPending = true;
            window.shellRemote.send('desktop:codec', { codec: codec });
            showToast('切换编码为 ' + codec.toUpperCase() + '…', '');
        });
    }

    // 码率档切换（rustdesk 三档 + 自定义）：发送 desktop:quality，agent
    // 重建桌面流应用新档/自定义码率硬顶。
    const qualitySelect = document.getElementById('desktop-quality-select');
    if (qualitySelect) {
        qualitySelect.addEventListener('change', function() {
            let quality = this.value;
            let custom = 0;
            if (quality === 'custom') {
                const input = window.prompt('自定义码率 (kbps)，例如 1000', '1000');
                if (input === null) { this.value = 'balanced'; return; }
                custom = parseInt(input, 10);
                if (!(custom > 0)) { this.value = 'balanced'; return; }
                quality = 'balanced'; // 自定义 = balanced 档 + 码率硬顶
            }
            if (!desktopEnabled || !window.shellRemote) return;
            window.__codecSwitchPending = true;
            window.shellRemote.send('desktop:quality', { quality: quality, bitrate_kbps: custom });
            showToast('切换码率档…', '');
        });
    }

    // 灰度模式开关（弱网省带宽）：发送 desktop:gray，agent 编码前把色度
    // 置中性，即时生效不重建流。
    const grayToggle = document.getElementById('desktop-gray-toggle');
    if (grayToggle) {
        grayToggle.addEventListener('change', function() {
            if (!desktopEnabled || !window.shellRemote) return;
            window.shellRemote.send('desktop:gray', { enabled: this.checked });
            showToast(this.checked ? '已开启灰度模式（省带宽）' : '已关闭灰度模式', '');
        });
    }

    toggleDesktopBtn.addEventListener('click', function() {
        if (!desktopEnabled) return;
        if (desktopActive) {
            // 切回终端：停流
            desktopView.disconnect();
            window.shellRemote.send('desktop:stop', {});
            desktopStarting = false;
            showTerminalView();
            return;
        }
        if (desktopStarting) return;
        desktopStarting = true;
        toggleDesktopBtn.disabled = true;
        toggleDesktopBtn.textContent = '连接中…';
        window.shellRemote.send('desktop:start', {});
    });

    function renderTabs() {
        tabListEl.innerHTML = '';
        tabs.forEach(t => {
            const el = document.createElement('div');
            el.className = 'tab-item' + (t.tab_id === activeTabId ? ' active' : '');
            el.innerHTML = '<span>' + (t.title || 'Shell') + '</span>';
            if (tabs.length > 1) {
                const close = document.createElement('span');
                close.className = 'tab-close';
                close.textContent = '\u00d7';
                close.onclick = (e) => {
                    e.stopPropagation();
                    window.shellRemote.send('session:tab_close', { tab_id: t.tab_id });
                };
                el.appendChild(close);
            }
            el.onclick = () => {
                if (t.tab_id !== activeTabId) {
                    pendingTabSwitch = t.tab_id;
                    window.shellRemote.send('session:tab_switch', { tab_id: t.tab_id, _user_id: window.shellRemote.getUserId() });
                }
            };
            tabListEl.appendChild(el);
        });
    }

    // ── ShellRemote event handlers ─────────────────────────────────────

    window.shellRemote.on('connected', function(msg) {
        sessionNameEl.textContent = '已连接';
        disconnectOverlay.classList.add('hidden');
        armJoinWatchdog();
        term.focus();
        term.onResize((cols, rows) => {
            window.shellRemote.send('terminal:resize', {
                cols: cols, rows: rows, tab_id: activeTabId
            });
        });
        term.onInput((data) => {
            const bytes = new TextEncoder().encode(data);
            const b64 = btoa(String.fromCharCode(...bytes));
            window.shellRemote.send('terminal:input', {
                data: b64, tab_id: activeTabId
            });
        });
    });

    window.shellRemote.on('terminal:output', function(msg) {
        clearJoinWatchdog();
        try {
            const binaryStr = atob(msg.payload.data);
            const bytes = Uint8Array.from(binaryStr, c => c.charCodeAt(0));
            const decoded = new TextDecoder().decode(bytes);
            if (msg.payload.tab_id === activeTabId) {
                term.write(decoded);
            }
        } catch (e) {
            console.error('Failed to decode terminal output', e);
        }
    });

    window.shellRemote.on('session:tab_list', function(msg) {
        clearJoinWatchdog();
        tabs = msg.payload.tabs || [];
        if (!activeTabId && tabs.length > 0) {
            activeTabId = tabs[0].tab_id;
        }
        renderTabs();
        if (activeTabId) {
            setTimeout(() => {
                term.resize();
                window.shellRemote.send('terminal:resize', {
                    cols: term.getCols(), rows: term.getRows(), tab_id: activeTabId
                });
            }, 100);
        }
    });

    window.shellRemote.on('session:tab_switched', function(msg) {
        if (pendingTabSwitch === null) return;
        if (pendingTabSwitch !== '__new__' && pendingTabSwitch !== msg.payload.tab_id) return;
        pendingTabSwitch = null;
        activeTabId = msg.payload.tab_id;
        term.clear();
        renderTabs();
        setTimeout(() => {
            term.resize();
            window.shellRemote.send('terminal:resize', {
                cols: term.getCols(), rows: term.getRows(), tab_id: activeTabId
            });
        }, 100);
    });

    window.shellRemote.on('session:users', function(msg) {
        clearJoinWatchdog();
        onlineUsers = msg.payload.count || 0;
        updateOnlineCount();
    });

    window.shellRemote.on('desktop:capabilities', function(msg) {
        clearJoinWatchdog();
        setDesktopEnabled(msg.payload && msg.payload.available);
        // 按 agent 声明的可用编码过滤切换选项（codecs: ["av1","vp9","h264"]）。
        const codecs = (msg.payload && msg.payload.codecs) || [];
        const sel = document.getElementById('desktop-codec-select');
        if (sel && codecs.length) {
            const cur = sel.value;
            sel.innerHTML = '';
            for (const c of ['av1', 'vp9', 'vp8', 'h264']) {
                if (codecs.indexOf(c) >= 0) {
                    const o = document.createElement('option');
                    o.value = c;
                    o.textContent = c.toUpperCase();
                    sel.appendChild(o);
                }
            }
            sel.value = (codecs.indexOf(cur) >= 0) ? cur : (codecs[0] || 'av1');
        }
        if (msg.payload && msg.payload.running && !desktopActive && !desktopStarting) {
            // 其它浏览器已开启桌面：本地直接进入观看
            showDesktopView();
            desktopView.connect();
        }
    });

    window.shellRemote.on('desktop:started', function(msg) {
        desktopStarting = false;
        toggleDesktopBtn.disabled = !desktopEnabled;
        // 编码热切换完成：清除 pending 标志。
        window.__codecSwitchPending = false;
        if (msg.payload && msg.payload.error) {
            showToast('桌面启动失败: ' + msg.payload.error, 'error');
            showTerminalView();
            return;
        }
        // 实际生效的捕获后端（auto 解析后可能与请求不同，例如 dxgi 回退 gdi）。
        window._srDesktopInfo = window._srDesktopInfo || {};
        window._srDesktopInfo.backend = (msg.payload && msg.payload.backend) || null;
        // 实际生效的编码方案（可能因 fallback 与 select 默认值不同）同步 UI。
        if (msg.payload && msg.payload.codec) {
            const cs = document.getElementById('desktop-codec-select');
            if (cs && Array.from(cs.options).some(function(o) { return o.value === msg.payload.codec; })) {
                cs.value = msg.payload.codec;
            }
        }
        showDesktopView();
        desktopView.connect();
    });

    // agent 上行链路方式（ws | http）变化时上报，指标面板展示。
    window.shellRemote.on('desktop:uplink', function(msg) {
        window._srDesktopInfo = window._srDesktopInfo || {};
        window._srDesktopInfo.uplink = (msg.payload && msg.payload.uplink) || null;
        if (desktopView && desktopView._uplinkMode !== undefined) {
            desktopView._uplinkMode = window._srDesktopInfo.uplink;
        }
    });

    // ── 剪贴板同步（纯文本）──────────────────────────────────
    // → 远端：读文本框（为空则读本机剪贴板）发给 agent 设置远端剪贴板。
    // 远端 → 本机：请求 agent 读远端剪贴板，回包写入文本框并尝试写本机
    // 剪贴板（写剪贴板需要用户手势——按钮点击就是手势）。
    const clipBtn = document.getElementById('desktop-clip-btn');
    const clipPanel = document.getElementById('desktop-clip-panel');
    const clipText = document.getElementById('desktop-clip-text');
    if (clipBtn && clipPanel) {
        clipBtn.addEventListener('click', function() {
            clipPanel.classList.toggle('hidden');
        });
        document.getElementById('clip-to-remote-btn').addEventListener('click', async function() {
            let text = clipText.value;
            if (!text) {
                try { text = await navigator.clipboard.readText(); clipText.value = text; } catch (e) {}
            }
            if (!text) { showToast('没有可同步的文本', 'error'); return; }
            window.shellRemote.send('desktop:clipboard:set', { text: text });
            showToast('已写入远端剪贴板', 'success');
        });
        document.getElementById('clip-from-remote-btn').addEventListener('click', function() {
            window.shellRemote.send('desktop:clipboard:get', {});
            showToast('正在读取远端剪贴板…', 'success');
        });
    }
    window.shellRemote.on('desktop:clipboard', function(msg) {
        const text = (msg.payload && msg.payload.text) || '';
        if (clipText) clipText.value = text;
        if (text && navigator.clipboard && navigator.clipboard.writeText) {
            navigator.clipboard.writeText(text).then(function() {
                showToast('远端剪贴板已同步到本机', 'success');
            }).catch(function() {
                showToast('远端文本已载入面板（本机剪贴板写入被浏览器拒绝）', 'success');
            });
        } else {
            showToast(text ? '远端文本已载入面板' : '远端剪贴板为空', 'success');
        }
    });

    window.shellRemote.on('desktop:stopped', function(msg) {
        // 编码热切换中的 stop：保持桌面视图, 等 desktop:started 重连新流。
        if (window.__codecSwitchPending) return;
        if (desktopActive) {
            desktopView.disconnect();
            showTerminalView();
            showToast('桌面已关闭', 'success');
        }
    });

    window.shellRemote.on('desktop:error', function(msg) {
        // 捕获/编码器持续失败（例如 Wayland 下 XWayland root 无法 GetImage）
        desktopStarting = false;
        if (desktopActive) {
            desktopView.disconnect();
            showTerminalView();
        }
        if (msg.payload && msg.payload.error) {
            showToast('桌面捕获失败: ' + msg.payload.error, 'error');
        }
    });

    window.shellRemote.on('fs:result', function(msg) {
        if (msg.payload._upload_id) {
            const t = document.getElementById('toast');
            if (t && t.dataset.progressId === msg.payload._upload_id) {
                t.classList.add('hidden');
            }
        }
        if (msg.payload._mcp_request_id && files.handleDownloadResult(msg.payload._mcp_request_id, msg.payload)) {
            return;
        }
        // A chunked-download chunk that doesn't match a pending download here
        // (e.g. another browser's download broadcast to this session) — ignore
        // it so it doesn't spuriously refresh this browser's directory listing.
        if (msg.payload.chunk_index !== undefined) {
            return;
        }
        if (Array.isArray(msg.payload.entries)) {
            files.render(msg.payload.entries, msg.payload.path || files.currentPath);
        } else if (msg.payload.success && msg.payload.path) {
            files.loadDirectory(files.currentPath);
        }
    });

    window.shellRemote.on('session:agent_disconnect', function(msg) {
        clearJoinWatchdog();
        desktopView.disconnect();
        showTerminalView();
        disconnectText.textContent = '设备连接中断，正在自动重连…';
        disconnectOverlay.classList.remove('hidden');
    });

    window.shellRemote.on('session:error', function(msg) {
        if (msg.payload && msg.payload.code === 'AGENT_NOT_CONNECTED') {
            showToast('设备未连接，正在自动重试…', 'error');
        }
    });

    window.shellRemote.on('error', function(msg) {
        if (msg.payload.code === 'AUTH_INVALID_TOKEN') {
            showToast('密钥无效或已过期', 'error');
            setTimeout(() => window.location.href = '/', 2000);
        } else if (msg.payload.code === 'AUTH_INVALID_PASSWORD') {
            showToast('服务器密码错误', 'error');
            setTimeout(() => window.location.href = '/', 2000);
        } else if (msg.payload.code === 'PERMISSION_DENIED') {
            showToast('权限不足：只读访问', 'error');
        } else {
            showToast(msg.payload.message || '错误', 'error');
        }
    });

    // ── UI controls ────────────────────────────────────────────────────

    tabNewBtn.onclick = () => {
        window.shellRemote.send('session:tab_create', {});
        pendingTabSwitch = '__new__';
    };

    document.getElementById('copy-token-btn').addEventListener('click', () => {
        navigator.clipboard.writeText(token).then(() => {
            showToast('密钥已复制', 'success');
        }).catch(() => {
            const input = document.createElement('input');
            input.value = token;
            document.body.appendChild(input);
            input.select();
            document.execCommand('copy');
            document.body.removeChild(input);
            showToast('密钥已复制', 'success');
        });
    });

    document.getElementById('toggle-files-btn').addEventListener('click', () => {
        const isHidden = fileDrawer.classList.contains('hidden');
        if (isHidden) { fileDrawer.classList.remove('hidden'); files.init(); }
        else { fileDrawer.classList.add('hidden'); }
    });

    document.getElementById('close-files-btn').addEventListener('click', () => {
        fileDrawer.classList.add('hidden');
    });

    document.getElementById('file-new-folder-btn').onclick = () => files.createFolder();
    document.getElementById('file-refresh-btn').onclick = () => files.loadDirectory(files.currentPath);
    document.getElementById('file-upload-input').onchange = (e) => {
        const fileList = e.target.files;
        for (let i = 0; i < fileList.length; i++) {
            files.uploadFile(fileList[i]);
        }
        e.target.value = '';
    };

    fileDrawer.addEventListener('dragover', (e) => { e.preventDefault(); e.stopPropagation(); });
    fileDrawer.addEventListener('drop', (e) => {
        e.preventDefault(); e.stopPropagation();
        if (!e.dataTransfer || !e.dataTransfer.files) return;
        for (let i = 0; i < e.dataTransfer.files.length; i++) {
            files.uploadFile(e.dataTransfer.files[i]);
        }
    });

    document.getElementById('disconnect-ok-btn').addEventListener('click', () => {
        window.location.href = '/';
    });

    // Resizable file drawer
    let isResizing = false;

    fileResizer.addEventListener('mousedown', (e) => {
        isResizing = true;
        e.preventDefault();
    });
    document.addEventListener('mousemove', (e) => {
        if (!isResizing) return;
        const rect = document.querySelector('.main-content').getBoundingClientRect();
        const w = rect.right - e.clientX;
        fileDrawer.style.width = Math.max(180, Math.min(w, rect.width * 0.5)) + 'px';
    });
    document.addEventListener('mouseup', () => {
        isResizing = false;
    });
})();
