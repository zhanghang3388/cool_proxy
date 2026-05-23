use std::sync::OnceLock;

use serde_json::{json, Map, Value};

/// Codex 客户端的完整模型描述模板，从 CLIProxyAPI 同步过来。
/// 编译时直接嵌进二进制；启动 / 运行期都不读文件。
const CODEX_CLIENT_MODELS_JSON: &str =
    include_str!("./assets/codex_client_models.json");

/// 解析过的 catalog（slug → 完整模板）。
/// 启动后第一次访问 /v1/models 时懒加载。
struct Catalog {
    by_slug: Vec<(String, Map<String, Value>)>,
    default_template: Option<Map<String, Value>>,
}

fn catalog() -> &'static Catalog {
    static C: OnceLock<Catalog> = OnceLock::new();
    C.get_or_init(|| {
        let parsed: Value = serde_json::from_str(CODEX_CLIENT_MODELS_JSON)
            .expect("codex_client_models.json must be valid JSON");
        let mut by_slug: Vec<(String, Map<String, Value>)> = Vec::new();
        let mut default_template: Option<Map<String, Value>> = None;

        if let Some(arr) = parsed
            .get("models")
            .and_then(|v| v.as_array())
        {
            for entry in arr {
                let Some(obj) = entry.as_object() else {
                    continue;
                };
                let slug = obj
                    .get("slug")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if slug.is_empty() {
                    continue;
                }
                by_slug.push((slug.clone(), obj.clone()));
                // CLIProxyAPI 用 gpt-5.5 模板做 default。我们沿用同样的约定。
                if slug == "gpt-5.5" {
                    default_template = Some(obj.clone());
                }
            }
        }
        // 没有 gpt-5.5 也得有兜底
        if default_template.is_none() {
            default_template = by_slug.first().map(|(_, t)| t.clone());
        }

        Catalog {
            by_slug,
            default_template,
        }
    })
}

/// 简化版 / OpenAI SDK 风格：每个 model 只 4 字段。
pub fn build_simple_list(model_ids: &[&str]) -> Value {
    let data: Vec<Value> = model_ids
        .iter()
        .map(|id| {
            json!({
                "id": id,
                "object": "model",
                "created": 1_700_000_000u64,
                "owned_by": "openai",
            })
        })
        .collect();
    json!({ "object": "list", "data": data })
}

/// codex CLI 风格：返回 `{models: [...]}`，每条是完整模板（context_window /
/// supported_reasoning_levels / availability_nux 等）。
/// 在 catalog 里命中的直接克隆模板；没命中的用 default 模板套一层 metadata。
pub fn build_codex_client_response(model_ids: &[&str]) -> Value {
    let cat = catalog();
    let mut out: Vec<Value> = Vec::with_capacity(model_ids.len());

    for id in model_ids {
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        if let Some((_, tmpl)) = cat.by_slug.iter().find(|(slug, _)| slug == id) {
            let mut entry = tmpl.clone();
            sanitize_reasoning(&mut entry);
            apply_visibility_override(&mut entry, id);
            out.push(Value::Object(entry));
            continue;
        }
        let Some(def) = cat.default_template.as_ref() else {
            continue;
        };
        let mut entry = def.clone();
        apply_default_metadata(&mut entry, id);
        sanitize_reasoning(&mut entry);
        apply_visibility_override(&mut entry, id);
        out.push(Value::Object(entry));
    }

    // 按 priority 升序（CLIProxyAPI 默认 100；保留写在模板里的优先级）
    out.sort_by_key(|m| {
        m.as_object()
            .and_then(|o| o.get("priority"))
            .and_then(|v| v.as_i64())
            .unwrap_or(100)
    });

    json!({ "models": out })
}

/// 给在 catalog 里没命中的 id 套上基本字段，避免客户端报"missing display_name"之类。
fn apply_default_metadata(entry: &mut Map<String, Value>, id: &str) {
    entry.insert("slug".into(), Value::String(id.to_string()));
    entry.insert("display_name".into(), Value::String(id.to_string()));
    entry.insert("description".into(), Value::String(id.to_string()));
    entry.insert("priority".into(), Value::from(100));
    entry.insert("prefer_websockets".into(), Value::Bool(false));
    // 这些字段对自定义 model 没意义；删掉避免误导
    entry.remove("apply_patch_tool_type");
    entry.remove("upgrade");
    entry.remove("availability_nux");
}

