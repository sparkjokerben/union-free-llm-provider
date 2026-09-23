# ufp · 统一 LLM 中转网关

自用 / 小圈子的 LLM 网关：**下游只讲 Anthropic Messages 协议**（客户端是 Claude Code），
**上游聚合一堆免费额度**（OpenAI 兼容站、Gemini 原生、OpenAI Responses、Anthropic 兼容站），
在中间做协议转换、模型级熔断、额度冷却、故障转移、WebSearch 仿真与用量统计。

目标机器很小：**1 vCPU / 512MB / 10GB SSD 的 Alpine**，所以一路上都在做减法：
单进程、无容器、SQLite、单页内嵌后台、静态 musl 二进制。

```
Claude Code ──TLS──▶ nginx ──HTTP──▶ ufp (127.0.0.1:8787)
                                       │  鉴权 → 选候选（层级 + 会话粘性 + 能力过滤）
                                       │       → 逐候选尝试（熔断 / 冷却 / 矫正）
                                       │       → 协议转换 + SSE 后处理（提交闸门 / WebSearch / 签名信封）
                                       └─▶ 上游池（多渠道 × 多 key × 多模型）
```

## 快速开始（服务器）

```sh
# 1) 从 Release 下载与架构对应的静态二进制（不需要装任何编译工具链）
wget https://github.com/<你>/union-free-llm-provider/releases/latest/download/ufp-x86_64-unknown-linux-musl -O ufp

# 2) 一键安装（建用户与目录、装 nginx、装 OpenRC 服务、设后台密码）
DOMAIN=your.domain sh deploy/install.sh ./ufp

# 3) 证书（acme.sh 自动续期）
apk add --no-cache acme.sh
acme.sh --issue --nginx -d your.domain
acme.sh --install-cert -d your.domain \
  --key-file /etc/nginx/certs/your.domain.key \
  --fullchain-file /etc/nginx/certs/your.domain.crt \
  --reloadcmd "rc-service nginx reload"

# 4) 起服务
rc-service ufp start
rc-service nginx start
```

打开 `https://your.domain/admin`，添加渠道 / key / 条目，然后给 Claude Code 配两个环境变量：

```sh
export ANTHROPIC_BASE_URL=https://your.domain
export ANTHROPIC_AUTH_TOKEN=ufp-xxxx        # 在「下游 key」页新建，明文只显示一次
claude
```

> 不要设 `CLAUDE_CODE_USE_GATEWAY`：那会让 Claude Code 自己关掉 WebSearch，
> 用 `ANTHROPIC_BASE_URL` 即可。

## 概念

| 概念 | 说明 |
|---|---|
| **渠道**（channel） | 一个上游服务：协议（`openai_chat` / `openai_responses` / `gemini` / `anthropic`）、base_url、附加请求头 |
| **上游 key** | 挂在渠道下。同一渠道可以放多把 key（免费额度每把各算一份），额度类错误按「key × 模型」冷却 |
| **条目**（entry） | 渠道 × 上游模型，带层级、上下文窗口、是否支持图片/PDF。运行期的候选 = 条目 × 该渠道启用的 key |
| **层级**（tier） | 数字越小越优先。只在最小可用层里挑；整层都被冷却/熔断才降级 |
| **会话粘性** | 同一 Claude Code 会话固定在同一个条目上，保住上游的隐式前缀缓存；条目不可用才迁移 |
| **熔断 / 冷却** | 真故障（5xx/超时/空流）按「渠道 × 模型」熔断；额度问题（429/402）按「key × 模型」冷却，时长取 Retry-After / Gemini retryDelay / 日配额重置 |
| **下游 key** | 客户端用的 key，只存 sha256；用于鉴权与按 key 统计 |

## 它替你处理的麻烦事

