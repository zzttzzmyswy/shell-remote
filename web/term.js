class TerminalManager {
    constructor(containerId) {
        this.container = document.getElementById(containerId);
        this._observer = null;

        this.term = new Terminal({
            cursorBlink: true,
            fontSize: 14,
            fontFamily: "'Sarasa Term SC', 'Sarasa Mono SC', 'Noto Sans Mono CJK SC', 'JetBrains Mono', 'Fira Code', 'Consolas', monospace",
            // 配色来自 theme.css 的设计令牌（唯一来源），不再写死第三份调色板。
            theme: TerminalManager.readTheme(),
        });

        this.fitAddon = new FitAddon.FitAddon();
        this.term.loadAddon(this.fitAddon);

        try {
            this.webglAddon = new WebglAddon.WebglAddon();
            this.term.loadAddon(this.webglAddon);
        } catch (e) {
            console.warn('WebGL addon not available, falling back to canvas');
        }

        this.term.open(this.container);

        const doFit = () => {
            try { this.fitAddon.fit(); } catch (e) { console.warn('Fit error:', e); }
        };

        // Wait for fonts to load before first fit (ensures correct char dimensions)
        document.fonts.ready.then(() => doFit());

        // ResizeObserver catches ALL size changes (drawer open/close, window resize, manual drag)
        let debounceTimer = null;
        this._observer = new ResizeObserver(() => {
            clearTimeout(debounceTimer);
            debounceTimer = setTimeout(() => doFit(), 50);
        });
        this._observer.observe(this.container);

        this.term.onResize(({ cols, rows }) => {
            if (this.onResizeCallback) {
                this.onResizeCallback(cols, rows);
            }
        });

        this.onInputCallback = null;
        this.onResizeCallback = null;

        this.term.onData((data) => {
            if (this.onInputCallback) {
                this.onInputCallback(data);
            }
        });
    }

    // 从 CSS 变量读出 xterm 主题：theme.css 里 :root[data-theme=...] 定义，
    // 切换主题后调用 applyTheme() 重新读取即可，不需要在 JS 里复制颜色。
    static readTheme() {
        const css = getComputedStyle(document.documentElement);
        const v = (name, fallback) => {
            const raw = css.getPropertyValue(name);
            return raw && raw.trim() ? raw.trim() : fallback;
        };
        return {
            background: v('--term-bg', '#0b0f17'),
            foreground: v('--term-fg', '#d8deea'),
            cursor: v('--term-cursor', '#8ca4ff'),
            cursorAccent: v('--term-cursor-accent', '#0b0f17'),
            selectionBackground: v('--term-selection', 'rgba(109, 140, 255, 0.35)'),
            black: v('--term-black', '#000000'),
            red: v('--term-red', '#cd3131'),
            green: v('--term-green', '#0dbc79'),
            yellow: v('--term-yellow', '#e5e510'),
            blue: v('--term-blue', '#2472c8'),
            magenta: v('--term-magenta', '#bc3fbc'),
            cyan: v('--term-cyan', '#11a8cd'),
            white: v('--term-white', '#e5e5e5'),
            brightBlack: v('--term-bright-black', '#666666'),
            brightRed: v('--term-bright-red', '#f14c4c'),
            brightGreen: v('--term-bright-green', '#23d18b'),
            brightYellow: v('--term-bright-yellow', '#f5f543'),
            brightBlue: v('--term-bright-blue', '#3b8eea'),
            brightMagenta: v('--term-bright-magenta', '#d670d6'),
            brightCyan: v('--term-bright-cyan', '#29b8db'),
            brightWhite: v('--term-bright-white', '#ffffff'),
        };
    }

    // 主题切换后调用（session.js 监听 sr-theme-change）。
    applyTheme() {
        try {
            this.term.options.theme = TerminalManager.readTheme();
            // WebGL 渲染器把颜色烘进 atlas，需要重建才能拾取新主题；
            // canvas 渲染器会自动重绘，重建失败也不能影响终端可用性。
            if (this.webglAddon && typeof this.webglAddon.clearTextureAtlas === 'function') {
                this.webglAddon.clearTextureAtlas();
            }
        } catch (e) {
            console.warn('Terminal theme update failed:', e);
        }
    }

    write(data) {
        this.term.write(data);
    }

    onInput(callback) {
        this.onInputCallback = callback;
    }

    onResize(callback) {
        this.onResizeCallback = callback;
    }

    resize() {
        try { this.fitAddon.fit(); } catch (e) { console.warn('Fit error:', e); }
    }

    getCols() {
        return this.term.cols;
    }

    getRows() {
        return this.term.rows;
    }

    focus() {
        this.term.focus();
    }

    clear() {
        this.term.clear();
    }

    // 清屏并清空回滚区（重连重放缓冲输出前用，避免内容重复）
    reset() {
        this.term.reset();
    }
}
