// ui.js — shared web UI kit: theme switching + in-page dialogs (MYS-2288 E1/E4).
//
// Dialogs replace every native browser alert / confirm / prompt on the user
// pages and the admin panel (none are left in the sources). Each dialog is a
// real modal:
//   role="dialog" aria-modal="true", labelled by its title/message
//   focus is trapped inside; Esc cancels; Enter confirms
//   the previously focused element is restored on close
//
// Public API
//   UI.confirm(msg, {danger}) -> Promise<boolean>
//   UI.prompt(msg, defaultValue) -> Promise<string|null>
//   UI.alert(msg) -> Promise<void>
//   UI.showSecret(title, text) -> Promise<void>   (read-only field + copy)
//   UI.toast(msg, kind, ms)                       (uses the page's #toast)
//   UI.theme.mode / UI.theme.theme / UI.theme.set / UI.theme.cycle
//
// The palette lives in /theme.css; nothing here defines colours.
(function () {
    'use strict';

    // ── Theme controller ────────────────────────────────────────────────
    // The inline <head> snippet applies the stored theme before first paint;
    // this object owns everything after that. Keep the key and the mode names
    // in sync with that snippet.
    var THEME_KEY = 'sr-theme';
    var THEME_MODES = ['system', 'light', 'dark'];
    var THEME_LABELS = {
        system: { zh: '跟随系统', en: 'System' },
        light: { zh: '浅色', en: 'Light' },
        dark: { zh: '深色', en: 'Dark' }
    };
    var themeMedia = window.matchMedia ? window.matchMedia('(prefers-color-scheme: light)') : null;
    var themeMode = readThemeMode();

    function readThemeMode() {
        try {
            var v = localStorage.getItem(THEME_KEY);
            if (THEME_MODES.indexOf(v) !== -1) return v;
        } catch (e) { /* private mode / storage disabled */ }
        return 'system';
    }

    function resolveTheme(m) {
        if (m === 'light' || m === 'dark') return m;
        return themeMedia && themeMedia.matches ? 'light' : 'dark';
    }

    function themeLabel(m) {
        var lang = document.documentElement.lang || 'zh';
        var isEn = lang.toLowerCase().indexOf('en') === 0;
        var entry = THEME_LABELS[m] || THEME_LABELS.system;
        return isEn ? entry.en : entry.zh;
    }

    function applyTheme(notify) {
        var root = document.documentElement;
        var resolved = resolveTheme(themeMode);
        root.setAttribute('data-theme', resolved);
        root.setAttribute('data-theme-mode', themeMode);
        var btns = document.querySelectorAll('.theme-toggle');
        for (var i = 0; i < btns.length; i++) {
            btns[i].setAttribute('data-mode', themeMode);
            var label = themeLabel(themeMode);
            btns[i].setAttribute('title', label);
            btns[i].setAttribute('aria-label', label);
        }
        if (notify !== false) {
            document.dispatchEvent(new CustomEvent('sr-theme-change', {
                detail: { mode: themeMode, theme: resolved }
            }));
        }
    }

    var theme = {
        modes: THEME_MODES.slice(),
        get mode() { return themeMode; },
        get theme() { return resolveTheme(themeMode); },
        set: function (m) {
            if (THEME_MODES.indexOf(m) === -1 || m === themeMode) return;
            themeMode = m;
            try { localStorage.setItem(THEME_KEY, m); } catch (e) { /* ignore */ }
            applyTheme();
        },
        // system → light → dark → system
        cycle: function () {
            theme.set(THEME_MODES[(THEME_MODES.indexOf(themeMode) + 1) % THEME_MODES.length]);
        },
        // Re-apply labels (the admin panel calls this after a language switch)
        // and bind every .theme-toggle button exactly once.
        init: function () {
            applyTheme(false);
            var btns = document.querySelectorAll('.theme-toggle');
            for (var i = 0; i < btns.length; i++) {
                if (btns[i].dataset.themeBound === '1') continue;
                btns[i].dataset.themeBound = '1';
                btns[i].addEventListener('click', theme.cycle);
            }
            return resolveTheme(themeMode);
        },
        refreshLabels: function () { applyTheme(false); }
    };

    if (themeMedia) {
        var onSchemeChange = function () { if (themeMode === 'system') applyTheme(); };
        if (themeMedia.addEventListener) themeMedia.addEventListener('change', onSchemeChange);
        else if (themeMedia.addListener) themeMedia.addListener(onSchemeChange);
    }

    // ── Dialog plumbing ─────────────────────────────────────────────────
    var stack = [];       // open dialogs, innermost last
    var FOCUSABLE = 'a[href], button:not([disabled]), input:not([disabled]), ' +
        'select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

    function activeDialog() {
        return stack.length ? stack[stack.length - 1] : null;
    }

    function focusables(root) {
        var list = root.querySelectorAll(FOCUSABLE);
        var out = [];
        for (var i = 0; i < list.length; i++) {
            if (list[i].offsetParent !== null || list[i] === document.activeElement) out.push(list[i]);
        }
        return out;
    }

    function onKeydown(e) {
        var d = activeDialog();
        if (!d) return;
        if (e.key === 'Escape') {
            e.preventDefault();
            e.stopPropagation();
            d.close(d.cancelValue);
            return;
        }
        if (e.key === 'Enter' && !e.shiftKey) {
            // Let textareas keep their newlines; everything else confirms.
            var t = e.target;
            var multiline = t && t.tagName === 'TEXTAREA';
            if (!multiline) {
                e.preventDefault();
                e.stopPropagation();
                d.close(d.confirmValue());
                return;
            }
        }
        if (e.key === 'Tab') {
            var f = focusables(d.root);
            if (f.length === 0) { e.preventDefault(); return; }
            var first = f[0], last = f[f.length - 1];
            if (e.shiftKey && (document.activeElement === first || !d.root.contains(document.activeElement))) {
                e.preventDefault();
                last.focus();
            } else if (!e.shiftKey && document.activeElement === last) {
                e.preventDefault();
                first.focus();
            }
        }
    }

    // Build the dialog shell and wire focus handling. Returns the config object
    // with a close(result) method; caller fills the body.
    function openDialog(opts) {
        var prevFocus = document.activeElement;
        var root = document.createElement('div');
        root.className = 'sr-dialog-mask';
        root.setAttribute('data-sr-dialog', opts.kind || 'dialog');

        var box = document.createElement('div');
        box.className = 'sr-dialog' + (opts.danger ? ' sr-dialog-danger' : '');
        box.setAttribute('role', 'dialog');
        box.setAttribute('aria-modal', 'true');
        var titleId = 'sr-dialog-title-' + (stack.length + 1) + '-' + Date.now();
        box.setAttribute('aria-labelledby', titleId);
        root.appendChild(box);

        var head = document.createElement('div');
        head.className = 'sr-dialog-head';
        head.id = titleId;
        head.textContent = opts.title || '';
        box.appendChild(head);

        var body = document.createElement('div');
        body.className = 'sr-dialog-body';
        box.appendChild(body);

        var actions = document.createElement('div');
        actions.className = 'sr-dialog-actions';
        box.appendChild(actions);

        var closed = false;
        var api = {
            kind: opts.kind || 'dialog',
            root: root,
            box: box,
            body: body,
            actions: actions,
            cancelValue: opts.cancelValue === undefined ? null : opts.cancelValue,
            confirmValue: opts.confirmValue || function () { return true; },
            close: function (result) {
                if (closed) return;
                closed = true;
                var idx = stack.indexOf(api);
                if (idx !== -1) stack.splice(idx, 1);
                if (stack.length === 0) document.removeEventListener('keydown', onKeydown, true);
                root.remove();
                if (prevFocus && typeof prevFocus.focus === 'function' && document.contains(prevFocus)) {
                    try { prevFocus.focus(); } catch (e) { /* gone */ }
                }
                if (opts.onClose) opts.onClose(result);
            }
        };
        stack.push(api);
        document.addEventListener('keydown', onKeydown, true);
        document.body.appendChild(root);
        return api;
    }

    function addButton(parent, label, cls, onClick) {
        var b = document.createElement('button');
        b.type = 'button';
        b.className = 'sr-dialog-btn' + (cls ? ' ' + cls : '');
        b.textContent = label;
        b.addEventListener('click', onClick);
        parent.appendChild(b);
        return b;
    }

    function defaultLabels() {
        var isEn = (document.documentElement.lang || 'zh').toLowerCase().indexOf('en') === 0;
        return isEn
            ? { ok: 'OK', cancel: 'Cancel', confirm: 'Confirm', copy: 'Copy', copied: 'Copied' }
            : { ok: '确定', cancel: '取消', confirm: '确定', copy: '复制', copied: '已复制' };
    }

    function messageEl(parent, msg) {
        var p = document.createElement('p');
        p.className = 'sr-dialog-msg';
        p.textContent = msg == null ? '' : String(msg);
        parent.appendChild(p);
        return p;
    }

    // ── Public dialog API ───────────────────────────────────────────────
    var UI = {};

    UI.alert = function (msg, opts) {
        opts = opts || {};
        var labels = defaultLabels();
        return new Promise(function (resolve) {
            var d = openDialog({
                kind: 'alert',
                title: opts.title || '',
                cancelValue: undefined,
                confirmValue: function () { return true; },
                onClose: function () { resolve(); }
            });
            messageEl(d.body, msg);
            var ok = addButton(d.actions, opts.okLabel || opts.confirmLabel || labels.ok, 'sr-dialog-btn-primary', function () {
                d.close(true);
            });
            setTimeout(function () { ok.focus(); }, 0);
        });
    };

    UI.confirm = function (msg, opts) {
        opts = opts || {};
        var labels = defaultLabels();
        return new Promise(function (resolve) {
            var d = openDialog({
                kind: 'confirm',
                title: opts.title || '',
                danger: !!opts.danger,
                cancelValue: false,
                confirmValue: function () { return true; },
                onClose: function (r) { resolve(r === true); }
            });
            messageEl(d.body, msg);
            addButton(d.actions, opts.cancelLabel || labels.cancel, 'sr-dialog-btn-ghost', function () {
                d.close(false);
            });
            var ok = addButton(d.actions,
                opts.okLabel || opts.confirmLabel || (opts.danger ? labels.confirm : labels.ok),
                opts.danger ? 'sr-dialog-btn-danger' : 'sr-dialog-btn-primary',
                function () { d.close(true); });
            setTimeout(function () { ok.focus(); }, 0);
        });
    };

    UI.prompt = function (msg, defaultValue, opts) {
        opts = opts || {};
        var labels = defaultLabels();
        return new Promise(function (resolve) {
            var d = openDialog({
                kind: 'prompt',
                title: opts.title || '',
                danger: !!opts.danger,
                cancelValue: null,
                confirmValue: function () { return input.value; },
                onClose: function (r) { resolve(r === undefined ? null : r); }
            });
            var label = document.createElement('label');
            label.className = 'sr-dialog-msg';
            label.textContent = msg == null ? '' : String(msg);
            var input = document.createElement('input');
            input.type = opts.password ? 'password' : 'text';
            input.className = 'sr-dialog-input';
            input.value = defaultValue == null ? '' : String(defaultValue);
            if (opts.placeholder) input.placeholder = opts.placeholder;
            if (opts.inputMode) input.setAttribute('inputmode', opts.inputMode);
            label.appendChild(input);
            d.body.appendChild(label);
            addButton(d.actions, opts.cancelLabel || labels.cancel, 'sr-dialog-btn-ghost', function () {
                d.close(null);
            });
            addButton(d.actions, opts.okLabel || labels.ok, 'sr-dialog-btn-primary', function () {
                d.close(input.value);
            });
            setTimeout(function () {
                input.focus();
                if (opts.selectAll !== false) { try { input.select(); } catch (e) { /* ignore */ } }
            }, 0);
        });
    };

    // Read-only field + copy button. `text` may contain newlines (several new
    // tokens at once); the field stays a single string.
    UI.showSecret = function (title, text, opts) {
        opts = opts || {};
        var labels = defaultLabels();
        var copyLabel = opts.copyLabel || labels.copy;
        var copiedLabel = opts.copiedLabel || labels.copied;
        return new Promise(function (resolve) {
            var d = openDialog({
                kind: 'secret',
                title: title || '',
                confirmValue: function () { return true; },
                onClose: function () { resolve(); }
            });
            var wrap = document.createElement('div');
            wrap.className = 'sr-secret-row';
            var single = String(text == null ? '' : text).indexOf('\n') === -1;
            // One line → single-line input (easy to select-copy); several tokens →
            // textarea so every line stays visible without horizontal scrolling.
            var field = document.createElement(single ? 'input' : 'textarea');
            if (single) {
                field.type = 'text';
            } else {
                field.rows = Math.min(6, String(text).split('\n').length);
            }
            field.readOnly = true;
            field.className = 'sr-dialog-input sr-secret-field mono';
            field.value = text == null ? '' : String(text);
            field.setAttribute('aria-label', title || 'secret');
            wrap.appendChild(field);
            var copy = addButton(wrap, copyLabel, 'sr-dialog-btn-ghost', function () {
                copyText(field.value, function (ok) {
                    copy.textContent = ok ? copiedLabel : copyLabel;
                    copy.classList.toggle('copied', ok);
                    setTimeout(function () {
                        copy.textContent = copyLabel;
                        copy.classList.remove('copied');
                    }, 1600);
                });
            });
            d.body.appendChild(wrap);
            if (opts.hint) messageEl(d.body, opts.hint);
            addButton(d.actions, opts.okLabel || labels.ok, 'sr-dialog-btn-primary', function () { d.close(true); });
            setTimeout(function () {
                field.focus();
                try { field.select(); } catch (e) { /* ignore */ }
            }, 0);
        });
    };

    // Copy helper: async clipboard API when available, execCommand fallback for
    // non-secure contexts (plain http relay deployments).
    function copyText(text, done) {
        var fallback = function () {
            var ta = document.createElement('textarea');
            ta.value = text;
            ta.style.position = 'fixed';
            ta.style.opacity = '0';
            document.body.appendChild(ta);
            ta.select();
            var ok = false;
            try { ok = document.execCommand('copy'); } catch (e) { ok = false; }
            document.body.removeChild(ta);
            done(ok);
        };
        if (navigator.clipboard && window.isSecureContext) {
            navigator.clipboard.writeText(text).then(function () { done(true); }).catch(fallback);
        } else {
            fallback();
        }
    }

    // Page toast (#toast on the session page). Kept here so both session.js and
    // files.js share one duration policy: errors 6s, everything else 3s.
    UI.toast = function (msg, kind, ms) {
        var el = document.getElementById('toast');
        if (!el) return null;
        kind = kind || 'info';
        el.textContent = msg;
        el.className = 'toast ' + kind;
        if (ms == null) ms = (kind === 'error') ? 6000 : 3000;
        if (UI._toastTimer) clearTimeout(UI._toastTimer);
        UI._toastTimer = setTimeout(function () { el.classList.add('hidden'); }, ms);
        return el;
    };

    UI.theme = theme;
    UI.copyText = copyText;

    window.UI = UI;

    // Bind theme buttons as soon as the DOM is parsed (the <head> snippet has
    // already painted the right palette, so nothing flashes).
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', function () { theme.init(); });
    } else {
        theme.init();
    }
})();
