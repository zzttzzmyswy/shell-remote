class FileManager {
    constructor(wsClient, containerId) {
        this.ws = wsClient;
        this.container = document.getElementById(containerId);
        this.currentPath = '.';
        this._pendingDownloads = {};
        this._uploadQueue = [];
    }

    init() { this.loadDirectory('.'); }

    loadDirectory(path) {
        this.currentPath = path;
        this.ws.send({ type: 'fs:list', session_id: '', payload: { path: path } });
    }

    navigateTo(path) {
        this.loadDirectory(path);
    }

    render(entries, currentPath) {
        this.currentPath = currentPath;
        this.container.innerHTML = '';
        this._renderBreadcrumb(currentPath);
        this._renderUploaders();
        this._renderTable(entries, currentPath);
    }

    _renderBreadcrumb(path) {
        const bc = document.createElement('div');
        bc.className = 'breadcrumb';
        const parts = path === '/' ? [''] : path.split('/').filter(Boolean);
        let acc = '';

        const root = document.createElement('span');
        root.className = 'breadcrumb-item';
        root.textContent = '/';
        root.onclick = () => this.navigateTo('/');
        bc.appendChild(root);

        parts.forEach((p, i) => {
            const sep = document.createTextNode('');
            const span = document.createElement('span');
            span.className = 'breadcrumb-item';
            span.textContent = p;
            acc += '/' + p;
            const target = acc;
            span.onclick = () => this.navigateTo(target);
            if (i === parts.length - 1) span.classList.add('current');
            bc.appendChild(span);
        });
        this.container.appendChild(bc);
    }

    _renderUploaders() {
        const el = document.getElementById('upload-progress');
        if (!el) return;
        if (this._uploadQueue.length === 0) {
            el.classList.add('hidden');
            return;
        }
        el.classList.remove('hidden');
        el.innerHTML = '';
        const t = document.createElement('table');
        t.className = 'uploaders-table';
        t.innerHTML = '<thead><tr><th colspan="2">上传</th><th>进度</th></tr></thead><tbody></tbody>';
        const tbody = t.querySelector('tbody');
        this._uploadQueue.forEach(u => {
            const pct = u.total > 0 ? Math.round((u.loaded / u.total) * 100) : 0;
            const r = document.createElement('tr');
            r.innerHTML = `<td colspan="2">${this._esc(u.name)}</td><td><progress value="${u.loaded || 0}" max="${u.total || 1}"></progress><span>${pct}%</span></td>`;
            tbody.appendChild(r);
        });
        el.appendChild(t);
    }

    _renderTable(entries, currentPath) {
        const table = document.createElement('table');
        table.className = 'paths-table';

        const thead = document.createElement('thead');
        thead.innerHTML = '<tr><th>名称</th><th>大小</th><th>权限</th><th>所有者</th><th></th></tr>';
        table.appendChild(thead);

        const tbody = document.createElement('tbody');

        if (currentPath !== '/' && currentPath !== '.') {
            const row = document.createElement('tr');
            row.className = 'file-row';
            row.innerHTML = '<td class="file-name directory"><svg width="14" height="14" viewBox="0 0 16 16"><path fill="currentColor" d="M8.086 2.207a2 2 0 0 1 1.414.586l.828.828A2 2 0 0 0 11.742 4H14a2 2 0 0 1 2 2v6.5a1.5 1.5 0 0 1-1.5 1.5h-13A1.5 1.5 0 0 1 0 12.5v-9A1.5 1.5 0 0 1 1.5 2h5.172a2 2 0 0 1 1.414.586z"/></svg> ..</td><td></td><td></td><td></td><td></td>';
            row.addEventListener('click', () => {
                const parts = currentPath.split('/').filter(Boolean);
                parts.pop();
                this.navigateTo('/' + parts.join('/') || '/');
            });
            tbody.appendChild(row);
        }

        entries.forEach(entry => {
            const row = document.createElement('tr');
            row.className = 'file-row';
            row.dataset.path = entry.path;
            row.dataset.type = entry.type;

            const sizeStr = entry.type === 'file' ? this._formatSize(entry.size) : '';
            const icon = entry.type === 'directory'
                ? '<svg width="14" height="14" viewBox="0 0 16 16"><path fill="currentColor" d="M.54 3.87.5 3a2 2 0 0 1 2-2h3.672a2 2 0 0 1 1.414.586l.828.828A2 2 0 0 0 9.828 3h3.982a2 2 0 0 1 1.992 2.181l-.637 7A2 2 0 0 1 13.174 14H2.826a2 2 0 0 1-1.991-1.819l-.637-7a2 2 0 0 1 .342-1.31zM2.19 4a1 1 0 0 0-.996 1.09l.637 7a1 1 0 0 0 .995.91h10.348a1 1 0 0 0 .995-.91l.637-7A1 1 0 0 0 13.81 4H9.828a2 2 0 0 1-1.414-.586l-.828-.828A2 2 0 0 0 6.172 2H2.5a1 1 0 0 0-1 .98z"/></svg>'
                : '<svg width="14" height="14" viewBox="0 0 16 16"><path fill="currentColor" d="M14 4.5V14a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V2a2 2 0 0 1 2-2h5.5L14 4.5zm-3 0A1.5 1.5 0 0 1 9.5 3V1H4a1 1 0 0 0-1 1v12a1 1 0 0 0 1 1h8a1 1 0 0 0 1-1V4.5z"/></svg>';

            row.innerHTML =
                `<td class="file-name${entry.type === 'directory' ? ' directory' : ''}">${icon}<span>${this._esc(entry.name)}</span></td>` +
                `<td class="file-size">${sizeStr}</td>` +
                `<td class="file-perms">${this._esc(entry.mode || '')}</td>` +
                `<td class="file-owner">${this._esc(entry.owner || '')}</td>` +
                `<td class="file-actions">
                    ${entry.type === 'file' ? '<button class="btn-icon download" title="下载"><svg width="14" height="14" viewBox="0 0 16 16"><path fill="currentColor" d="M.5 9.9a.5.5 0 0 1 .5.5v2.5a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-2.5a.5.5 0 0 1 1 0v2.5a2 2 0 0 1-2 2H2a2 2 0 0 1-2-2v-2.5a.5.5 0 0 1 .5-.5"/><path fill="currentColor" d="M7.646 11.854a.5.5 0 0 0 .708 0l3-3a.5.5 0 0 0-.708-.708L8.5 10.293V1.5a.5.5 0 0 0-1 0v8.793L5.354 8.146a.5.5 0 1 0-.708.708z"/></svg></button>' : ''}
                    <button class="btn-icon delete" title="删除"><svg width="14" height="14" viewBox="0 0 16 16"><path fill="currentColor" d="M2.5 1a1 1 0 0 0-1 1v1a1 1 0 0 0 1 1H3v9a2 2 0 0 0 2 2h6a2 2 0 0 0 2-2V4h.5a1 1 0 0 0 1-1V2a1 1 0 0 0-1-1H10a1 1 0 0 0-1-1H7a1 1 0 0 0-1 1zm3 4a.5.5 0 0 1 .5.5v7a.5.5 0 0 1-1 0v-7a.5.5 0 0 1 .5-.5M8 5a.5.5 0 0 1 .5.5v7a.5.5 0 0 1-1 0v-7A.5.5 0 0 1 8 5m3 .5v7a.5.5 0 0 1-1 0v-7a.5.5 0 0 1 1 0"/></svg></button>
                </td>`;

            row.addEventListener('click', (e) => {
                if (e.target.closest('button')) return;
                this._selectRow(row);
                if (entry.type === 'directory') {
                    this.navigateTo(entry.path);
                }
            });

            row.addEventListener('contextmenu', (e) => {
                e.preventDefault();
                this._selectRow(row);
                this._showContextMenu(e.clientX, e.clientY, entry);
            });

            const dlBtn = row.querySelector('.btn-icon.download');
            if (dlBtn) dlBtn.addEventListener('click', (e) => { e.stopPropagation(); this.downloadFile(entry.path, entry.name); });

            const delBtn = row.querySelector('.btn-icon.delete');
            if (delBtn) delBtn.addEventListener('click', (e) => { e.stopPropagation(); this.deletePath(entry.path); });

            tbody.appendChild(row);
        });

        table.appendChild(tbody);
        this.container.appendChild(table);
    }

    _selectRow(row) {
        const all = this.container.querySelectorAll('.file-row');
        all.forEach(r => r.classList.remove('selected'));
        row.classList.add('selected');
    }

    uploadFile(file) {
        const token = sessionStorage.getItem('ssh-remote-token') || '';
        const fullPath = this.currentPath.endsWith('/')
            ? this.currentPath + file.name
            : this.currentPath + '/' + file.name;

        const q = { name: file.name, size: file.size, loaded: 0, total: file.size };
        this._uploadQueue.push(q);
        this._renderUploaders();

        const xhr = new XMLHttpRequest();
        xhr.upload.onprogress = (e) => {
            if (e.lengthComputable) {
                q.loaded = e.loaded;
                q.total = e.total;
                this._renderUploaders();
            }
        };
        xhr.onload = () => {
            this._uploadQueue = this._uploadQueue.filter(x => x !== q);
            this._renderUploaders();
            if (xhr.status !== 200) {
                console.error('Upload failed:', xhr.status, xhr.responseText);
            }
        };
        xhr.onerror = () => {
            this._uploadQueue = this._uploadQueue.filter(x => x !== q);
            this._renderUploaders();
            console.error('Upload network error');
        };
        xhr.open('POST', '/upload?token=' + encodeURIComponent(token) + '&path=' + encodeURIComponent(fullPath));
        xhr.send(file);
    }

    downloadFile(path, name) {
        const dlId = 'dl-' + Date.now();
        this._pendingDownloads[dlId] = { name };
        this._showToast('下载中: ' + name);
        this.ws.send({ type: 'fs:read', session_id: '', payload: { path: path, _mcp_request_id: dlId } });
    }

    handleDownloadResult(requestId, payload) {
        if (!this._pendingDownloads[requestId]) return false;
        const info = this._pendingDownloads[requestId];
        delete this._pendingDownloads[requestId];
        if (payload.success && payload.content) {
            try {
                const binaryStr = atob(payload.content);
                const bytes = new Uint8Array(binaryStr.length);
                for (let i = 0; i < binaryStr.length; i++) {
                    bytes[i] = binaryStr.charCodeAt(i);
                }
                const blob = new Blob([bytes]);
                const url = URL.createObjectURL(blob);
                const a = document.createElement('a');
                a.href = url; a.download = info.name;
                a.style.display = 'none';
                document.body.appendChild(a);
                a.click();
                setTimeout(() => {
                    document.body.removeChild(a);
                    URL.revokeObjectURL(url);
                }, 100);
            } catch (e) {
                console.error('Download failed:', e);
            }
        }
        return true;
    }

    createFolder() {
        const name = prompt('文件夹名称:');
        if (name) {
            const fp = this.currentPath === '/' ? '/' + name : this.currentPath + '/' + name;
            this.ws.send({ type: 'fs:mkdir', session_id: '', payload: { path: fp } });
        }
    }

    deletePath(path) {
        if (confirm('确定删除 ' + path + ' ？')) {
            this.ws.send({ type: 'fs:delete', session_id: '', payload: { path: path } });
        }
    }

    _showContextMenu(x, y, entry) {
        const ex = document.querySelector('.context-menu');
        if (ex) ex.remove();
        const m = document.createElement('div');
        m.className = 'context-menu';
        m.style.left = x + 'px'; m.style.top = y + 'px';

        const add = (text, cb, danger) => {
            const i = document.createElement('div');
            i.className = 'context-menu-item' + (danger ? ' context-menu-item-danger' : '');
            i.textContent = text;
            i.onclick = () => { m.remove(); cb(); };
            m.appendChild(i);
        };

        if (entry.type === 'file') {
            add('下载', () => this.downloadFile(entry.path, entry.name));
        }
        add('重命名', () => {
            const nn = prompt('新名称:', entry.name);
            if (nn && nn !== entry.name) {
                const np = entry.path.replace(/[^/]+$/, nn);
                this.ws.send({ type: 'fs:rename', session_id: '', payload: { from: entry.path, to: np } });
            }
        });
        add('删除', () => this.deletePath(entry.path), true);

        document.body.appendChild(m);
        const close = (e) => {
            if (e && m.contains(e.target)) return;
            m.remove();
            document.removeEventListener('mousedown', close, true);
            document.removeEventListener('contextmenu', close, true);
            window.removeEventListener('blur', close);
        };
        setTimeout(() => {
            document.addEventListener('mousedown', close, true);
            document.addEventListener('contextmenu', close, true);
            window.addEventListener('blur', close);
        }, 0);
    }

    _showToast(msg) {
        const t = document.getElementById('toast');
        if (t) { t.textContent = msg; t.className = 'toast info'; t.classList.remove('hidden'); }
    }

    _esc(s) { const d = document.createElement('div'); d.textContent = s; return d.innerHTML; }

    _formatSize(b) {
        if (b < 1024) return b + ' B';
        if (b < 1048576) return (b / 1024).toFixed(1) + ' KB';
        return (b / 1048576).toFixed(1) + ' MB';
    }
}
