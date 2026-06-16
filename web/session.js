(function() {
    const token = sessionStorage.getItem('shell-remote-token');
    const permission = sessionStorage.getItem('shell-remote-permission') || 'rw';

    if (!token) {
        window.location.href = '/';
        return;
    }

    let activeTabId = null;
    let pendingTabSwitch = null;
    let onlineUsers = 0;
    let tabs = [];

    const term = new TerminalManager('terminal-container');
    const files = new FileManager('file-tree');

    const onlineCountEl = document.getElementById('online-count');
    const sessionNameEl = document.getElementById('session-name');
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

    function updateOnlineCount() {
        onlineCountEl.textContent = onlineUsers + ' online';
    }

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
        var sessionId = window.shellRemote.getSessionId();
        sessionNameEl.textContent = '会话: ' + sessionId.substring(0, 8);
        disconnectOverlay.classList.add('hidden');
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
        onlineUsers = msg.payload.count || 0;
        updateOnlineCount();
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
        if (Array.isArray(msg.payload.entries)) {
            files.render(msg.payload.entries, msg.payload.path || files.currentPath);
        } else if (msg.payload.success && msg.payload.path) {
            files.loadDirectory(files.currentPath);
        }
    });

    window.shellRemote.on('session:agent_disconnect', function(msg) {
        disconnectText.textContent = '远程会话已结束';
        disconnectOverlay.classList.remove('hidden');
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
