# CivitaiProxy

用 Rust 编写的 [Civitai](https://civitai.com) 全功能反向代理。不只是代理网页，还把 Civitai 的
**REST API、模型下载（含 307 跳转到的 R2/B2 存储）、图片 CDN、tRPC、Next.js 资源、WebSocket**
全部映射到你自己的域名下，A1111 / ComfyUI 插件、aria2、wget 等工具只需替换域名即可使用。

服务只在本地非标端口提供 HTTP（默认 `127.0.0.1:8787`），TLS 由前置的 Caddy / CDN 负责。

## 特性

- **两种部署模式**
  - `single` 单域名：`civitai.example.com`
    - `civitai.com` → `https://civitai.example.com/`
    - 其它主机 → `https://civitai.example.com/__h/<上游主机>/...`（如 `/__h/image.civitai.com/...`）
  - `wildcard` 泛域名：`*.example.com`
    - `civitai.com` → `example.com`，`<sub>.civitai.com` → `<sub>.example.com`（image、signals 等自动覆盖）
    - `civitai.red` / `civitai.green` → `civitai-red.example.com` / `civitai-green.example.com`
    - 存储等外部主机 → `ext.example.com/<上游主机>/...`
- **URL 全量改写**：HTML / JS / CSS / JSON / tRPC / `_next/data` / RSC 中的
  `https://host`、`//host`、`https:\/\/host`（JSON 转义）、`https%3A%2F%2Fhost`（URL 编码）及裸主机名；
  `Location`、`Link`、`Set-Cookie Domain`、CORS 头；WebSocket 文本帧。
- **请求方向还原**：`Origin` / `Referer`、查询参数和 JSON/表单请求体中的代理域名会还原为上游域名。
- **模型下载**：`/api/download/models/{id}` 的 307 预签名地址被改写为代理地址，客户端自动跟随后
  仍经由代理；预签名 query 原样保留，`Range` 断点续传、`Content-Disposition` 文件名透传，
  纯流式不落盘。发往存储主机的请求会去掉 `Authorization` / `Cookie`（避免 S3 签名冲突）。
- **API Key**：`Authorization: Bearer <key>` 与 `?token=<key>` 原样透传给 civitai。
- **压缩**：上游 gzip / br / deflate 自动解压后改写，压缩交给 Caddy 的 `encode`。
- **防开放代理**：只允许访问 `upstream_allow` 列表中的主机。
- **可选 IP 白名单**：支持 CIDR、过期时间、JSON 持久化；提供 HTTP 接口和 CLI，
  一个带 token 的 GET 请求即可为当前 IP 加白。

## 快速开始

```bash
cargo build --release
cp config.example.toml config.toml   # 修改 mode / public_domain 等
./target/release/civitai-proxy -c config.toml serve
```

然后按 `Caddyfile.example` 配置 Caddy（单域名 / 泛域名两种写法都有）。也可使用
`Dockerfile` 或 `civitai-proxy.service`（systemd）部署。

### 使用示例（单域名 `civitai.example.com`）

```bash
# 公共 API：返回中的 downloadUrl / 图片地址均已是代理域名
curl 'https://civitai.example.com/api/v1/models?limit=1'

# 下载模型（需要登录的模型带上 API Key）
curl -L -OJ -H "Authorization: Bearer $CIVITAI_API_KEY" \
  https://civitai.example.com/api/download/models/128713
aria2c -x8 "https://civitai.example.com/api/download/models/128713?token=$CIVITAI_API_KEY"
```

在 A1111 / ComfyUI 等插件里，把 `https://civitai.com` 替换为 `https://civitai.example.com`
（泛域名模式为 `https://example.com`）即可。

## 访问控制（IP 白名单）

```toml
[access]
enabled = true
admin_token = "一个足够长的随机串"
default_ttl_secs = 0              # 0 = 永久
trusted_proxies = ["127.0.0.1/32", "::1/128"]
client_ip_header = "X-Forwarded-For"
```

开启后，非白名单 IP 访问会得到 403。`/__cp/*` 管理接口不受白名单限制（但需要 token）：

| 接口 | 说明 |
| --- | --- |
| `GET /__cp/allow?token=T` | 把**当前请求的 IP** 加入白名单 |
| `GET /__cp/allow?token=T&ip=1.2.3.0/24&ttl=86400&note=xx` | 加白指定 IP/CIDR，可选有效期（秒）和备注 |
| `GET /__cp/deny?token=T[&ip=...]` | 移除（默认移除当前 IP） |
| `GET /__cp/list?token=T` | 列出白名单 |
| `GET /__cp/ip` | 查看服务端识别到的你的 IP 及是否已加白 |
| `GET /__cp/health` | 健康检查 |

token 也可以放在 `X-Admin-Token` 请求头里。例如在手机浏览器打开一次
`https://civitai.example.com/__cp/allow?token=...&ttl=86400`，当前网络即可使用 24 小时。

CLI（服务运行中时通过本地管理接口操作，保证内存与文件一致；服务未运行时直接编辑白名单文件）：

```bash
civitai-proxy -c config.toml allow 1.2.3.4 --ttl 3600 --note phone
civitai-proxy -c config.toml allow 10.0.0.0/8
civitai-proxy -c config.toml deny 1.2.3.4
civitai-proxy -c config.toml list
civitai-proxy -c config.toml config   # 打印生效配置
```

真实客户端 IP 只在 TCP 对端属于 `trusted_proxies` 时才从 `client_ip_header` 读取，
防止伪造。Caddy 与本服务不在同一主机/容器时，请把 Caddy 的地址加入 `trusted_proxies`。

## 登录

- **API Key**（推荐）：在 civitai 用户设置中生成，用 `Authorization: Bearer` 或 `?token=` 传递。
- **网页登录**：`Set-Cookie` 会被改写到代理域名，邮箱登录可用（邮件里的链接需把
  `civitai.com` 手动替换成你的域名）。
- **Discord / Google / GitHub 等 OAuth 登录**：回调地址绑定 civitai.com，无法经代理完成。
  请先直接登录 civitai.com，在浏览器开发者工具中复制 `__Secure-civitai-token` Cookie 的值，
  再打开 `https://你的域名/__cp/login` 粘贴导入。

## 已知限制

- 若上游触发 Cloudflare 人机验证，代理无法绕过。
- WebSocket 上游连接不走 `upstream_proxy`（直接连接）。
- 不做缓存；如需缓存图片，可在前置 CDN 上配置。

## 开发

```bash
cargo test      # 单元测试 + 基于本地 mock 上游的端到端测试
cargo clippy --all-targets
```

调试时可用 `upstream_resolve` 把某个上游主机指向本地服务（Host 头保持为上游主机）：

```toml
[upstream_resolve]
"civitai.com" = "http://127.0.0.1:9000"
```
