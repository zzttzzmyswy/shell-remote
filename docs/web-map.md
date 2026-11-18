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

1. **安全上下文**：Service Worker 只能在受信证书的 https，或 `http://localhost` / `http://127.0.0.1` 下注册。relay 默认自签证书浏览器不认，完整模式不可用——按下一节配置证书
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

## 自签名证书与完整模式

保留自签名 HTTPS 时，让完整模式工作只有一条路：**让浏览器真正信任 relay 的证书，并且证书里的名字与地址栏里的地址一致**。本节的所有结论都在 Chrome 157 与 Firefox 1495 上实测过（复跑方法见文末）。

### 先认清一件事：“继续前往” ≠ 信任证书

点过证书警告页的「高级 → 继续前往（不安全）」之后，页面能加载、`window.isSecureContext` 也是 `true`，但**Service Worker 仍然注册失败**，控制台报：

```
An SSL certificate error occurred when fetching the script.
```

实测对比（同一张未受信证书）：

| 页面能力 | 点“继续前往”之后 |
|---------|-----------------|
| 页面加载 / SSE / 终端 | 正常 |
| `fetch('/sr-web-sw.js')` | 200（普通请求不校验证书链的信任） |
| `navigator.serviceWorker.register(...)` | **拒绝**：`SecurityError` + 上述文案 |

浏览器对 SW 脚本单独做一次严格校验，这是刻意的：SW 一旦激活就能拦截整个源的请求，所以不接受“用户临时放行”的证书。想用完整模式，就必须真的把证书装进信任库，或者改用受信证书/`http://`。

### 最短修复路径

仓库里**没有**现成的证书签发脚本（按任务范围不新增）。下面两段可以直接复制执行，用系统里的 `openssl`，不需要装新工具。若你更习惯 `mkcert`，它生成的根证书用法与之等价（把 `mkcert -install` 换成下面的“导入根证书”步骤即可）。

**第 1 步：建一个私有根 CA（只做一次，之后复用它给多台 relay 签发）**

```bash
mkdir -p ~/sr-ca && cd ~/sr-ca
# 根私钥：不要外发，权限收紧
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca-key.pem -out ca.pem -days 3650 \
  -subj "/CN=shell-remote private CA" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign"
```

**第 2 步：用实际访问的地址签发服务器证书**

`ADDR` 必须填**你在浏览器里输入的地址**——用公网 IP 访问就填公网 IP，用域名访问就填域名。多写几个进 SAN 可以一次覆盖（下面的写法同时放 IP 与 DNS）。

```bash
ADDR=203.0.113.10                 # ← 改成你实际访问 relay 用的 IP 或域名
cd ~/sr-ca
openssl req -newkey rsa:2048 -nodes -keyout relay-key.pem -out relay.csr -subj "/CN=$ADDR"
cat > relay-ext.cnf <<EOF
subjectAltName=IP:$ADDR,DNS:$ADDR
basicConstraints=CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
EOF
openssl x509 -req -in relay.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial \
  -out relay-cert.pem -days 825 -extfile relay-ext.cnf
openssl x509 -in relay-cert.pem -noout -subject -ext subjectAltName
```

**第 3 步：让 relay 用这张证书**

```bash
./shell-remote relay --bind 0.0.0.0:3000 --auth '你的密码' \
  --tls-cert ~/sr-ca/relay-cert.pem --tls-key ~/sr-ca/relay-key.pem
```

证书路径会写进启动日志的 `HTTPS (self-signed) enabled … note=using certs at …`，可据此确认生效。

**第 4 步：把根证书导入**发起访问的那台机器**的信任库**

> 这一步改的是**你自己的电脑**，不是 relay 服务器，也不是线上服务。导入前请自己核对指纹，见下面的“核对指纹”。

| 系统 / 浏览器 | 做法 | 生效范围 |
|--------------|------|---------|
| Windows（Chrome / Edge / IE） | 双击 `ca.pem` → 安装证书 → **本地计算机** → “受信任的根证书颁发机构” | 该机器的所有用户与该系统的 Chromium 系浏览器；Firefox 见下一行 |
| macOS（Safari / Chrome） | 钥匙串访问 → 系统 → 导入 `ca.pem` → 双击 → 信任 → “始终信任” | 该机器的系统信任库 |
| Linux + Chromium 系 | 拷到 `/usr/local/share/ca-certificates/sr-ca.crt` 后 `sudo update-ca-certificates` | 系统信任库，Chromium 会读取 |
| Firefox（全平台） | 设置 → 隐私与安全 → 证书 → 查看证书 → 证书颁发机构 → 导入 | **只影响 Firefox**，用它自己的库 |

**第 5 步：重启浏览器再打开会话页**。地址栏应显示正常的锁标（而不是“不安全”），此时打开网页视图才是“完整模式”。

