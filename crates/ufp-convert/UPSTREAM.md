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
| `src/providers/gemini_shadow.rs` | `proxy/providers/gemini_shadow.rs` |
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

## 已知要修的上游缺口（实现时逐一补，均加 `// UFP:` 注释）

- `message_start` 缺 `content: []` / `stop_reason` / `stop_sequence`。
- Chat 流里 `data: {"error":…}` 被当成空 chunk 吞掉；流在没有 `finish_reason` 时
  不补 `message_delta` / `message_stop`，块一直悬着。
- Gemini 流内错误直接变成 `io::Error` 断开连接，应改成 Anthropic `error` 事件。
- Gemini 的工具调用被堆到流的末尾才输出，位置相对文本丢失。
- Gemini 完全没有映射 `thinkingConfig`，思考部分被丢弃。
- Chat 上 gpt-5.x 需要 `max_completion_tokens`。
- `gemini_shadow.rs` 的进程内 `thoughtSignature` 存储要换成无状态签名信封
  （泛化 `reasoning_bridge.rs` 的做法），否则重启即丢。
