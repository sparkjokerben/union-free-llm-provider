//! 受限 JSON Patch：只允许改写白名单路径，且优先改写而不是删除。
//!
//! LLM 给的补丁不可全信 —— 它会写出 `/messages/0` 这种把整段对话干掉的路径，
//! 或者干脆返回一段散文。所以：
//!
//! 1. 只接受下面白名单里的路径（工具定义与 schema、tool_choice、max_tokens、
//!    thinking、system、output_config，以及内容块的类型/结构字段）；
//! 2. `remove` 比 `replace`/`add` 收得更紧：只能删工具、schema 里的键、
//!    单个内容块或 cache_control —— 不允许删整条消息；
//! 3. 补丁条数与单条体积都有上限，路径里的数组下标越界会被拒绝；
//! 4. 应用后如果请求体变得明显更小（比如工具被删掉了），记一条日志以备排查。

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchOpKind {
    Replace,
    Add,
    Remove,
}

#[derive(Debug, Clone)]
pub struct PatchOp {
    pub op: PatchOpKind,
    /// JSON 指针形式的路径。
    pub path: String,
    pub value: Option<Value>,
}

const MAX_OPS: usize = 20;
const MAX_VALUE_BYTES: usize = 64 * 1024;

/// 允许 `replace` / `add` 的路径前缀与模式。
const ALLOWED_WRITE_PREFIXES: &[&str] = &[
    "/tools/",
    "/tool_choice",
    "/max_tokens",
    "/thinking",
    "/output_config",
    "/system",
    "/stop_sequences",
    "/temperature",
];

/// 内容块里允许改写的字段（`/messages/<n>/content/<m>/...`）。
const ALLOWED_BLOCK_FIELDS: &[&str] = &["type", "name", "tool_use_id", "is_error", "cache_control"];

/// 允许 `remove` 的路径模式（更紧）。
fn remove_allowed(path: &str) -> bool {
    // /tools/<n>                     删掉整个工具
    // /tools/<n>/input_schema/<key>  删掉 schema 里的某个键
    // /messages/<n>/content/<m>      删掉单个内容块
    // /thinking                      关掉思考
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match segs.as_slice() {
        ["tools", n] => n.parse::<usize>().is_ok(),
        ["tools", n, "input_schema", _] => n.parse::<usize>().is_ok(),
        ["tools", n, "input_schema", "properties", _] => n.parse::<usize>().is_ok(),
        ["messages", n, "content", m] => n.parse::<usize>().is_ok() && m.parse::<usize>().is_ok(),
        ["thinking"] => true,
        ["messages", n, "content", m, "cache_control"] => {
            n.parse::<usize>().is_ok() && m.parse::<usize>().is_ok()
        }
        _ => false,
    }
}

fn write_allowed(path: &str) -> bool {
    if path.contains("..") {
        return false;
    }
    // 前缀统一去掉结尾的斜杠再比对，避免 "/tools//" 这种拼接错误。
    let is_under = |prefix: &str| {
        let p = prefix.trim_end_matches('/');
        path == p || path.starts_with(&format!("{p}/")) || path.starts_with(&format!("{p}["))
    };
    if ALLOWED_WRITE_PREFIXES.iter().any(|p| is_under(p)) {
        return true;
    }
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if let ["messages", n, "content", m, field] = segs.as_slice() {
        return n.parse::<usize>().is_ok()
            && m.parse::<usize>().is_ok()
            && ALLOWED_BLOCK_FIELDS.contains(field);
    }
    false
}

