# CivitaiProxy

用 Rust 编写的多站点全功能反向代理，内置 **Civitai、Hugging Face、GitHub、GitHub Container
Registry（ghcr.io）、Docker Hub** 五个站点预设，以及一个覆盖 **OpenAI、Anthropic、Gemini（AI Studio）、
Vertex AI、OpenRouter 等 40 多家 AI API** 的网关预设。不只是代理网页，还把各站点的 **API、文件/模型下载
（含 307 跳转到的 S3/R2/CDN 预签名地址）、Git 克隆、Xet 存储、容器镜像仓库、WebSocket**
全部映射到你自己的域名下。客户端工具只需替换域名（或设置 `HF_ENDPOINT`、镜像加速地址）即可使用。

服务只在本地非标端口提供 HTTP（默认 `127.0.0.1:8787`），TLS 由前置的 Caddy / CDN 负责。
所有站点共用一个端口，按 `Host` 头区分。

## 各站点能做什么

| 站点 | 示例域名 | 用法 |
| --- | --- | --- |
| Civitai | `civitai.example.com` | 网页、`/api/v1/*`、`/api/download/models/{id}`（API Key 透传），A1111/ComfyUI 插件替换域名即可 |
| Hugging Face | `hf.example.com` | `HF_ENDPOINT=https://hf.example.com`；`huggingface_hub` / `hf download` / `transformers` / `git clone` / Git LFS / **Xet** 下载；网页 |
| GitHub | `gh.example.com` | 网页、`git clone https://gh.example.com/owner/repo`、Release 下载、raw 文件、`api.github.com`；兼容 ghproxy 写法 `https://gh.example.com/https://github.com/...` |
| GHCR | `ghcr.example.com` | `docker pull ghcr.example.com/owner/image` |
| Docker Hub | `docker.example.com` | `docker pull docker.example.com/nginx`（自动补 `library/`），或配置为镜像加速 `registry-mirrors`；Hub 网页 |
| AI API 网关 | `ai.example.com` | `https://ai.example.com/openai/v1`、`/anthropic`、`/gemini`、`/vertex-<地域>` 等，见下文 [AI API 网关](#ai-api-网关) |

## 工作原理

- **域名映射**：每个站点一个公开域名，两种模式：
  - `single`：上游根站（如 `huggingface.co`）映射到域名根路径，其它上游主机走
    `/__h/<上游主机>/...`（如 `/__h/us.aws.cdn.hf.co/...`）。
  - `wildcard`：另需 `*.<域名>` 的解析和证书。`<子域>.<上游根域>` 映射到 `<子域>.<域名>`，
    别名如 `x.hf.space` 映射到 `x--space.hf.example.com`，其它主机走 `ext.<域名>/<上游主机>/...`。
  - 任意路径也可写成 `/https://<上游主机>/...`（ghproxy 风格）。
- **URL 改写**：HTML / JS / CSS / JSON / tRPC 中出现的**所有已配置站点**的 URL 都会被改写，
  包括 `https://host`、`//host`、JSON 转义写法 `https:\/\/host`、URL 编码写法 `https%3A%2F%2Fhost` 和裸主机名。
  跨站链接同样处理，例如 Docker Hub 页面引用的 GitHub 图片会走 GitHub 站点。
  响应头里带 URL 的也会改写，例如 `Location`、`Link`、`WWW-Authenticate` 的 realm、
  `X-Xet-Cas-Url`、`Set-Cookie` 的 Domain。HTML 中的 SRI `integrity` 属性会被移除。
- **请求方向还原**：`Origin`、`Referer`、查询参数、JSON/表单请求体里的代理域名会还原为上游域名。
- **内容原样透传**：文件内容、镜像 manifest/layer、Git 数据、Release 资源、HF `resolve` / `raw`
  文件绝不改写，保证哈希、`ETag`、`Content-Length` 和 Docker digest 一致。
  这些下载都是纯流式，不落盘，支持 `Range` 断点续传。
- **凭据控制**：`Cookie` / `Authorization` 只发往该站点的 `credential_hosts`。
  带签名参数的预签名 URL（`X-Amz-Signature`、`Signature`、`sig`、`verify` 等）一律不带凭据，
  避免 token 泄露到 CDN，也避免 S3 报“多种认证方式”错误。
- **HF 单域名兼容**：`huggingface_hub` 会跟随同主机的跳转，并从最终响应里读取
  `X-Repo-Commit`、`X-Linked-*`、`X-Xet-Hash` 和 Xet 的 `Link` 头。代理会把这些头
  从跳转响应带到跳转目标的响应上（`carry_headers`）。
- **Docker**：Hub 网页和 registry 共用一个域名，`/v2/` 下的 registry API 按路径路由到
  `registry-1.docker.io`，其余请求（包括 Hub 网页 API）走 `hub.docker.com`。
  Token realm 会被改写回代理地址。
- **防开放代理**：只允许访问各站点 `allow` 列表中的主机。

## 快速开始

```bash
cargo build --release
cp config.example.toml config.toml   # 修改各站点 public_domain，删掉不需要的站点
./target/release/civitai-proxy -c config.toml serve
```

按 `Caddyfile.example` 配置 Caddy。也可以用 `Dockerfile` 或 `civitai-proxy.service`（systemd）部署。
只想用环境变量时：

```bash
CP_SITES="civitai=civitai.example.com,huggingface=hf.example.com,github=gh.example.com,ghcr=ghcr.example.com,dockerhub=docker.example.com" \
  civitai-proxy serve
```

（在域名后加 `:wildcard` 可启用泛域名模式。）没有配置 `[[sites]]` 时，旧版的顶层
`mode` / `public_domain` 配置仍然作为单个 Civitai 站点生效。

## Docker 镜像

每次合入 `main` 或推送 `v*` tag，GitHub Actions 会自动构建 amd64 和 arm64 双架构镜像，发布到
`ghcr.io/lianzizhou/civitaiproxy`。PR 上的流水线只构建，不推送。

| 标签 | 来源 |
| --- | --- |
| `latest`、`edge` | `main` 分支最新提交 |
| `1.2.3`、`1.2` | tag `v1.2.3` |
| `sha-<短哈希>` | 每次发布的提交 |

```bash
mkdir -p civitai-proxy && cp config.example.toml civitai-proxy/config.toml   # 按需修改
docker run -d --name civitai-proxy --restart unless-stopped \
  -p 127.0.0.1:8787:8787 -v "$PWD/civitai-proxy:/data" \
  ghcr.io/lianzizhou/civitaiproxy:latest
```

容器内配置文件为 `/data/config.toml`，白名单默认也存放在 `/data`。在容器里运行时，请求来源
是 Docker 网桥地址，而不是 `127.0.0.1`，所以要把 Caddy 的来源地址（例如 `172.16.0.0/12`）
加入 `[access].trusted_proxies`，否则拿不到真实客户端 IP。

## 客户端用法

```bash
# Civitai
curl -L -OJ -H "Authorization: Bearer $CIVITAI_API_KEY" https://civitai.example.com/api/download/models/128713

# Hugging Face
export HF_ENDPOINT=https://hf.example.com
hf download openai-community/gpt2            # 或 huggingface_hub / transformers / diffusers
git clone https://hf.example.com/openai-community/gpt2

# GitHub
git clone https://gh.example.com/cli/cli
curl -LO https://gh.example.com/cli/cli/releases/download/v2.60.0/gh_2.60.0_linux_amd64.tar.gz
curl -LO https://gh.example.com/https://raw.githubusercontent.com/cli/cli/trunk/README.md

# Docker Hub / GHCR
docker pull docker.example.com/nginx              # = docker.io/library/nginx
docker pull docker.example.com/bitnami/redis
docker pull ghcr.example.com/homebrew/core/hello:2.12.1
# 或在 /etc/docker/daemon.json 中设为镜像加速：
#   { "registry-mirrors": ["https://docker.example.com"] }
```

私有资源照常带凭据：Civitai API Key、`HF_TOKEN`、GitHub token
（`Authorization: token ...`，同样适用于私有仓库的 raw 文件），以及
`docker login docker.example.com` 或 `docker login ghcr.example.com`（凭据会转发给上游认证服务）。

## AI API 网关

`preset = "ai"` 的站点把各家 AI API 放在同一个域名下，按路径前缀区分。打开网关首页
（`https://ai.example.com/`）可以看到完整列表；请求时带 `Accept: application/json` 则返回 JSON。

- **纯透传**：代理不保存任何 API Key。各家的 `Authorization`、`x-api-key`、`api-key`、`x-goog-api-key`
  和 `?key=` 都原样转发。
- **不碰内容**：提示词、模型输出、SSE 流、WebSocket 帧、查询参数一律不改写。只有少数会返回
  "需要客户端再去下载的 URL" 的接口例外，这些 URL 会被映射回代理：Anthropic 批处理的
  `results_url`、Gemini 上传会话的 `X-Goog-Upload-URL` 和 `file.uri`、OpenAI 图片的 `url`，
  以及 Replicate、fal、BFL、Ideogram 生成结果的地址。
- **流式**：SSE 和音频流逐块转发，不缓冲。AI 服务的上游空闲超时为 1 小时，长时间推理不会被掐断。
- **WebSocket**：支持 OpenAI Realtime、Gemini Live、ElevenLabs、Deepgram、AssemblyAI、Cartesia。
- **gRPC**：Vertex AI / Gemini 的 gRPC 调用通过 HTTP/2 透传，trailers 完整保留。

| 分类 | 前缀 → 上游 |
| --- | --- |
| 主流 | `/openai` → api.openai.com · `/anthropic` → api.anthropic.com · `/gemini` → generativelanguage.googleapis.com · `/vertex` → aiplatform.googleapis.com，`/vertex-<地域>` → `<地域>-aiplatform.googleapis.com`，`/google-oauth` → oauth2.googleapis.com · `/openrouter` → openrouter.ai |
| 云厂商 | `/azure-<资源名>` → `<资源名>.openai.azure.com`，`/azure-cog-<资源名>` → `.cognitiveservices.azure.com`，`/azure-ai-<资源名>` → `.services.ai.azure.com` · `/bedrock-<地域>` → `bedrock-runtime.<地域>.amazonaws.com`，`/bedrock-mantle-<地域>` → `bedrock-mantle.<地域>.api.aws` |
| OpenAI 兼容等 | `/xai` `/mistral` `/deepseek` `/groq` `/together` `/fireworks` `/cerebras` `/perplexity` `/cohere` `/nvidia` `/sambanova` `/hyperbolic` `/novita` `/github-models` `/hf-router` `/parallel` |
| 国内 | `/moonshot`（国际 `/moonshot-intl`）· `/zhipu`（Z.ai `/zai`）· `/dashscope`（国际 `/dashscope-intl`）· `/siliconflow`（国际 `/siliconflow-intl`）· `/minimax`（国际 `/minimax-intl`） |
| 语音 | `/elevenlabs` `/deepgram` `/assemblyai`（实时 `/assemblyai-rt`）`/cartesia` |
| 图像 / 视频 | `/replicate` `/fal`（队列 `/fal-queue`）`/stability` `/ideogram` `/bfl` `/runway` `/luma` |
| 向量 / 搜索 | `/voyage` `/jina`（`/jina-reader`、`/jina-search`） |

每个服务商也可以作为独立站点用在自己的域名上，例如 `preset = "openai"` + `public_domain = "openai.example.com"`。
`providers = [...]` 可以只开放部分服务商。

### SDK 用法

```python
# OpenAI（以及所有 OpenAI 兼容服务：把 /openai/v1 换成 /deepseek/v1、/groq/openai/v1 ...）
from openai import OpenAI
client = OpenAI(base_url="https://ai.example.com/openai/v1")

# Anthropic
import anthropic
client = anthropic.Anthropic(base_url="https://ai.example.com/anthropic")

# Gemini（AI Studio）
from google import genai
client = genai.Client(api_key=KEY, http_options={"base_url": "https://ai.example.com/gemini"})

# Vertex AI（google-genai，REST）
client = genai.Client(vertexai=True, project=PROJECT, location="us-central1",
                      http_options={"base_url": "https://ai.example.com/vertex-us-central1"})

# Vertex AI（google-cloud-aiplatform，默认 gRPC）：gRPC 不能带路径前缀，需要独立的 vertex 站点，
# wildcard 模式下每个地域一个子域名
import vertexai
vertexai.init(project=PROJECT, location="us-central1",
              api_endpoint="us-central1-aiplatform.vertex.example.com")

# OpenRouter
client = OpenAI(base_url="https://ai.example.com/openrouter/api/v1")
```

```bash
# 环境变量写法
export OPENAI_BASE_URL=https://ai.example.com/openai/v1
export ANTHROPIC_BASE_URL=https://ai.example.com/anthropic
export GOOGLE_GEMINI_BASE_URL=https://ai.example.com/gemini
# Amazon Bedrock：只支持 Bedrock API Key（Bearer）
export AWS_BEARER_TOKEN_BEDROCK=...   # base URL: https://ai.example.com/bedrock-mantle-us-east-1/openai/v1
```

### 注意

- **Amazon Bedrock**：只能用 Bedrock API Key（`AWS_BEARER_TOKEN_BEDROCK`）。SigV4 签名包含 Host 头，
  经过代理后签名必然失效。
- **地域限制**：上游看到的是代理服务器的 IP，服务商的地域策略按代理所在地判断。
- **密钥经过代理**：TLS 在前置 Caddy 终止，代理进程能看到明文 API Key 和对话内容。只在你自己控制的
  服务器上部署，并建议开启 IP 白名单。
- **Azure 前缀**：资源名本身以 `cog-` 或 `ai-` 开头时会和 `/azure-cog-`、`/azure-ai-` 前缀冲突，
  这种情况用 `/__h/<资源名>.openai.azure.com/` 访问。

## 访问控制（IP 白名单）

```toml
[access]
enabled = true
admin_token = "一个足够长的随机串"
default_ttl_secs = 0              # 0 = 永久
trusted_proxies = ["127.0.0.1/32", "::1/128"]
client_ip_header = "X-Forwarded-For"
```

开启后，非白名单 IP 访问会得到 403。`/__cp/*` 管理接口不受白名单限制（但需要 token），
在任一站点域名下都可用：

| 接口 | 说明 |
| --- | --- |
| `GET /__cp/allow?token=T` | 把**当前请求的 IP** 加入白名单 |
| `GET /__cp/allow?token=T&ip=1.2.3.0/24&ttl=86400&note=xx` | 加白指定 IP/CIDR，可选有效期（秒）和备注 |
| `GET /__cp/deny?token=T[&ip=...]` | 移除（默认移除当前 IP） |
| `GET /__cp/list?token=T` | 列出白名单 |
| `GET /__cp/ip` | 查看服务端识别到的你的 IP 及是否已加白 |
| `GET /__cp/health` | 健康检查 |
| `GET /__cp/login` | 导入登录 Cookie（见下文） |

token 也可以放在 `X-Admin-Token` 请求头里。

CLI 在服务运行时通过本地管理接口操作，服务未运行时直接编辑白名单文件：

```bash
civitai-proxy -c config.toml allow 1.2.3.4 --ttl 3600 --note phone
civitai-proxy -c config.toml allow 10.0.0.0/8
civitai-proxy -c config.toml deny 1.2.3.4
civitai-proxy -c config.toml list
civitai-proxy -c config.toml config   # 打印生效配置
```

真实客户端 IP 只在 TCP 对端属于 `trusted_proxies` 时才从 `client_ip_header` 读取，以防伪造。
Caddy 与本服务不在同一主机或容器时，请把 Caddy 的地址加入 `trusted_proxies`。

## 网页登录

- **API Token**（推荐）：各站点的 token 都原样透传。
- **网页表单登录**：`Set-Cookie` 会被改写到代理域名，站内表单登录一般可用。
- **OAuth / SSO 第三方登录**（Discord、Google、Docker 的 login.docker.com 等）：回调地址绑定原站，
  无法经代理完成。请直接在原站登录，从浏览器开发者工具复制会话 Cookie，再打开
  `https://<站点域名>/__cp/login`，按 `name=value; name2=value2` 格式粘贴导入
  （页面会提示该站点需要哪些 Cookie，例如 GitHub 需要 `user_session` 和 `__Host-user_session_same_site`）。

## 注意事项与已知限制

- 多个站点的域名不要嵌套在某个 wildcard 站点之下（例如 civitai 用 `*.example.com`，同时 hf 用
  `hf.example.com`）。否则 wildcard 站点设置的 `Domain=.example.com` Cookie 也会发给其它站点，
  启动时会给出警告。推荐每个站点用互不包含的域名。
- Hugging Face Spaces（`*.hf.space`）只有在 HF 站点使用 wildcard 模式时才能完整运行。
- GitHub 静态资源（`github.githubassets.com`）不改写，以保持 SRI 校验有效。
- 若上游触发 Cloudflare / AWS WAF 人机验证，代理无法绕过。
- WebSocket 上游连接不走 `upstream_proxy`（直接连接）。gRPC 支持 `http://` 形式的 CONNECT 代理。
- 不做缓存；如需缓存，可在前置 CDN 上配置。

## 开发

```bash
cargo test      # 单元测试 + 基于本地 mock 上游的端到端测试（Civitai/HF/GitHub/Docker/AI 网关/gRPC）
cargo clippy --all-targets
```

调试时可用 `upstream_resolve` 把某个上游主机指向本地服务（Host 头保持为上游主机）：

```toml
[upstream_resolve]
"huggingface.co" = "http://127.0.0.1:9000"
```

调试日志：`RUST_LOG=info,civitai_proxy=debug`，会逐条记录请求及其映射到的上游地址。
