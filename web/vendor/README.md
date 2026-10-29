# web/vendor — 前端第三方资源（本地化，无 CDN 依赖）

这些文件从 jsDelivr 原样下载，版本与原先页面里引用的 CDN URL **完全一致**。
目的是让 `session` 页和管理后台在内网/断外网的部署环境里也能正常渲染终端
（原先直接引 `cdn.jsdelivr.net`，离线时 `window.Terminal` 为 undefined，
整个终端不可用）。

| 文件 | 来源 URL | 版本 | 许可证 |
| --- | --- | --- | --- |
| `xterm/xterm.js` | https://cdn.jsdelivr.net/npm/xterm@5.3.0/lib/xterm.js | xterm 5.3.0 | MIT |
| `xterm/xterm.css` | https://cdn.jsdelivr.net/npm/xterm@5.3.0/css/xterm.css | xterm 5.3.0 | MIT |
| `xterm-addon-fit/xterm-addon-fit.js` | https://cdn.jsdelivr.net/npm/xterm-addon-fit@0.8.0/lib/xterm-addon-fit.js | xterm-addon-fit 0.8.0 | MIT |
| `xterm-addon-webgl/xterm-addon-webgl.js` | https://cdn.jsdelivr.net/npm/xterm-addon-webgl@0.16.0/lib/xterm-addon-webgl.js | xterm-addon-webgl 0.16.0 | MIT |

## 校验和（sha256）

```
f0aea0f75f48559013ae6643c2479dd737d26da42d5524e6d2b70915ae6523c7  xterm/xterm.js
832f3f2c603b43ad4351ff04970150cc7a873014276db126a6065c6dd81e4872  xterm/xterm.css
10f3194c5f17c1786fb7d5db865c1ec8539b6736a318063fd38bdaaf7c46848f  xterm-addon-fit/xterm-addon-fit.js
0c9c48c9391c4cee816eacf95699dbde97e8cc8f191e87f3a571e73d214c8df8  xterm-addon-webgl/xterm-addon-webgl.js
```

（`sourceMappingURL` 注释仍指向 CDN 的 `.map` 文件；只在浏览器开发者工具里按需
拉取，不影响运行，未做改动以保持与上游字节一致。）

## 引用方式

- 会话页 `web/session.html`：`/vendor/xterm/xterm.css`、`/vendor/xterm/xterm.js`、
  `/vendor/xterm-addon-fit/xterm-addon-fit.js`、`/vendor/xterm-addon-webgl/xterm-addon-webgl.js`
- 管理后台 `src/relay/admin_page.html`（页面挂在 secret 路径下，不能用相对路径，
  故用绝对路径 `/vendor/...`）：`/vendor/xterm/xterm.css`、`/vendor/xterm/xterm.js`

## 打包

`src/web.rs` 的 `rust_embed` 以整个 `web/` 目录为根（`#[folder = "web/"]`），
所以 `web/vendor/**` 会被内嵌进二进制，并由 `static_handler`（含 `fallback`）
按路径原样提供；`tools/verify/verify_mys2283_c.js` 断言这四个 URL 均返回 HTTP 200。

## 升级

改这里时同步改 `web/session.html` / `src/relay/admin_page.html` 的版本号注释性
信息即可（路径不变）。下载后请更新上表的 sha256。

## 许可证

三个包均为 MIT，版权归 xterm.js 作者（The xterm.js authors）。
上游 LICENSE：
- https://github.com/xtermjs/xterm.js/blob/5.3.0/LICENSE
- https://github.com/xtermjs/xterm.js/blob/0.8.0/addons/xterm-addon-fit/LICENSE
- https://github.com/xtermjs/xterm.js/blob/0.16.0/addons/xterm-addon-webgl/LICENSE