### 什么时候可以不走私有 CA

relay 未给 `--tls-cert` 时会自动生成一张自签证书，持久化在 `~/.shell-remote/self-signed/`，SAN 覆盖 `localhost` + 本机主机名 + 本机全部 IPv4。理论上也可以**把这张叶证书本身当信任锚**导入（实测 Chrome 与 Firefox 都能接受，直接信任叶证书后 SW 注册成功）。

但它有两个实际障碍，所以在多数场景下私有 CA 更省事：

- SAN 只含**生成时**那台机器的地址。用公网 IP 或内网 NAT 之外的地址访问就对不上——证书里没有这个名字，浏览器直接不给例外之外的任何余地
- 每台 relay 各生成一张，多设备时要在浏览器里逐个导入，且换机器/重装就要重来

只有在“单台设备 + 用本机地址访问 + 只在一台电脑上打开”时，才建议直接导入 `~/.shell-remote/self-signed/relay-cert.pem`。

### 核对指纹（导入前必做）

自签证书的安全前提是“你导入的确实是这台 relay 的那张证书”。导入前把两边的 SHA-256 指纹对齐：

```bash
# 在 relay 机器上
openssl x509 -in ~/sr-ca/relay-cert.pem -noout -fingerprint -sha256
```

浏览器里点地址栏的锁标/证书信息可以看到同一串指纹。两边一致再导入。指纹不一致说明连到了别的服务或证书被替换过。

### 其他会影响完整模式的情况

- **代理/网关终止 TLS**：如果前置 nginx、Caddy 或云负载均衡在**外层**做 HTTPS 终结再转发给 relay，那么浏览器看到的是**它**的证书，不是 relay 的。此时要么给外层配受信证书（`--tls-cert` 给 relay 就没有意义了），要么让外层直接透传 443 给 relay。在哪一层做 TLS，证书就要配在哪一层
- **代理里跑着别的路径**：`/sr-web-sw.js` 必须由 relay 提供（不能被 SPA fallback 或缓存规则吃掉），否则报 `SW_HTTP_404`；响应类型必须是 `application/javascript`，否则报 `SW_MIME`
- **自签证书的重启与迁移**：证书文件不变则重启不影响已建立的例外；但**换了证书文件**（重新生成、或把数据目录清掉）等于换了一张新证书，之前导入的信任或例外都不再匹配，浏览器会重新告警。迁移 relay 时把 `~/sr-ca/` 一起带走即可保持连续
- **证书有效期**：上面命令给根 CA 3650 天、服务器证书 825 天（这是 Chrome 接受的最长有效期）。服务器证书过期后要重新签发（第 2 步），根 CA 不用动

### 怎么判断是证书问题还是别的问题

网页视图状态区会给出分类后的提示；控制台里也可以自己看一眼：

| 现象 | 结论 |
|------|------|
| 控制台 `An SSL certificate error occurred when fetching the script.`，状态区提示证书 | 证书没被信任，或 SAN 与访问地址不符 → 按上节处理 |
| 地址栏锁标正常但仍是静态模式 | 多半是 ro Token、旧 agent，或浏览器禁用了 SW（隐私/无痕模式）——状态区会写明 |
| 状态区提示 HTTP 404 / 响应类型不对 | 前置代理拦了或改写了 `/sr-web-sw.js`，与证书无关 |
| 只有 Firefox 报 `The operation is insecure.` | Firefox 对策略类失败只给这一句，无法细分；先按证书核对一遍，再排查 SW 是否被策略禁用 |

### 复跑这些结论

`tools/verify/verify_web_map_tls.js` 用临时 CA + 独立浏览器 profile 复跑上面全部判断，不改你机器上的任何信任设置：

```bash
cargo build --bin shell-remote
xvfb-run -a node tools/verify/verify_web_map_tls.js
```

它覆盖：临时 CA 签发的 SAN 匹配证书下 SW 注册成功并激活（完整模式可用）；同一张证书未被信任时注册失败且状态区给证书提示（负例）；证书被信任但访问的 IP 不在 SAN 时的表现；以及只读 Token 的静态模式回退仍然工作。隔离方式是给 Chromium 独立 `HOME`（NSS 库）与独立 profile，不用 `--ignore-certificate-errors`、也不经 CDP 忽略证书错误。

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
- 非受信证书的 https relay（默认自签）无法注册 SW → 完整模式不可用，见上文“自签名证书与完整模式”
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

HTTPS 与证书信任（临时 CA + 独立浏览器信任库，不改本机设置）：SAN 匹配且被信任时完整模式可用、未受信时准确提示、SAN 不匹配的表现。

```bash
cargo build --bin shell-remote
xvfb-run -a node tools/verify/verify_web_map_tls.js
```
