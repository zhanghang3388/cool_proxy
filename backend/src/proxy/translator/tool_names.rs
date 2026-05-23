use std::collections::{HashMap, HashSet};

const NAME_LIMIT: usize = 64;

/// 单个 tool 名超长时的最小化缩名规则。`mcp__` 前缀的 tool 名（来自 MCP server 的工具）会保留前缀
/// 和最后一段，避免缩成无意义的字符串；其他情况直接截断到 64 字节。
pub fn shorten_name_if_needed(name: &str) -> String {
    if name.len() <= NAME_LIMIT {
        return name.to_string();
    }
    if let Some(rest) = name.strip_prefix("mcp__") {
        // rest 形如 "<server>__<actual_name>"；只保留最后一段
        if let Some(idx) = rest.rfind("__") {
            let last = &rest[idx + 2..];
            let mut cand = format!("mcp__{last}");
            if cand.len() > NAME_LIMIT {
                cand.truncate(NAME_LIMIT);
            }
            return cand;
        }
    }
    let mut out = name.to_string();
    out.truncate(NAME_LIMIT);
    out
}

/// 给一组 tool 名建立 "原名 → 缩名" 映射，保证缩名互不冲突。
/// 冲突时追加 `_1` / `_2` 等后缀，整体仍然不超过 64 字节。
pub fn build_short_name_map(names: &[&str]) -> HashMap<String, String> {
    let mut used: HashSet<String> = HashSet::new();
    let mut map: HashMap<String, String> = HashMap::new();

    let base_candidate = |n: &str| -> String {
        if n.len() <= NAME_LIMIT {
            return n.to_string();
        }
        if let Some(rest) = n.strip_prefix("mcp__") {
            if let Some(idx) = rest.rfind("__") {
                let last = &rest[idx + 2..];
                let mut cand = format!("mcp__{last}");
                if cand.len() > NAME_LIMIT {
                    cand.truncate(NAME_LIMIT);
                }
                return cand;
            }
        }
        let mut out = n.to_string();
        out.truncate(NAME_LIMIT);
        out
    };

    let make_unique = |cand: String, used: &HashSet<String>| -> String {
        if !used.contains(&cand) {
            return cand;
        }
        let base = cand;
        for i in 1.. {
            let suffix = format!("_{i}");
            let allowed = NAME_LIMIT.saturating_sub(suffix.len());
            let mut trimmed = base.clone();
            if trimmed.len() > allowed {
                trimmed.truncate(allowed);
            }
            trimmed.push_str(&suffix);
            if !used.contains(&trimmed) {
                return trimmed;
            }
        }
        unreachable!()
    };

    for n in names {
        let cand = base_candidate(n);
        let uniq = make_unique(cand, &used);
        used.insert(uniq.clone());
        map.insert((*n).to_string(), uniq);
    }
    map
}

/// 从原始 OpenAI 请求体里反向构建 "缩名 → 原名" 映射。
/// 响应翻译时用这个把 codex 返回的 (可能被缩过的) 函数名还原成调用方期望的名字。
pub fn build_reverse_map_from_openai(original_body: &[u8]) -> HashMap<String, String> {
    let mut rev = HashMap::new();
    let v: serde_json::Value = match serde_json::from_slice(original_body) {
        Ok(v) => v,
        Err(_) => return rev,
    };
    let Some(tools) = v.get("tools").and_then(|t| t.as_array()) else {
        return rev;
    };
    let names: Vec<&str> = tools
        .iter()
        .filter(|t| t.get("type").and_then(|x| x.as_str()) == Some("function"))
        .filter_map(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
        })
        .collect();
    if names.is_empty() {
        return rev;
    }
    let fwd = build_short_name_map(&names);
    for (orig, short) in fwd {
        rev.insert(short, orig);
    }
    rev
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_name_passthrough_for_short_names() {
        assert_eq!(shorten_name_if_needed("read_file"), "read_file");
    }

    #[test]
    fn short_name_truncates_plain_long_names() {
        let name = "a".repeat(100);
        let s = shorten_name_if_needed(&name);
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c == 'a'));
    }

    #[test]
    fn short_name_preserves_mcp_last_segment() {
        let name = "mcp__some_very_long_server_with_extra_stuff__that_keeps_going_and_going__do_thing";
        assert!(name.len() > 64);
        let s = shorten_name_if_needed(name);
        assert!(s.starts_with("mcp__do_thing"), "got {s:?}");
        assert!(s.len() <= 64);
    }

    #[test]
    fn map_resolves_collisions() {
        let n1 = "x".repeat(70);
        let n2 = "x".repeat(80);
        let names: Vec<&str> = vec![&n1, &n2];
        let m = build_short_name_map(&names);
        let s1 = &m[&n1];
        let s2 = &m[&n2];
        assert_ne!(s1, s2);
        assert!(s1.len() <= 64);
        assert!(s2.len() <= 64);
    }

    #[test]
    fn reverse_map_builds_from_request_body() {
        let body = br#"{
            "tools": [
                {"type":"function","function":{"name":"read_file","parameters":{}}},
                {"type":"function","function":{"name":"write_file","parameters":{}}}
            ]
        }"#;
        let rev = build_reverse_map_from_openai(body);
        // 短名直接映射回自己
        assert_eq!(rev.get("read_file"), Some(&"read_file".to_string()));
        assert_eq!(rev.get("write_file"), Some(&"write_file".to_string()));
    }
}