/// CLIProxyAPI 同款：把不识别的 reasoning level 过滤掉，保证 default 在 supported 里。
fn sanitize_reasoning(entry: &mut Map<String, Value>) {
    let allowed = ["none", "low", "medium", "high", "xhigh"];
    let Some(raw_levels) = entry.get("supported_reasoning_levels").cloned() else {
        return;
    };
    let Some(arr) = raw_levels.as_array() else {
        entry.remove("supported_reasoning_levels");
        entry.remove("default_reasoning_level");
        return;
    };

    let mut kept: Vec<Value> = Vec::with_capacity(arr.len());
    for item in arr {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let effort = obj
            .get("effort")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_lowercase())
            .unwrap_or_default();
        if !allowed.contains(&effort.as_str()) || effort.is_empty() {
            continue;
        }
        let mut cloned = obj.clone();
        cloned.insert("effort".into(), Value::String(effort));
        kept.push(Value::Object(cloned));
    }

    if kept.is_empty() {
        entry.remove("supported_reasoning_levels");
        entry.remove("default_reasoning_level");
        return;
    }

    let default_lv = entry
        .get("default_reasoning_level")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();
    let default_ok = kept
        .iter()
        .any(|l| l.get("effort").and_then(|v| v.as_str()) == Some(default_lv.as_str()));
    let final_default = if default_ok {
        default_lv
    } else {
        kept[0]
            .get("effort")
            .and_then(|v| v.as_str())
            .unwrap_or("medium")
            .to_string()
    };

    entry.insert("supported_reasoning_levels".into(), Value::Array(kept));
    entry.insert(
        "default_reasoning_level".into(),
        Value::String(final_default),
    );
}

/// CLIProxyAPI 里把几个图像/视频 slug 强制隐藏。我们目前不暴露图像端点，沿用同款隐藏。
fn apply_visibility_override(entry: &mut Map<String, Value>, id: &str) {
    if matches!(
        id,
        "grok-imagine-image-quality" | "gpt-image-2" | "grok-imagine-image" | "grok-imagine-video"
    ) {
        entry.insert("visibility".into(), Value::String("hide".into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_loads_with_slugs() {
        let cat = catalog();
        assert!(!cat.by_slug.is_empty(), "catalog should have entries");
        assert!(cat.default_template.is_some(), "default template missing");
    }

    #[test]
    fn simple_list_has_required_keys() {
        let v = build_simple_list(&["gpt-5-codex", "gpt-5"]);
        assert_eq!(v["object"], "list");
        assert_eq!(v["data"].as_array().unwrap().len(), 2);
        let first = &v["data"][0];
        assert_eq!(first["id"], "gpt-5-codex");
        assert_eq!(first["object"], "model");
        assert!(first["created"].is_number());
        assert_eq!(first["owned_by"], "openai");
    }

    #[test]
    fn codex_client_uses_template_when_present() {
        // gpt-5.5 是 catalog 里有的，应该带回完整描述
        let v = build_codex_client_response(&["gpt-5.5"]);
        let arr = v["models"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let m = &arr[0];
        assert_eq!(m["slug"], "gpt-5.5");
        // catalog 里这一项本来就有 display_name
        assert!(m["display_name"].as_str().is_some_and(|s| !s.is_empty()));
    }

    #[test]
    fn codex_client_falls_back_to_default_template() {
        // gpt-5-codex 不在 catalog 里，应该套 default 模板，但 slug = id
        let v = build_codex_client_response(&["gpt-5-codex"]);
        let arr = v["models"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let m = &arr[0];
        assert_eq!(m["slug"], "gpt-5-codex");
        assert_eq!(m["display_name"], "gpt-5-codex");
        assert_eq!(m["description"], "gpt-5-codex");
    }

    #[test]
    fn sanitize_reasoning_keeps_valid_levels_only() {
        let mut entry: Map<String, Value> = serde_json::from_str(
            r#"{
              "supported_reasoning_levels": [
                {"effort": "low"},
                {"effort": "BOGUS"},
                {"effort": "high"}
              ],
              "default_reasoning_level": "BOGUS"
            }"#,
        )
        .unwrap();
        sanitize_reasoning(&mut entry);
        let levels = entry["supported_reasoning_levels"].as_array().unwrap();
        assert_eq!(levels.len(), 2);
        // default 不在列表里时回退到第一项
        assert_eq!(entry["default_reasoning_level"], "low");
    }
}
