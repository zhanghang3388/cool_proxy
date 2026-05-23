use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

use serde_json::{json, Map, Value};

/// codex 上游每个 plan 能用的 model 列表，从 CLIProxyAPI 同步过来
/// (`internal/registry/models/models.json` 中的 `codex-free` / `codex-team` /
/// `codex-plus` / `codex-pro` 四个段落)。编译时直接嵌进二进制；启动 / 运行期都不读文件。
const CODEX_PLAN_MODELS_JSON: &str =
    include_str!("./assets/codex_plan_models.json");

#[derive(Debug, Clone)]
pub struct PlanCatalog {
    /// plan key（"codex-free" / "codex-plus" / "codex-pro" / "codex-team"）→ model 数组
    by_plan: HashMap<String, Vec<Map<String, Value>>>,
}

fn catalog() -> &'static PlanCatalog {
    static C: OnceLock<PlanCatalog> = OnceLock::new();
    C.get_or_init(|| {
        let parsed: Value = serde_json::from_str(CODEX_PLAN_MODELS_JSON)
            .expect("codex_plan_models.json must be valid JSON");
        let mut by_plan: HashMap<String, Vec<Map<String, Value>>> = HashMap::new();
        if let Some(obj) = parsed.as_object() {
            for (plan_key, list) in obj {
                if let Some(arr) = list.as_array() {
                    let v: Vec<Map<String, Value>> = arr
                        .iter()
                        .filter_map(|x| x.as_object().cloned())
                        .collect();
                    by_plan.insert(plan_key.clone(), v);
                }
            }
        }
        PlanCatalog { by_plan }
    })
}

/// 把账号的 plan 字符串（"free"/"plus"/"pro"/"team"/"business"/"go"…）映射到 catalog 里的 key。
/// 未识别 plan 走 codex-pro 当兜底（CLIProxyAPI 同款行为）。
pub fn plan_key_for(plan: Option<&str>) -> &'static str {
    match plan
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
        .unwrap_or("")
    {
        "pro" => "codex-pro",
        "plus" => "codex-plus",
        "team" | "business" | "go" => "codex-team",
        "free" => "codex-free",
        _ => "codex-pro",
    }
}

/// 给一组账号的 plan 求 model 并集，按 model id 去重，按 catalog 里的出现顺序稳定排序。
fn union_models_for_plans(plans: &[&str]) -> Vec<Map<String, Value>> {
    let cat = catalog();
    let mut seen: HashSet<String> = HashSet::new();
    let mut order_by_id: BTreeMap<usize, Map<String, Value>> = BTreeMap::new();
    let mut order_counter: usize = 0;

    for plan in plans {
        let key = plan_key_for(Some(plan));
        let Some(list) = cat.by_plan.get(key) else {
            continue;
        };
        for m in list {
            let Some(id) = m.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            if seen.insert(id.to_string()) {
                order_by_id.insert(order_counter, m.clone());
                order_counter += 1;
            }
        }
    }

    order_by_id.into_values().collect()
}

/// 简化版 / OpenAI SDK 风格：每个 model 只 4 字段。
/// `plans` 是当前可用账号的 plan 列表。
pub fn build_simple_list(plans: &[&str]) -> Value {
    let merged = union_models_for_plans(plans);
    let data: Vec<Value> = merged
        .into_iter()
        .map(|m| {
            let id = m
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let owned_by = m
                .get("owned_by")
                .and_then(|v| v.as_str())
                .unwrap_or("openai")
                .to_string();
            let created = m.get("created").and_then(|v| v.as_i64()).unwrap_or(1_700_000_000);
            json!({
                "id": id,
                "object": "model",
                "created": created,
                "owned_by": owned_by,
            })
        })
        .collect();
    json!({ "object": "list", "data": data })
}

/// codex CLI 风格：返回完整的 model metadata（context_window /
/// supported_reasoning_levels 等）。直接吐 catalog 模板原样。
pub fn build_codex_client_response(plans: &[&str]) -> Value {
    let merged = union_models_for_plans(plans);
    let arr: Vec<Value> = merged.into_iter().map(Value::Object).collect();
    json!({ "models": arr })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_loads_all_four_plans() {
        let c = catalog();
        for k in ["codex-free", "codex-team", "codex-plus", "codex-pro"] {
            let arr = c.by_plan.get(k);
            assert!(arr.is_some(), "missing plan {k}");
            assert!(!arr.unwrap().is_empty(), "plan {k} is empty");
        }
    }

    #[test]
    fn plan_key_mapping() {
        assert_eq!(plan_key_for(Some("free")), "codex-free");
        assert_eq!(plan_key_for(Some("Plus")), "codex-plus");
        assert_eq!(plan_key_for(Some("PRO")), "codex-pro");
        assert_eq!(plan_key_for(Some("team")), "codex-team");
        assert_eq!(plan_key_for(Some("business")), "codex-team");
        // 未知 / 空 → pro
        assert_eq!(plan_key_for(Some("enterprise")), "codex-pro");
        assert_eq!(plan_key_for(Some("")), "codex-pro");
        assert_eq!(plan_key_for(None), "codex-pro");
    }

    #[test]
    fn simple_list_has_required_keys() {
        let v = build_simple_list(&["pro"]);
        assert_eq!(v["object"], "list");
        let arr = v["data"].as_array().unwrap();
        assert!(!arr.is_empty());
        for m in arr {
            assert!(m["id"].as_str().is_some_and(|s| !s.is_empty()));
            assert_eq!(m["object"], "model");
            assert!(m["created"].is_number());
            assert!(m["owned_by"].as_str().is_some_and(|s| !s.is_empty()));
        }
    }

    #[test]
    fn plus_and_pro_both_include_codex_auto_review() {
        // plus 与 pro 都包含 codex-auto-review；free / team 不含 spark
        let pro_ids: HashSet<String> = build_simple_list(&["pro"])["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(String::from))
            .collect();
        let plus_ids: HashSet<String> = build_simple_list(&["plus"])["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(String::from))
            .collect();
        let free_ids: HashSet<String> = build_simple_list(&["free"])["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(String::from))
            .collect();
        assert!(plus_ids.contains("codex-auto-review"));
        assert!(pro_ids.contains("codex-auto-review"));
        // spark 仅在 plus / pro
        assert!(plus_ids.contains("gpt-5.3-codex-spark"));
        assert!(pro_ids.contains("gpt-5.3-codex-spark"));
        assert!(!free_ids.contains("gpt-5.3-codex-spark"));
    }

    #[test]
    fn union_dedupes_across_plans() {
        // free 和 plus 都有 gpt-5.5，并集里只能出现一次
        let merged = build_simple_list(&["free", "plus"]);
        let mut count = 0;
        for m in merged["data"].as_array().unwrap() {
            if m["id"] == "gpt-5.5" {
                count += 1;
            }
        }
        assert_eq!(count, 1);
    }

    #[test]
    fn empty_plans_returns_empty_list() {
        let v = build_simple_list(&[]);
        assert_eq!(v["data"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn codex_client_returns_full_metadata() {
        let v = build_codex_client_response(&["pro"]);
        let arr = v["models"].as_array().unwrap();
        assert!(!arr.is_empty());
        // catalog 里的条目带 display_name + context_length（来自 models.json 的 codex 段）
        let first = &arr[0];
        assert!(first["display_name"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(first["context_length"].is_number());
    }
}