/// 解析并校验一段补丁 JSON（分析条目输出的 `patch` 数组）。
pub fn parse(patch: &Value) -> Result<Vec<PatchOp>, String> {
    let Some(items) = patch.as_array() else {
        return Err("patch 必须是数组".into());
    };
    if items.len() > MAX_OPS {
        return Err(format!("补丁条数超过上限（{} 条）", MAX_OPS));
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let op = match item.get("op").and_then(|o| o.as_str()) {
            Some("replace") => PatchOpKind::Replace,
            Some("add") => PatchOpKind::Add,
            Some("remove") => PatchOpKind::Remove,
            other => return Err(format!("不支持的 op：{other:?}")),
        };
        let path = item
            .get("path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| "缺少 path".to_string())?
            .to_string();
        if !path.starts_with('/') {
            return Err(format!("path 必须是绝对 JSON 指针：{path}"));
        }
        let allowed = match op {
            PatchOpKind::Remove => remove_allowed(&path),
            _ => write_allowed(&path),
        };
        if !allowed {
            return Err(format!("路径不在白名单里：{path}"));
        }
        let value = item.get("value").cloned();
        if op != PatchOpKind::Remove {
            let Some(v) = value.as_ref() else {
                return Err(format!("{path} 缺少 value"));
            };
            if serde_json::to_vec(v).map(|b| b.len()).unwrap_or(0) > MAX_VALUE_BYTES {
                return Err(format!("{path} 的 value 过大"));
            }
        }
        out.push(PatchOp { op, path, value });
    }
    Ok(out)
}

/// 应用补丁，返回改动条数。
pub fn apply(body: &mut Value, patches: &[PatchOp]) -> Result<usize, String> {
    let mut applied = 0;
    for patch in patches {
        if apply_one(body, patch)? {
            applied += 1;
        }
    }
    Ok(applied)
}

fn apply_one(body: &mut Value, patch: &PatchOp) -> Result<bool, String> {
    let tokens = tokenize(&patch.path)?;
    let (last, parents) = tokens.split_last().ok_or_else(|| "空路径".to_string())?;
    let mut cursor = body;
    for token in parents {
        cursor = descend_mut(cursor, token).ok_or_else(|| format!("路径不存在：{}", patch.path))?;
    }
    match (last, &patch.op) {
        (Token::Key(key), PatchOpKind::Replace) => {
            let Some(map) = cursor.as_object_mut() else {
                return Err(format!("{} 的父级不是对象", patch.path));
            };
            let value = patch.value.clone().unwrap_or(Value::Null);
            match map.get(key) {
                None => Ok(false),                       // 不存在就不改（replace 不创建）
                Some(old) if *old == value => Ok(false), // 值没变不算改动
                Some(_) => {
                    map.insert(key.clone(), value);
                    Ok(true)
                }
            }
        }
        (Token::Key(key), PatchOpKind::Add) => {
            let Some(map) = cursor.as_object_mut() else {
                return Err(format!("{} 的父级不是对象", patch.path));
            };
            let value = patch.value.clone().unwrap_or(Value::Null);
            if map.get(key) == Some(&value) {
                return Ok(false);
            }
            map.insert(key.clone(), value);
            Ok(true)
        }
        (Token::Key(key), PatchOpKind::Remove) => {
            let Some(map) = cursor.as_object_mut() else {
                return Err(format!("{} 的父级不是对象", patch.path));
            };
            Ok(map.remove(key).is_some())
        }
        (Token::Index(i), PatchOpKind::Replace | PatchOpKind::Remove) => {
            let Some(items) = cursor.as_array_mut() else {
                return Err(format!("{} 的父级不是数组", patch.path));
            };
            if *i >= items.len() {
                return Err(format!("{} 下标越界", patch.path));
            }
            match patch.op {
                PatchOpKind::Replace => {
                    let value = patch.value.clone().unwrap_or(Value::Null);
                    if items[*i] == value {
                        return Ok(false);
                    }
                    items[*i] = value;
                    Ok(true)
                }
                PatchOpKind::Remove => {
                    items.remove(*i);
                    Ok(true)
                }
                PatchOpKind::Add => unreachable!(),
            }
        }
        (Token::Index(_), PatchOpKind::Add) => Err("数组末尾追加暂不支持，请用具体下标".into()),
    }
}

enum Token {
    Key(String),
    Index(usize),
}

fn tokenize(path: &str) -> Result<Vec<Token>, String> {
    let mut out = Vec::new();
    for part in path.trim_start_matches('/').split('/') {
        if part.is_empty() {
            continue;
        }
        // JSON 指针里的 ~0 / ~1 转义
        let part = part.replace("~1", "/").replace("~0", "~");
        if let Ok(i) = part.parse::<usize>() {
            out.push(Token::Index(i));
        } else {
            out.push(Token::Key(part));
        }
    }
    Ok(out)
}

fn descend_mut<'a>(value: &'a mut Value, token: &Token) -> Option<&'a mut Value> {
    match token {
        Token::Key(key) => value.as_object_mut()?.get_mut(key),
        Token::Index(i) => value.as_array_mut()?.get_mut(*i),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn patch(op: &str, path: &str, value: Option<Value>) -> Value {
        match value {
            Some(v) => json!({"op": op, "path": path, "value": v}),
            None => json!({"op": op, "path": path}),
        }
    }

    #[test]
    fn 白名单外的路径被拒() {
        assert!(parse(&json!([patch(
            "replace",
            "/messages/0/content/0/text",
            Some(json!("换掉"))
        )]))
        .is_err());
        assert!(parse(&json!([patch("replace", "/messages/0", Some(json!([])))])).is_err());
        assert!(parse(&json!([patch("remove", "/messages/0", None)])).is_err());
    }

    #[test]
    fn 允许改工具_schema_与结构字段() {
        let ops = parse(&json!([
            patch(
                "replace",
                "/tools/0/input_schema/type",
                Some(json!("object"))
            ),
            patch(
                "add",
                "/tools/0/input_schema/additionalProperties",
                Some(json!(false))
            ),
            patch("remove", "/tools/0/input_schema/required", None),
            patch("replace", "/tool_choice", Some(json!("auto"))),
            patch("replace", "/max_tokens", Some(json!(4096))),
            patch("remove", "/messages/1/content/2", None),
        ]))
        .unwrap();
        assert_eq!(ops.len(), 6);
    }

    #[test]
    fn 应用补丁会真的改到请求体() {
        let mut body = json!({
            "max_tokens": 200000,
            "tool_choice": {"type": "tool", "name": "x"},
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}, {"type": "image"}]}],
            "tools": [{"name": "x", "input_schema": {"type": "string", "required": ["a"]}}]
        });
        let ops = parse(&json!([
            patch("replace", "/max_tokens", Some(json!(8192))),
            patch("replace", "/tool_choice", Some(json!("auto"))),
            patch(
                "replace",
                "/tools/0/input_schema/type",
                Some(json!("object"))
            ),
            patch("remove", "/tools/0/input_schema/required", None),
            patch("remove", "/messages/0/content/1", None),
        ]))
        .unwrap();
        let applied = apply(&mut body, &ops).unwrap();
        assert_eq!(applied, 5);
        assert_eq!(body["max_tokens"], 8192);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert!(body["tools"][0]["input_schema"].get("required").is_none());
        assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn replace_不会创建不存在的键() {
        let mut body = json!({"max_tokens": 100});
        let ops = parse(&json!([patch(
            "replace",
            "/thinking/budget_tokens",
            Some(json!(1000))
        )]));
        // 路径父级不存在 → 报错而不是瞎建
        assert!(ops.is_ok());
        let err = apply(&mut body, &ops.unwrap()).unwrap_err();
        assert!(err.contains("路径不存在"), "{err}");
    }

    #[test]
    fn 值没变不算改动() {
        let mut body = json!({"max_tokens": 4096, "tools": [{"name": "x", "input_schema": {"type": "object"}}]});
        let ops = parse(&json!([
            patch("replace", "/max_tokens", Some(json!(4096))),
            patch("add", "/tools/0/input_schema/type", Some(json!("object"))),
        ]))
        .unwrap();
        assert_eq!(apply(&mut body, &ops).unwrap(), 0);
    }

    #[test]
    fn 下标越界被拒() {
        let mut body = json!({"messages": [{"role": "user", "content": []}]});
        let ops = parse(&json!([patch("remove", "/messages/0/content/5", None)])).unwrap();
        assert!(apply(&mut body, &ops).is_err());
    }

    #[test]
    fn 条数超限被拒() {
        let many: Vec<Value> = (0..30)
            .map(|i| {
                patch(
                    "replace",
                    &format!("/tools/{i}/description"),
                    Some(json!("x")),
                )
            })
            .collect();
        assert!(parse(&json!(many)).is_err());
    }
}
