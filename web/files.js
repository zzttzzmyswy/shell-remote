class FileManager {
    constructor(containerId) {
        this.container = document.getElementById(containerId);
        this.currentPath = '.';
        this._pendingDownloads = {};
        this._uploadQueue = [];
        // 下载 id：Date.now() 在快速连续点击时会同值（同一毫秒），后一次会把
        // 前一次 pending 覆盖掉。用单调自增计数器 + 前缀即可。
        this._downloadSeq = 0;
        // 表头排序状态：{key:'name'|'size', dir:1|-1}；null = 保持 agent 顺序
        // （agent 已按 目录优先 + 名称 排好，是默认视图）。
        this._sort = null;
        this._entries = [];
        this._currentPathForSort = '.';
        this._contextMenu = null;
    }

    init() { this.loadDirectory('.'); }

    // ── Windows / POSIX 路径工具（纯函数，便于单测）──────────────────
    // agent 可能跑在 Windows 上：列目录返回的 path 形如 `C:\Users\u\a.txt`。
    // 写死 '/' 会让面包屑、`..` 和重命名拼出混合分隔符的路径，
    // agent 侧 resolve 后可能指向不存在的目录。
    static separatorFor(path) {
        const p = String(path || '');
        if (p.indexOf('\\') !== -1 && p.indexOf('/') === -1) return '\\';
        if (p.indexOf('\\') !== -1 && /^[A-Za-z]:\\/.test(p)) return '\\';
        return '/';
    }

    static splitPath(path) {
        return String(path || '').split(/[\\/]/).filter(Boolean);
    }

    static isWindowsPath(path) {
        return FileManager.separatorFor(path) === '\\';
    }

    // 拼一个子项路径，保持父路径本身的分隔符风格。
    static joinPath(base, name) {
        const sep = FileManager.separatorFor(base);
        const b = String(base || '');
        if (!b) return name;
        if (b.endsWith('/') || b.endsWith('\\')) return b + name;
        return b + sep + name;
    }

    // 去掉最后一个分量，得到父目录。Windows 盘根的父目录是它自己
    // （`C:\a` → `C:\`，`C:\` → `C:\`）。
    static parentPath(path) {
        const p = String(path || '');
        const sep = FileManager.separatorFor(p);
        const parts = FileManager.splitPath(p);
        if (parts.length === 0) return p;
        const isWindows = sep === '\\';
        if (parts.length === 1) {
            // 已在根上：POSIX 根是 `/`，Windows 盘根是 `C:\`。
            return isWindows ? parts[0] + '\\' : '/';
        }
        parts.pop();
        if (isWindows) return parts.join('\\') + '\\';
        return '/' + parts.join('/');
    }

    // 替换最后一个分量（重命名）。新名由调用方校验过不含分隔符。
    static replaceLeaf(path, newName) {
        const p = String(path || '');
        const sep = FileManager.separatorFor(p);
        const parts = FileManager.splitPath(p);
        if (parts.length === 0) return newName;
        parts[parts.length - 1] = newName;
        if (sep === '\\') {
            // 保留盘符后的反斜杠风格：C:\a\b → C:\a\c
            return parts.join('\\');
        }
        return (p.startsWith('/') ? '/' : '') + parts.join('/');
    }

    static hasSeparator(name) {
        const n = String(name || '');
        return n.indexOf('/') !== -1 || n.indexOf('\\') !== -1;
    }

    loadDirectory(path) {
        this.currentPath = path;
        window.shellRemote.send('fs:list', { path: path });
    }

    navigateTo(path) {
        this.loadDirectory(path);
    }

    render(entries, currentPath) {
        this.currentPath = currentPath;
        this._entries = entries || [];
        this._currentPathForSort = currentPath;
        this.container.innerHTML = '';
        this._renderBreadcrumb(currentPath);
        this._renderUploaders();
        this._renderDownloads();
        this._renderTable(this._sortedEntries(), currentPath);
    }

    _renderBreadcrumb(path) {
        const bc = document.createElement('div');
        bc.className = 'breadcrumb';
        const sep = FileManager.separatorFor(path);
        const parts = FileManager.splitPath(path);
        let acc = '';
        const isWindows = sep === '\\';

        const root = document.createElement('span');
        root.className = 'breadcrumb-item';
        root.textContent = isWindows ? (parts[0] || '') + '\\' : '/';
        // Windows 盘根：`C:\`；POSIX：`/`。
        root.onclick = () => this.navigateTo(isWindows ? (parts[0] || '') + '\\' : '/');
        bc.appendChild(root);

        parts.forEach((p, i) => {
            // 第一段在 Windows 下已经作为根显示过了（盘符）。
            if (isWindows && i === 0) return;
            const span = document.createElement('span');
            span.className = 'breadcrumb-item';
            span.textContent = p;
            acc = acc ? FileManager.joinPath(acc, p) : (isWindows ? parts[0] + '\\' : '') + p;
            if (!isWindows && !acc.startsWith('/')) acc = '/' + acc;
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

    // 下载进行中列表（E2：下载要有进度条）。进度只能按「已收字节 / 总字节」
    // 估：分块协议按 chunk_index/total_chunks 推进，总字节要等最后一个
    // chunk 才知道，因此以块数为主进度、并在已知总字节时补一个字节进度。
    _renderDownloads() {
        const el = document.getElementById('download-progress');
        if (!el) return;
        const items = Object.keys(this._pendingDownloads).map(k => this._pendingDownloads[k]).filter(Boolean);
        if (items.length === 0) {
            el.classList.add('hidden');
            el.innerHTML = '';
            return;
        }
        el.classList.remove('hidden');
        el.innerHTML = '';
        items.forEach(d => {
            const row = document.createElement('div');
            row.className = 'dl-row';
            const pct = d.total > 0 ? Math.min(100, Math.round((d.received / d.total) * 100)) : 0;
            const name = document.createElement('span');
            name.className = 'dl-name';
            name.textContent = d.name || '';
            name.title = d.name || '';
            const bar = document.createElement('progress');
            bar.max = 100;
            bar.value = pct;
            bar.setAttribute('aria-label', '下载进度 ' + (d.name || ''));
            const label = document.createElement('span');
            label.className = 'dl-pct';
            label.textContent = d.total > 0 ? pct + '%' : '…';
            row.appendChild(name);
            row.appendChild(bar);
            row.appendChild(label);
            el.appendChild(row);
        });
    }

    // 排序后的条目：目录始终排在文件前（与 agent 的默认顺序一致），组内按
    // 选定键排序。第三次点击回到「默认」。
    //
    // 只提供「名称 / 大小」两列可排序：fs:list 的 FileEntry 没有 mtime
    // 字段（src/proto/mod.rs），补时间需要改协议 —— 本卡明令禁止。见交付
    // 说明的偏差一节。
    _sortedEntries() {
        const entries = this._entries.slice();
        const s = this._sort;
        if (!s) return entries;
        const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });
        entries.sort((a, b) => {
            const aDir = a.type === 'directory';
            const bDir = b.type === 'directory';
            if (aDir !== bDir) return aDir ? -1 : 1;
            let r;
            if (s.key === 'size') r = (a.size || 0) - (b.size || 0);
            else r = collator.compare(a.name || '', b.name || '');
            if (r === 0) r = collator.compare(a.name || '', b.name || '');
            return r * s.dir;
        });
        return entries;
    }

    _toggleSort(key) {
        const s = this._sort;
        if (!s || s.key !== key) this._sort = { key: key, dir: 1 };
        else if (s.dir === 1) this._sort = { key: key, dir: -1 };
        else this._sort = null;   // 第三次点击：回到默认顺序
        this._renderTable(this._sortedEntries(), this._currentPathForSort);
    }

    _sortHeaderCell(label, key) {
        const th = document.createElement('th');
        th.className = 'sortable';
        th.textContent = label;
        th.tabIndex = 0;
        th.setAttribute('scope', 'col');
        const s = this._sort;
        if (s && s.key === key) {
            th.setAttribute('aria-sort', s.dir === 1 ? 'ascending' : 'descending');
        } else {
            th.setAttribute('aria-sort', 'none');
        }
        const act = () => this._toggleSort(key);
        th.addEventListener('click', act);
        th.addEventListener('keydown', (e) => {
            if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); act(); }
        });
        return th;
    }

    _renderTable(entries, currentPath) {
        const table = document.createElement('table');
        table.className = 'paths-table';

        const thead = document.createElement('thead');
        const headRow = document.createElement('tr');
        headRow.appendChild(this._sortHeaderCell('名称', 'name'));
        headRow.appendChild(this._sortHeaderCell('大小', 'size'));
        const thPerm = document.createElement('th');
        thPerm.textContent = '权限';
        thPerm.setAttribute('scope', 'col');
        headRow.appendChild(thPerm);
        const thOwner = document.createElement('th');
        thOwner.textContent = '所有者';
        thOwner.setAttribute('scope', 'col');
        headRow.appendChild(thOwner);
        const thAct = document.createElement('th');
        thAct.setAttribute('scope', 'col');
        thAct.innerHTML = '<span aria-hidden="true"></span>';
        headRow.appendChild(thAct);
        thead.appendChild(headRow);
        table.appendChild(thead);

        const tbody = document.createElement('tbody');

        const atRoot = currentPath === '/'
            || currentPath === '.'
            || /^[A-Za-z]:[\\/]$/.test(currentPath || '');
        if (!atRoot) {
            const row = document.createElement('tr');
            row.className = 'file-row';
            row.innerHTML = '<td class="file-name directory"><svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M8.086 2.207a2 2 0 0 1 1.414.586l.828.828A2 2 0 0 0 11.742 4H14a2 2 0 0 1 2 2v6.5a1.5 1.5 0 0 1-1.5 1.5h-13A1.5 1.5 0 0 1 0 12.5v-9A1.5 1.5 0 0 1 1.5 2h5.172a2 2 0 0 1 1.414.586z"/></svg> ..</td><td></td><td></td><td></td><td></td><td></td>';
            row.addEventListener('click', () => {
                this.navigateTo(FileManager.parentPath(currentPath));
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
                ? '<svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M.54 3.87.5 3a2 2 0 0 1 2-2h3.672a2 2 0 0 1 1.414.586l.828.828A2 2 0 0 0 9.828 3h3.982a2 2 0 0 1 1.992 2.181l-.637 7A2 2 0 0 1 13.174 14H2.826a2 2 0 0 1-1.991-1.819l-.637-7a2 2 0 0 1 .342-1.31zM2.19 4a1 1 0 0 0-.996 1.09l.637 7a1 1 0 0 0 .995.91h10.348a1 1 0 0 0 .995-.91l.637-7A1 1 0 0 0 13.81 4H9.828a2 2 0 0 1-1.414-.586l-.828-.828A2 2 0 0 0 6.172 2H2.5a1 1 0 0 0-1 .98z"/></svg>'
                : '<svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M14 4.5V14a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V2a2 2 0 0 1 2-2h5.5L14 4.5zm-3 0A1.5 1.5 0 0 1 9.5 3V1H4a1 1 0 0 0-1 1v12a1 1 0 0 0 1 1h8a1 1 0 0 0 1-1V4.5z"/></svg>';

            row.innerHTML =
                `<td class="file-name${entry.type === 'directory' ? ' directory' : ''}" title="${this._esc(entry.name)}">${icon}<span>${this._esc(entry.name)}</span></td>` +
                `<td class="file-size">${sizeStr}</td>` +
                `<td class="file-perms">${this._esc(entry.mode || '')}</td>` +
                `<td class="file-owner">${this._esc(entry.owner || '')}</td>` +
                `<td class="file-actions">
                    ${entry.type === 'file' ? '<button type="button" class="btn-icon download" title="下载" aria-label="下载 ' + this._escAttr(entry.name) + '"><svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M.5 9.9a.5.5 0 0 1 .5.5v2.5a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-2.5a.5.5 0 0 1 1 0v2.5a2 2 0 0 1-2 2H2a2 2 0 0 1-2-2v-2.5a.5.5 0 0 1 .5-.5"/><path fill="currentColor" d="M7.646 11.854a.5.5 0 0 0 .708 0l3-3a.5.5 0 0 0-.708-.708L8.5 10.293V1.5a.5.5 0 0 0-1 0v8.793L5.354 8.146a.5.5 0 1 0-.708.708z"/></svg></button>' : ''}
                    <button type="button" class="btn-icon delete" title="删除" aria-label="删除 ' + this._escAttr(entry.name) + '"><svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M2.5 1a1 1 0 0 0-1 1v1a1 1 0 0 0 1 1H3v9a2 2 0 0 0 2 2h6a2 2 0 0 0 2-2V4h.5a1 1 0 0 0 1-1V2a1 1 0 0 0-1-1H10a1 1 0 0 0-1-1H7a1 1 0 0 0-1 1zm3 4a.5.5 0 0 1 .5.5v7a.5.5 0 0 1-1 0v-7a.5.5 0 0 1 .5-.5M8 5a.5.5 0 0 1 .5.5v7a.5.5 0 0 1-1 0v-7A.5.5 0 0 1 8 5m3 .5v7a.5.5 0 0 1-1 0v-7a.5.5 0 0 1 1 0"/></svg></button>
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
                this._showContextMenu(e.clientX, e.clientY, entry, row);
            });

            const dlBtn = row.querySelector('.btn-icon.download');
            if (dlBtn) dlBtn.addEventListener('click', (e) => { e.stopPropagation(); this.downloadFile(entry.path, entry.name); });

            const delBtn = row.querySelector('.btn-icon.delete');
            if (delBtn) delBtn.addEventListener('click', (e) => { e.stopPropagation(); this.deletePath(entry.path); });

            tbody.appendChild(row);
        });

        // 空目录提示
        if (entries.length === 0 && atRoot) {
            const row = document.createElement('tr');
            const td = document.createElement('td');
            td.colSpan = 5;
            td.className = 'empty-state';
            td.textContent = '空目录';
            row.appendChild(td);
            tbody.appendChild(row);
        }

        table.appendChild(tbody);
        this.container.appendChild(table);
    }

    _selectRow(row) {
        const all = this.container.querySelectorAll('.file-row');
        all.forEach(r => r.classList.remove('selected'));
        row.classList.add('selected');
    }

    uploadFile(file) {
        const token = sessionStorage.getItem('shell-remote-token') || '';
        const fullPath = FileManager.joinPath(this.currentPath, file.name);

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
            if (xhr.status < 200 || xhr.status >= 300) {
                console.error('Upload failed:', xhr.status, xhr.responseText);
                this._showToast('上传失败: ' + file.name + ' (HTTP ' + xhr.status + ')');
            } else {
                this.loadDirectory(this.currentPath); // 刷新目录列表
            }
        };
        xhr.onerror = () => {
            this._uploadQueue = this._uploadQueue.filter(x => x !== q);
            this._renderUploaders();
            console.error('Upload network error');
            this._showToast('上传失败: ' + file.name + ' (网络错误)');
        };
        // relay 的 /agent/upload 用 extract_token_from_headers_or_query 先看
        // Authorization: Bearer —— URL 里不必再带 token（会被 nginx/访问日志
        // 记录下来）。
        xhr.open('POST', '/agent/upload?path=' + encodeURIComponent(fullPath));
        xhr.setRequestHeader('Authorization', 'Bearer ' + token);
        const pw = sessionStorage.getItem('shell-remote-auth');
        if (pw) xhr.setRequestHeader('X-Auth', pw);
        xhr.send(file);
    }

    downloadFile(path, name) {
        const dlId = 'dl-' + (++this._downloadSeq);
        // The agent streams the file as multiple chunked fs:result messages
        // (256 KiB each) to keep messages small; we reassemble here.
        // `done`/`total` 是块数进度，`received` 是已收字节数（E2 进度条用）。
        this._pendingDownloads[dlId] = {
            name, chunks: [], total: 0, done: 0, received: 0, timer: null
        };
        // Safety net: abandon a stalled download so a dropped chunk (slow
        // browser hitting the relay's bounded channel) doesn't hang forever.
        this._pendingDownloads[dlId].timer = setTimeout(() => {
            if (this._pendingDownloads[dlId]) {
                delete this._pendingDownloads[dlId];
                this._renderDownloads();
                this._showToast('下载超时: ' + name, 'error');
            }
        }, 120000);
        this._renderDownloads();
        this._showToast('下载中: ' + name);
        window.shellRemote.send('fs:read', { path: path, _mcp_request_id: dlId });
    }

    handleDownloadResult(requestId, payload) {
        const info = this._pendingDownloads[requestId];
        if (!info) return false;

        if (payload.success === false) {
            clearTimeout(info.timer);
            delete this._pendingDownloads[requestId];
            this._renderDownloads();
            this._showToast('下载失败: ' + (payload.error || info.name), 'error');
            return true;
        }

        // Chunked path: accumulate base64 chunks until the last, then save.
        if (payload.chunk_index !== undefined && payload.total_chunks !== undefined) {
            try {
                const b64 = payload.content || '';
                const binaryStr = atob(b64);
                const bytes = new Uint8Array(binaryStr.length);
                for (let i = 0; i < binaryStr.length; i++) bytes[i] = binaryStr.charCodeAt(i);
                info.chunks.push(bytes);
                info.total = payload.total_chunks;
                info.received += bytes.length;
                info.done = payload.chunk_index + 1;
                if (payload.chunk_index + 1 >= payload.total_chunks) {
                    clearTimeout(info.timer);
                    const totalLen = info.chunks.reduce((a, c) => a + c.length, 0);
                    const all = new Uint8Array(totalLen);
                    let off = 0;
                    for (const c of info.chunks) { all.set(c, off); off += c.length; }
                    delete this._pendingDownloads[requestId];
                    this._renderDownloads();
                    this._saveBlob(all, payload.name || info.name);
                } else {
                    this._renderDownloads();
                }
            } catch (e) {
                clearTimeout(info.timer);
                delete this._pendingDownloads[requestId];
                this._renderDownloads();
                console.error('Download reassembly failed:', e);
            }
            return true;
        }

        // Legacy single-message download (fallback).
        clearTimeout(info.timer);
        delete this._pendingDownloads[requestId];
        this._renderDownloads();
        if (payload.content) {
            try {
                const binaryStr = atob(payload.content);
                const bytes = new Uint8Array(binaryStr.length);
                for (let i = 0; i < binaryStr.length; i++) bytes[i] = binaryStr.charCodeAt(i);
                this._saveBlob(bytes, payload.name || info.name);
            } catch (e) {
                console.error('Download failed:', e);
            }
        }
        return true;
    }

    _saveBlob(bytes, name) {
        const blob = new Blob([bytes]);
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url; a.download = name;
        a.style.display = 'none';
        document.body.appendChild(a);
        a.click();
        setTimeout(() => {
            document.body.removeChild(a);
            URL.revokeObjectURL(url);
        }, 100);
    }

    async createFolder() {
        const name = await UI.prompt('文件夹名称：', '', { title: '新建文件夹', placeholder: 'dir-name' });
        if (name === null) return;
        if (!name.trim() || FileManager.hasSeparator(name)) {
            this._showToast('新建失败: 名称不能为空，也不能包含 / 或 \\', 'error');
            return;
        }
        const fp = FileManager.joinPath(this.currentPath, name.trim());
        window.shellRemote.send('fs:mkdir', { path: fp });
    }

    async deletePath(path) {
        const ok = await UI.confirm('确定删除 ' + path + ' ？', { title: '删除', danger: true, okLabel: '删除' });
        if (ok) {
            window.shellRemote.send('fs:delete', { path: path });
        }
    }

    // 右键菜单：支持键盘（Esc 关闭，↑/↓ 选择，Enter 执行）。打开后焦点落在
    // 第一项，因此纯键盘用户不必回到鼠标。
    _showContextMenu(x, y, entry) {
        this._closeContextMenu();
        const m = document.createElement('div');
        m.className = 'context-menu';
        m.setAttribute('role', 'menu');
        m.setAttribute('aria-label', '文件操作');

        const items = [];
        const add = (text, cb, danger) => {
            const i = document.createElement('div');
            i.className = 'context-menu-item' + (danger ? ' context-menu-item-danger' : '');
            i.textContent = text;
            i.tabIndex = -1;
            i.setAttribute('role', 'menuitem');
            i.addEventListener('click', () => { this._closeContextMenu(); cb(); });
            m.appendChild(i);
            items.push(i);
        };

        if (entry.type === 'file') {
            add('下载', () => this.downloadFile(entry.path, entry.name));
        }
        add('重命名', () => this._renameEntry(entry));
        add('删除', () => this.deletePath(entry.path), true);

        document.body.appendChild(m);
        // 视口内收边：贴右下角弹出时不要溢出。
        const rect = m.getBoundingClientRect();
        const left = Math.min(x, Math.max(0, window.innerWidth - rect.width - 8));
        const top = Math.min(y, Math.max(0, window.innerHeight - rect.height - 8));
        m.style.left = left + 'px';
        m.style.top = top + 'px';

        const focusAt = (i) => {
            const idx = Math.max(0, Math.min(items.length - 1, i));
            items[idx].focus();
        };
        m.addEventListener('keydown', (e) => {
            const cur = items.indexOf(document.activeElement);
            if (e.key === 'Escape') {
                e.preventDefault();
                e.stopPropagation();
                this._closeContextMenu();
                return;
            }
            if (e.key === 'ArrowDown') { e.preventDefault(); focusAt(cur + 1); }
            else if (e.key === 'ArrowUp') { e.preventDefault(); focusAt(cur <= 0 ? items.length - 1 : cur - 1); }
            else if (e.key === 'Home') { e.preventDefault(); focusAt(0); }
            else if (e.key === 'End') { e.preventDefault(); focusAt(items.length - 1); }
            else if (e.key === 'Tab') { this._closeContextMenu(); }
        });

        const close = (e) => {
            if (e && e.type === 'contextmenu' && m.contains(e.target)) return;
            if (e && e.type === 'mousedown' && m.contains(e.target)) return;
            this._closeContextMenu();
        };
        this._contextMenu = { el: m, close: close };
        setTimeout(() => {
            document.addEventListener('mousedown', close, true);
            document.addEventListener('contextmenu', close, true);
            window.addEventListener('blur', close);
            window.addEventListener('resize', close);
            focusAt(0);
        }, 0);
    }

    _closeContextMenu() {
        const cm = this._contextMenu;
        if (!cm) return;
        this._contextMenu = null;
        cm.el.remove();
        document.removeEventListener('mousedown', cm.close, true);
        document.removeEventListener('contextmenu', cm.close, true);
        window.removeEventListener('blur', cm.close);
        window.removeEventListener('resize', cm.close);
    }

    async _renameEntry(entry) {
        const nn = await UI.prompt('新名称：', entry.name, { title: '重命名' });
        if (nn === null || nn === entry.name) return;
        if (!nn.trim() || FileManager.hasSeparator(nn)) {
            this._showToast('重命名失败: 名称不能为空，也不能包含 / 或 \\', 'error');
            return;
        }
        const np = FileManager.replaceLeaf(entry.path, nn.trim());
        window.shellRemote.send('fs:rename', { from: entry.path, to: np });
    }

    // 与 session.js 的 showToast 同一策略：错误 6s，其余 3s。
    _showToast(msg, kind) {
        UI.toast(msg, kind || 'info', kind === 'error' ? 6000 : 3000);
    }

    _esc(s) { const d = document.createElement('div'); d.textContent = s; return d.innerHTML; }
    _escAttr(s) { return this._esc(s).replace(/"/g, '&quot;'); }

    _formatSize(b) {
        if (b < 1024) return b + ' B';
        if (b < 1048576) return (b / 1024).toFixed(1) + ' KB';
        return (b / 1048576).toFixed(1) + ' MB';
    }
}

// 路径工具是纯函数：挂到 window 供 verify_mys2283_c.js 用 page.evaluate
// 直接断言（没有 Windows 环境，靠单测覆盖分隔符分支）。真实页面里
// files.js 与其它脚本共享全局作用域，这里只新增一个命名空间。
window.SRPathUtils = {
    separatorFor: FileManager.separatorFor,
    splitPath: FileManager.splitPath,
    isWindowsPath: FileManager.isWindowsPath,
    joinPath: FileManager.joinPath,
    parentPath: FileManager.parentPath,
    replaceLeaf: FileManager.replaceLeaf,
    hasSeparator: FileManager.hasSeparator
};