- **协议转换**：Anthropic Messages ⇄ OpenAI Chat / OpenAI Responses / Gemini 原生。
  转换层移植自 [cc-switch](https://github.com/farion1231/cc-switch)，见 `crates/ufp-convert/UPSTREAM.md`。
- **提交闸门**：免费上游常见的「先返回 200 再报错 / 给空流」不再直接透给客户端 ——
  网关会缓冲到**第一个内容增量**才下发，之前出错就悄悄换下一个候选。
- **WebSearch 仿真**：Claude Code 看到的是标准 `web_search` 服务端工具，
  上游看到的是普通函数工具，搜索由网关调用 Tavily/Exa/… 执行，全程不出网关。
  命中 Claude Code 的固定形态搜索请求时走快速路径，省掉一次上游调用。
- **思考与签名**：把 thinking 设置映射到各家推理参数；上游不透明的状态
  （Gemini thoughtSignature、OpenAI encrypted_content）编码进 `thinking` / `redacted_thinking` 的
  签名里随历史往返，重启不丢。客户端没开 thinking 时改写成 `redacted_thinking`：签名照带、正文不显示。
- **请求矫正（会自己学）**：400/413/422 先试确定性矫正器（思考签名、思考预算、max_tokens 下夹、
  图片降级），仍是未知错误就把「错误 + 请求骨架」交给后台指定的分析条目，接受它给出的
  受限 JSON Patch（只允许改写白名单路径，优先改写而不是删除），成功补丁沉淀成规则，下次直接用。
- **用量统计**：每个请求一行明细、每次上游尝试也一行（能看到「换了谁、为什么换」），
  明细留 30 天后汇总成按天数据永久保留。后台按下游 key / 渠道 / 上游模型三个维度看。

## 目录

```
crates/ufp-convert/   协议转换层（移植自 cc-switch，改动带 // UFP: 注释）
  src/providers/gemini_signature.rs  UFP 新增：Gemini 签名的无状态信封
crates/ufp/           网关本体
  src/api/            下游面（/v1/messages、count_tokens、models）与后台
  src/router/         候选选择：层级、会话粘性、能力过滤
  src/health/         熔断（渠道×模型）与冷却（key×模型）
  src/upstream/       四种协议的请求构造与响应转换
  src/pipeline/       Anthropic 事件后处理：提交闸门、model 回显、思考显示、ping、usage
  src/forward.rs      候选循环与错误分类
  src/rectify/        确定性矫正器（LLM 分析与规则见 websearch/analyzer 同一套思路）
  src/websearch/      WebSearch 仿真：后端池、历史信封、拦截续写
  src/store/          SQLite（schema、批量写、配置快照）
deploy/               nginx 配置、OpenRC 服务、安装脚本
```

## 开发

```sh
cargo test --workspace        # 单元 + 移植测试 + 端到端（wiremock 模拟上游）
cargo clippy --workspace --all-targets
cargo run -p ufp -- serve     # 本机起一个，UFP_DB=./ufp.db
cargo run -p ufp -- set-admin-password
```

环境变量（进程级，改了要重启）：`UFP_LISTEN`、`UFP_DB`、`UFP_LOG_DIR`、
`UFP_MAX_BODY_BYTES`、`UFP_MAX_INFLIGHT`、`UFP_DRAIN_SECONDS`、`UFP_WORKERS`、`UFP_LOG`。
其余都在后台的「设置」里，改完立即生效。

## CI/CD（GitHub Actions）

| 工作流 | 触发 | 做什么 |
|---|---|---|
| `ci.yml` | push 到 main、PR | `cargo fmt --check`、`clippy -D warnings`、全量测试（含端到端），不碰密钥 |
| `release.yml` | 打 tag `v*` | 编 x86_64 / aarch64 两个 musl 静态二进制 → 发 Release → **自动部署** |
| `deploy.yml` | 手动 | 下发指定 tag（默认最近一次 Release）；`dry_run` 只验证域名可达 |

### 部署链路（为什么不用 SSH）

目标服务器是 **IPv6-only**，而 GitHub 托管 runner 只有 IPv4 出口（`ubuntu`/`macos`/`windows`
三个镜像都实测过：没有全局 IPv6 地址，`ping6`/`nc` 一律 `Network is unreachable`），
所以 CI 既不能 SSH 进来，服务器也拉不到 GitHub Release 的资产。

于是部署走 **Cloudflare 代理的 HTTPS**：runner（v4）→ CF 边缘 → 源站（v6）→
网关的 `POST /admin/api/deploy`。网关校验 `sha256` 后把包落盘，再通过
`sudo ufp-apply-deploy`（仅这一条免密规则）完成解包、安装或不断流升级、健康检查与回滚。
**CI 里没有任何 SSH 密钥。**

完整步骤（DNS、Origin CA 证书、CF 注意事项、首次手动安装）见
[`deploy/cloudflare.md`](deploy/cloudflare.md)。

### 需要的仓库 Secrets

| Secret | 说明 |
|---|---|
| `DEPLOY_URL` | `https://你的域名`（Cloudflare 代理开启） |
| `DEPLOY_TOKEN` | 服务器上 `ufp set-deploy-token` 生成，或在后台「设置 → 在线部署令牌」轮换 |

### 打一个版本

```sh
git tag v0.1.0 && git push origin v0.1.0     # 编译 → 发布 → 自动部署
```

部署日志在服务器上：`tail -f /var/lib/ufp/incoming/last-deploy.log`。

### 从能连 IPv6 的机器手动部署

CI 的 runner 不行，但你的本机可以（详见 `deploy/cloudflare.md` 第五节）：

```sh
scp deploy.tgz root@[你的IPv6]:/tmp/
ssh root@[你的IPv6] "/usr/local/bin/ufp-apply-deploy /tmp/deploy.tgz $(sha256sum deploy.tgz | awk '{print $1}')"
```

回滚：服务器上 `/usr/local/bin/ufp.old` 是上一版，

```sh
install -m 0755 /usr/local/bin/ufp.old /usr/local/bin/ufp && rc-service ufp restart
```

## 运维

```sh
rc-service ufp status
tail -f /var/log/ufp/ufp.log
curl -s localhost:8787/healthz | jq

# 升级（不断流）：新二进制先接管监听，旧进程排空在途请求后退出
wget <新版本> -O /usr/local/bin/ufp.new && rc-service ufp upgrade
```

## 已知边界

- 只支持流式请求的 WebSearch 续写；非流式请求遇到搜索调用时只做形态转换、不续写。
- Gemini 流式响应里工具调用仍在流的末尾输出（文本与工具调用的相对位置丢失），
  对 Claude Code 这类客户端没有影响。
- 搜索后端的域名过滤（`allowed_domains`）暂未下发到后端。
- 上游压缩响应只在非流式路径解压；流式路径强制 `accept-encoding: identity`。
- Gemini 的思考回放（thought part）与 `thinkingBudget: 0` 的行为需要在真机
  免费额度上实测一遍（已按官方文档实现，但免费层各版本策略有差异）。
