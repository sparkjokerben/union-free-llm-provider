# 上游同步说明

本 crate 的协议转换代码移植自 **cc-switch**：

- 仓库：https://github.com/farion1231/cc-switch
- 本地参考副本：`references/cc-switch`（git 忽略，不入库）
- 基准提交：`f2537fdf6b709082b52886d52ce6914bae690699`（2026-09-23）
- 原始路径：`src-tauri/src/proxy/`

## 文件映射

| ufp-convert | cc-switch 原文件 |
|---|---|
| `src/sse.rs` | `proxy/sse.rs` |
| `src/json_canonical.rs` | `proxy/json_canonical.rs` |
| `src/tool_media.rs` | `proxy/tool_media.rs` |
| `src/content_encoding.rs` | `proxy/content_encoding.rs` |
| `src/body_filter.rs` | `proxy/body_filter.rs` |
| `src/gemini_url.rs` | `proxy/gemini_url.rs` |
| `src/thinking_rectifier.rs` | `proxy/thinking_rectifier.rs` |
| `src/thinking_budget_rectifier.rs` | `proxy/thinking_budget_rectifier.rs` |
| `src/providers/transform.rs` | `proxy/providers/transform.rs` |
| `src/providers/streaming.rs` | `proxy/providers/streaming.rs` |
| `src/providers/transform_responses.rs` | `proxy/providers/transform_responses.rs` |
| `src/providers/streaming_responses.rs` | `proxy/providers/streaming_responses.rs` |
| `src/providers/reasoning_bridge.rs` | `proxy/providers/reasoning_bridge.rs` |
| `src/providers/transform_gemini.rs` | `proxy/providers/transform_gemini.rs` |
| `src/providers/streaming_gemini.rs` | `proxy/providers/streaming_gemini.rs` |
| `src/providers/gemini_schema.rs` | `proxy/providers/gemini_schema.rs` |
| `src/providers/gemini_shadow.rs` | `proxy/providers/gemini_shadow.rs`（保留但已不参与主链路） |
| `src/providers/gemini_signature.rs` | **UFP 新增**：无状态签名信封，替代 shadow store |
| `src/usage/parser.rs` | `proxy/usage/parser.rs` |

## 移植时做的改动

1. `ProxyError` → `ConvertError`（只保留 `TransformError` / `InvalidRequest` 两个变体），
   见 `src/error.rs`。
2. `use crate::proxy::…` → `use crate::…`；`axum::http::header::*` → `http::header::*`
   （不再依赖 axum）。
3. `types::RectifierConfig` 按需裁剪。
4. 需要被服务 crate 调用的条目从 `pub(crate)` 放宽为 `pub`。

所有与上游不同的地方都带 `// UFP:` 注释，改动清单可用：

```sh
git -C references/cc-switch log -1 --format=%H          # 确认基准提交
rg -n 'UFP:' crates/ufp-convert/src                     # 列出所有本地改动
```

## 上游缺口的处理情况（逐条对照，改动都带 `// UFP:` 注释）

已修（有专项测试）：

- ✅ `message_start` 缺 `content: []` / `stop_reason` / `stop_sequence`
  → 由网关的 `pipeline` 补齐（转换层不动，避免和上游 diff 打架）。
- ✅ Chat 流里 `data: {"error":…}` 被吞 → `streaming.rs` 转成 Anthropic `error` 事件。
- ✅ 流缺 `finish_reason` 时不收尾 → 网关 `pipeline::finalize()` 补
  `message_delta` + `message_stop`。
- ✅ Gemini 流内错误体被静默忽略 → `streaming_gemini.rs` 转成 `error` 事件；
  传输层 `io::Error` 由网关管线统一转成 `error` 事件（不再断连）。
- ✅ Gemini 没有映射 `thinkingConfig` → `build_generation_config` 按
  `thinking.type` 映射 `includeThoughts` / `thinkingBudget`（gemini-3 关不掉思考，只关展示）。
- ✅ Gemini 思考部分被丢弃 → 响应（流式与非流式）产出 `thinking` 块；
  请求方向把 thinking/redacted_thinking 回放成 `thought: true` part。
- ✅ 工具调用的 `thoughtSignature` 依赖进程内存 shadow → 换成无状态签名信封
  （`gemini_signature.rs`：签名编进 `tool_use.id` 与思考块签名），重启/多实例都不丢。
- ✅ Chat 上 gpt-5.x 需要 `max_completion_tokens` → `needs_max_completion_tokens()`。
- ✅ 非 shadow 分支把 `thoughtSignature` 写进了 `functionCall` 里面（上游
  `transform_gemini.rs` 的 `convert_message_content_to_parts`）→ 挂到 Part 上，与
  `functionCall` 平级。cc-switch 平时走 shadow 回放碰不到；我们不用 shadow，每个带签名的
  工具调用都会触发 `Unknown name "thoughtSignature" at ...function_call`。
- ✅ `gemini_schema.rs` 把没写 `items` 的数组放进受限的 `parameters`，Gemini 回
  `...items: missing field` → 改走 `parametersJsonSchema`。`items: {}` 与没有类型的嵌套
  schema（JSON Schema 的「任意值」）Google 不报错，但受限 Schema 表达不了，一并改走。
- ✅ 历史里有别家模型产生的工具调用（没有签名）时，Gemini 3 回 `Function call is missing
  a thought_signature` → `fill_missing_function_call_signatures` 给这一步第一个 functionCall
  补 Google 文档给的占位签名 `skip_thought_signature_validator`（UFP 新增，cc-switch 没有）。

仍未处理（不阻塞主线）：

- Gemini 流式的工具调用仍在流的末尾输出（相对文本的位置丢失）。对 Claude Code
  这类「先扫到 tool_use 再统一执行」的客户端不影响；真要严格保序需要按 part 原始
  下标切分文本块，风险与收益不成比例，暂不做。
- 域名过滤（`web_search.allowed_domains`）没有下发到搜索后端。
