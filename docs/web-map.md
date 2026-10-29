# 网页映射

会话页工具栏的"网页"按钮打开一个独立视图，经 relay → agent 隧道访问**远端设备网络环境**中的网页——包括只监听 `127.0.0.1`、外部无法直接访问的设备自身服务。

## 两种模式

按 agent 能力、Token 权限与浏览器安全上下文自动选择：

| 模式 | 条件 | 能力 |
|------|------|------|
| **v2 完整模式** | agent 声明 `net:tcp-v1` + **rw** Token + 安全上下文（受信证书的 https，或 `http://localhost` / `http://127.0.0.1`） | 远端脚本执行、SPA history 路由、Cookie 登录、任意方法与请求体、表单提交、WebSocket、下载 |
| **v1 静态模式** | agent 只声明 `web:proxy-v1`（v0.58.x），或 v2 条件不满足但 v1 可用 | 静态/轻量页面：远端 JS 不执行，只转发单次 GET |
| 无入口 | agent 两个能力都不声明（≤ v0.57.0） | — |

网页视图地址栏右侧显示当前模式徽标（"完整模式" / "静态模式"）；降级时提示区写明原因（只读 Token / 非安全上下文 / 无 Service Worker / agent 版本旧）。

## 能力协商

入口与模式完全由 `session:capabilities` 里声明的能力决定，不按版本号猜测：

- agent 注册时在 `capabilities` 里声明能力；relay 在浏览器 SSE 握手时补发 `session:capabilities {capabilities:[...]}`
- 浏览器侧判定在 `web/webview.js` 的 `detectMode`（纯函数，可单测）：
  1. 含 `net:tcp-v1`，且 `sessionStorage['shell-remote-permission'] === 'rw'`，且 `window.isSecureContext && 'serviceWorker' in navigator` → **v2**
  2. 否则含 `web:proxy-v1` → **v1**（同时记录降级原因）
  3. 都没有 → 入口隐藏
- 每次重连都会重新收到该事件，所以能力消失（agent 回退版本）会立刻收起入口
- 收到 `session:agent_disconnect`（agent 断线）时立即隐藏入口、退出网页视图、清空 iframe，并通知 Service Worker 丢弃该会话的页面归属；重连后以新的能力数组为准

## v2 完整模式

### 接口

| 路径 | 方法 | 鉴权 | 说明 |
|------|------|------|------|
| `/agent/web/map` | POST | `X-SR-Token` + `X-Auth` | 建（或复用）映射，body `{"url":"<绝对 http/https URL>"}`，返回 `{map_id, base, target_origin, expires_in}` |
| `/agent/web/m/<map_id>/…`（含无尾斜杠的 `/agent/web/m/<map_id>`） | ANY | 无（`map_id` 本身就是访问凭据） | 反代：透传方法、头、body、Cookie；`Upgrade: websocket` 时透传 101 |
| `/sr-web-sw.js` | GET | 无 | Service Worker 脚本（scope `/`） |
| `/sr-web-shim.js` | GET | 无 | 注入到映射页面的脚本 |

- 同一会话同一目标复用同一 `map_id`（`expires_in` 是映射的空闲 TTL，默认 1800s）；每会话最多同时存在 8 个映射，超出淘汰最久未用的
- 状态码：ro Token `403`、agent 无 `net:tcp-v1` `404 {"error":"unsupported"}`、agent 离线 `503`、URL 非法 `400`
- 带 `Service-Worker: script` 头经反代请求 → `403`（禁止远端页面借映射路径注册 SW）
- 响应改写：`Location` 加映射前缀；`Set-Cookie` 去 `Domain`、`Path=/` 保持 `/`、其他 `Path` 加前缀；剥掉 CSP / XFO / HSTS / `Clear-Site-Data` / COEP / COOP / CORP / `Alt-Svc` / `Service-Worker-Allowed`
- 反代只搬字节：agent 侧是纯 TCP 管道，不解析 HTTP、不改写响应体

### 浏览器侧路由（Service Worker）

`web/sr-web-sw.js` 在安全上下文注册（scope `/`），把映射页面的请求改写到反代路径。**映射页面在 iframe 中以目标站原始路径运行**（`location.pathname` 里没有 `/agent/web/m/<id>` 前缀），因此根路径资源、history 路由、`<a href="/x">` 整页跳转都按目标站点的写法工作。

fetch 事件分流（判定"不属于映射"时**直接 `return`、不 `respondWith`**，shell-remote 自身的页面 / SSE / 上传零影响）：

