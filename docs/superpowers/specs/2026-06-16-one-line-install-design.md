# shell-remote: One-Line Agent Install + GitHub Link

**Date:** 2026-06-16
**Status:** Approved

## Motivation

Current homepage has a verbose Agent guide and per-architecture download cards. Replace them with a single `curl | sh` command that auto-detects architecture, selects the best GitHub download source (with China mainland proxy fallback), downloads to memory-backed filesystem, and executes with zero residual files. Add a GitHub link in the top-right corner.

## Homepage Redesign

### Before
```
┌──────────────────────────────┐
│  shell-remote                │
│  连接表单                     │
│  Agent Mode Guide (长说明)    │
│  Download Cards (3张卡片)     │
└──────────────────────────────┘
```

### After
```
┌──────────────────────────────┐
│  shell-remote        [GitHub]│
│  远程协作终端                  │
│  连接表单                     │
│  一键安装命令 (1行)           │
└──────────────────────────────┘
```

### Changes to `web/index.html`

**Delete:**
- Agent Mode Guide HTML block (currently `guide-section`)
- Download Section HTML block (currently `download-section`)
- Download cards JavaScript logic

**Add:**
- GitHub link icon (top-right corner, absolute positioned)
- Install section with single `curl | sh` command + copy button

## Install Script Endpoint

### Relay route

```
GET /agent/install → install_script_handler
```

Returns a shell script with the relay URL baked in.

### Script embedding

`web/install.sh` embedded via `include_str!` in `src/relay/mod.rs`. Build-time replacement of `__RELAY_URL__` placeholder with the actual relay host (from request `Host` header + `X-Forwarded-Proto`).

### Script logic

1. **Architecture detection**: `uname -m` → map to `x86_64` / `aarch64` / `armv7`
2. **Temp directory selection**: prefer `/dev/shm` (memory-backed), fallback to `/tmp`
3. **Download with fallback chain**: try URLs in order:
   - `https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-{arch}`
   - `https://edgeone.gh-proxy.com/https://github.com/...`
   - `https://hk.gh-proxy.com/https://github.com/...`
   - `https://gh-proxy.com/https://github.com/...`
   - `https://gh.llkk.cc/https://github.com/...`
   - (extensible list)
4. **Download to temp**: `curl --connect-timeout 5 --max-time 30 -o $BIN`
5. **Execute**: `chmod +x $BIN; exec $BIN agent --relay-url $RELAY_URL "$@"`
6. **Cleanup**: `trap cleanup EXIT INT TERM` → `rm -f $BIN` on exit

### Script behavior

- `set -e` — fail on any error
- Each curl source tried sequentially; first success wins
- If all sources fail → exit 1 with "download failed"
- If `/dev/shm` not writable → fallback to `/tmp`
- User signals (Ctrl+C) → trap fires cleanup
- Supports `"$@"` passthrough for agent arguments (e.g., `--key`, `--token-type`)

## GitHub Link

- Position: top-right corner, absolute positioning
- Content: GitHub Mark SVG icon (inline, no external request)
- Link: `https://github.com/zzttzzmyswy/shell-remote` (new tab)
- Styling: opacity 0.7, hover → opacity 1, transition

## Error Handling

| Scenario | Behavior |
|---|---|
| Unsupported arch | `exit 1` with error message |
| All download sources fail | `exit 1` with "download failed" |
| `/dev/shm` unwritable | Auto-fallback to `/tmp` |
| curl timeout | Try next source |
| Download partial/corrupt | Script exits, trap removes partial file |
| User cancels (Ctrl+C) | trap cleans up binary |
| Relay unreachable | curl reports connection error |

## Files Affected

| File | Action |
|---|---|
| `web/index.html` | Delete guide+download sections; add install section + GitHub link |
| `web/style.css` | Add styles for `.install-section`, `.install-cmd`, `.github-link` |
| `web/install.sh` | New file — self-contained install script |
| `src/relay/mod.rs` | Add `install_script_handler` + route registration; update existing test for route count |
| `build.rs` | No change needed (already watches `web/` directory) |

## Testing

- Test `install_script_handler` returns 200 with correct content-type
- Test `__RELAY_URL__` replaced with correct host in response
- Test script architecture mapping: x86_64→x86_64, aarch64→aarch64, armv7l→armv7
- Update `test_relay_router_builds_without_error` for new route
- Update `test_all_web_assets_accessible` for `install.sh`
- Manual smoke test: `curl localhost:3000/agent/install | head -10` verifies bash script structure