1. 保留路径 `/sr-web-sw.js`、`/sr-web-shim.js` 永远放行
2. **导航请求**只处理 `destination` 为 `iframe` / `frame` 的；顶层 `document` 导航一律放行，绝不劫持会话页自己
   - 路径以 `/agent/web/m/<id>/` 开头（会话页首次进入，或 relay 改写后的 3xx `Location`）→ 登记 `pendingNav[去前缀的绝对 URL]`（10s 过期）后 `302` 到该 URL
   - 其他 iframe 导航按 `pendingNav` → `clientId` / `replacesClientId` 归属 → `lastActive`（30 min 内）的顺序确定 `map_id`，没有就放行；命中后向 `/agent/web/m/<id>` + 原始 pathname + query 发 fetch（`redirect:'manual'`）
   - HTML 响应**按字节**在第一个 `<head>` 标签的 `>` 之后插入 `<script src="/sr-web-shim.js?m=…&o=…">`；前 64 KiB 内找不到 `<head>` 就插在最前面。全程用 `TransformStream` 处理字节流，**不做 TextDecoder 解码**——内网设备的 GBK 页面解码再编码会损坏中文
3. **非导航请求**只处理来自映射页面的（`clientId` 在归属表里）：同源 URL 或 host 等于 `target_origin` 的 host（兼容浏览器把 http 自动升级成 https）→ 加映射前缀后重发；其他跨源放行

状态（`maps` / `clients` / `pendingNav` / `lastActive`）全部落 IndexedDB（库 `sr-web`），并按"clientId 已不在 `clients.matchAll()`"与"超过 24h"定期清理——Service Worker 随时可能被浏览器回收，内存状态不可依赖。

### 注入脚本

`web/sr-web-shim.js` 由 SW 插在页面第一个 `<head>` 之后同步执行，补齐 SW 管不到的部分：

- **WebSocket**：`window.WebSocket` 换成子类（保持 `instanceof`、静态常量、`protocols` 参数）。host 等于本站 host（且未带映射前缀）或等于 `target_origin` 的 host → 改写为 `<ws|wss>://<本站 host>/agent/web/m/<id>` + pathname + query；其他源原样。`ws` / `wss` 由**本页协议**决定（同源方案下 TLS 由 relay 在隧道上做）
- **fetch / XMLHttpRequest.open / EventSource**：只把指向 `target_origin` 的**绝对** URL 改成同源相对路径（`pathname + query`），之后由 SW 接手。相对的与 `location.host` 的写法本来就走 SW，不动
- **Service Worker 防护**：`navigator.serviceWorker` 的 `register` / `unregister` / `update` 返回 rejected Promise（`service workers are disabled inside shell-remote web mapping`），`getRegistrations` → `[]`，`getRegistration` → `undefined`，用 `Object.defineProperty` 定在实例上（不改原型——同源方案下改原型会连会话页自己的注册一起废掉）。远端页面因此注销不掉我们的 SW，也抢不走本站源的请求代理权
- 自包含 IIFE，不依赖 shell-remote 的全局变量，不读写 `sessionStorage` / `localStorage`

## v1 静态模式

| 路径 | 方法 | 鉴权 | 说明 |
|------|------|------|------|
| `/agent/web/ticket` | POST | `X-SR-Token` + `X-Auth` | 签发绑定会话的短期凭据，返回 `{ticket, expires_in}`（默认 600s） |
| `/agent/web/proxy?ticket=…&url=…` | GET | 仅 ticket（签发时已鉴权） | 取远端页面；响应头 `X-SR-Final-URL` 给出重定向后的最终地址 |

- 只支持 GET；`url` 必须是绝对 `http://` / `https://` 地址，长度 ≤ 4096
- **ticket 与会话 Token 完全独立**：256 bit CSPRNG 随机、不可从 Token 推导、每会话最多 8 个（超出按最旧淘汰）、默认 10min 过期、agent 断线时全部撤销。ticket 只出现在代理 URL 的 query 里，**会话 Token 与服务器口令从不进入任何 URL**
- 状态码：无效会话 Token `401`、无效/过期 ticket `403`、agent 无该能力 `404`、agent 离线 `503`、并发超限 `429`、上游超时 `504`、上游连接失败 `502`
- ro Token 也能浏览（页面 GET 不改变远端状态）

页面加载策略：relay 只做流式转发，重写全部在浏览器端 `web/webview.js`——用 `X-SR-Final-URL`（与文档内 `<base href>`）作相对 URL 基准 → 重写 `src` / `href` / `poster` / `srcset` / `style` 与 CSS `url()` / `@import` 指向同源代理 URL → 外部样式表取回后内联 → 删除 `<script>`、远端 CSP / refresh `<meta>` 与 `<base>` → 结果放进 `iframe[srcdoc]`，sandbox 只用 `allow-same-origin`（**不给 `allow-scripts`**，远端 JS 一律不执行）。站内链接点击被父页面拦截后改走同一代理；表单提交被拦下并提示。

## 部署要求

1. **安全上下文**：Service Worker 只能在受信证书的 https，或 `http://localhost` / `http://127.0.0.1` 下注册。用 relay 默认自签证书访问时浏览器不认，自动回落到 v1 静态模式
2. **前置 nginx 必须透传 WebSocket 并关闭缓冲**（否则 WS 握手 400、HTML 注入因缓冲而延迟）：

```nginx
location /agent/web/m/ {
    proxy_pass http://127.0.0.1:3000;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_buffering off;
}
```

`/sr-web-sw.js` 与 `/sr-web-shim.js` 走普通静态路径即可，但**不要**加 `Cache-Control: immutable` 长缓存——SW 脚本用 `updateViaCache: 'none'` 注册，服务端按内容 ETag + `no-cache` 返回。

## 安全说明

- 映射页面与 shell-remote **同源**运行（不使用独立域名/端口，是明确的取舍）。因此远端页面的脚本**能读到会话 Token 与服务器口令**（`sessionStorage`），等同于把该设备的 shell 交给它。**只访问可信设备**
- 完整映射能提交表单、修改路由器等设备配置，因此**只对 rw Token 开放**；ro Token 继续用 v1 静态模式
- relay 侧禁止远端页面经映射路径注册 Service Worker（`Service-Worker: script` → 403），并剥掉会作用到本站源或挡住同源嵌入的响应头
- 反代不外发 `X-SR-*` 凭据头与前置反代添加的客户端 IP 头（`X-Forwarded-*` / `X-Real-IP` / `Forwarded`）

## 使用示例

会话页点"网页" → 地址栏填设备侧地址 → "打开"。地址在**远程设备侧解析**，因此可以填：

```
http://127.0.0.1
http://127.0.0.1:8080/admin
http://192.168.1.50/status
https://internal.example.local
```

v2 打开页面后，地址栏显示的是**目标站点的原始地址**（随页面内路由与跳转更新）；"刷新"执行 iframe 内部 reload，保留当前路由与登录态。直接在地址栏输入另一个源的地址会为该源新建一个映射。

## 已知限制

v2（完整模式）：

- **同一浏览器多个标签页同时映射不同设备时，整页跳转只能尽力归属**（按 `pendingNav` → clientId → 最近活跃映射的顺序），可能落到另一个标签页的映射上。子资源与 `fetch` 按页面（clientId）准确归属，不受影响
- 页面里的 `window.open` 弹窗与**顶层**跳转不经过映射（SW 明确不劫持顶层导航）
- 写死目标 IP 的静态资源（`<img src="http://192.168.0.1/x.png">`）会被浏览器的混合内容策略拦截；相对地址与 `location.host` 的写法不受影响
- 非受信证书的 https relay（默认自签）无法注册 SW → 自动回落 v1
- 目标站自身注册 Service Worker 会被 shim 屏蔽（同源方案下的必然取舍）

v1（静态模式）：

- 远端 JavaScript 不执行，依赖 JS 渲染的 SPA、懒加载、动态拼出来的 URL 都不工作
- WebSocket / SSE 等长连接、非 GET 表单、文件下载不支持
- 自签证书 / 双向 TLS 的目标站会被 agent 的 HTTP 客户端拒绝（rustls 默认根证书）
- 单请求 5min 总时限、块间 30s 空闲超时；单页并发子资源每会话上限 8 个在途请求，超出返回 429
- 已内联的样式表上限 8 个 / 单个 512 KiB，主文档上限 8 MiB；超限时提示而不静默显示半页

## 验证

纯函数单测（node，无框架）：SW 的 URL 判定 / 前缀剥离 / `<head>` 字节注入 / WebSocket 改写。

```bash
node tools/verify/test_sr_web.js
node tools/verify/test_webview.js
```

端到端验证（真 relay + 真 agent + 只监听 `127.0.0.1` 的测试 SPA，无头 Chromium）：SPA 渲染、history 路由、Cookie 登录、整页跳转、302、WebSocket echo 与推送、绝对地址 fetch、GBK 页面、页面尝试注销 SW、SW 安装后 shell-remote 自身回归、ro / v0.58 / v0.57 三种降级、agent 断线。

```bash
cargo build --bin shell-remote
xvfb-run -a node tools/verify/verify_web_map_ui.js
```
