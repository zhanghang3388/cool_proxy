//! Kiro 账号的 token 数据模型 + 解析助手。
//!
//! Kiro 的授权信息字段命名很不统一（camelCase / snake_case 混用，嵌套层级各异），
//! 这里把从 `~/.aws/sso/cache/kiro-auth-token.json`、cockpit 导出的账号 JSON、或刷新接口
//! 返回的任意形状，统一规整成 [`KiroTokenData`]。大量提取助手从 cockpit-tools 移植，
//! 保持字段查找路径一致，确保两边解析结果相同。

use base64::Engine;
use chrono::{DateTime, Utc};
use serde_json::Value;

/// Kiro runtime（CodeWhisperer / Q）默认 endpoint，按 region 解析失败时兜底。
pub const KIRO_RUNTIME_DEFAULT_ENDPOINT: &str = "https://q.us-east-1.amazonaws.com";

/// 账号状态：正常 / 被封 / 查询出错。
pub const KIRO_STATUS_NORMAL: &str = "normal";
pub const KIRO_STATUS_BANNED: &str = "banned";
pub const KIRO_STATUS_ERROR: &str = "error";

/// 解析后的规整 token 数据。token 字段交给上层落库（serde skip），这里不做序列化裁剪。
#[derive(Debug, Clone)]
pub struct KiroTokenData {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,

    pub email: String,
    pub user_id: Option<String>,
    pub login_provider: Option<String>,
    /// "social"（Google/GitHub，走 refreshToken 接口）或 "idc"（企业 / Builder-ID，走 AWS OIDC）。
    pub auth_method: String,

    pub idc_region: Option<String>,
    pub issuer_url: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scopes: Option<String>,
    pub login_hint: Option<String>,
    pub profile_arn: Option<String>,

    /// 原始 auth token JSON（合并了 kiro_auth_token_raw），刷新 / 调试时保留。
    pub raw_auth_token: Value,
}

impl KiroTokenData {
    /// 从任意 Kiro 授权 JSON 解析。接受三种来源：
    /// 1. 原始 kiro-auth-token.json（顶层就是 token 对象）
    /// 2. cockpit 导出的账号对象（access_token 等在顶层，另带 kiro_auth_token_raw）
    /// 3. 刷新接口返回体
    pub fn from_value(input: &Value) -> Result<Self, String> {
        // 合并：以输入顶层为主，再补上 kiro_auth_token_raw 里缺的键。
        let mut auth_token = input.clone();
        if let Some(obj) = auth_token.as_object_mut() {
            if let Some(raw) = input.get("kiro_auth_token_raw").and_then(|v| v.as_object()) {
                for (k, v) in raw {
                    obj.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }

        let profile = input.get("kiro_profile_raw");

        let access_token = pick_string(
            Some(&auth_token),
            &[
                &["accessToken"],
                &["access_token"],
                &["token"],
                &["idToken"],
                &["id_token"],
                &["accessTokenJwt"],
            ],
        )
        .ok_or_else(|| "缺少 access token（accessToken / access_token）".to_string())?;

        let refresh_token = pick_string(
            Some(&auth_token),
            &[&["refreshToken"], &["refresh_token"], &["refreshTokenJwt"]],
        );

        let token_type = pick_string(
            Some(&auth_token),
            &[&["tokenType"], &["token_type"], &["authType"]],
        )
        .or_else(|| Some("Bearer".to_string()));

        let expires_at = parse_timestamp(
            get_path_value(&auth_token, &["expiresAt"])
                .or_else(|| get_path_value(&auth_token, &["expires_at"]))
                .or_else(|| get_path_value(&auth_token, &["expiry"]))
                .or_else(|| get_path_value(&auth_token, &["expiration"])),
        )
        .map(unix_seconds_to_dt)
        .or_else(|| {
            // 回退到 expiresIn / expires_in（相对秒数）
            pick_number(Some(&auth_token), &[&["expiresIn"], &["expires_in"]])
                .map(|secs| Utc::now() + chrono::Duration::seconds(secs.round() as i64))
        });

        let profile_arn = extract_profile_arn(Some(&auth_token), profile);

        let id_token_claims = pick_string(
            Some(&auth_token),
            &[&["idToken"], &["id_token"], &["idTokenJwt"], &["id_token_jwt"]],
        )
        .and_then(|raw| decode_jwt_claims(&raw));
        let access_token_claims =
            decode_jwt_claims(&access_token);

        let email = normalize_email(pick_string(
            profile,
            &[
                &["email"],
                &["user", "email"],
                &["account", "email"],
                &["primaryEmail"],
            ],
        ))
        .or_else(|| normalize_email(pick_string(Some(&auth_token), &[&["email"], &["userEmail"]])))
        .or_else(|| {
            normalize_email(pick_string(
                id_token_claims.as_ref(),
                &[&["email"], &["upn"], &["preferred_username"]],
            ))
        })
        .or_else(|| {
            normalize_email(pick_string(
                access_token_claims.as_ref(),
                &[&["email"], &["upn"], &["preferred_username"]],
            ))
        })
        .or_else(|| {
            normalize_email(pick_string(
                Some(&auth_token),
                &[&["login_hint"], &["loginHint"]],
            ))
        })
        .unwrap_or_default();

        let user_id = pick_string(
            profile,
            &[&["userId"], &["user_id"], &["id"], &["sub"], &["account", "id"]],
        )
        .or_else(|| {
            pick_string(
                Some(&auth_token),
                &[&["userId"], &["user_id"], &["sub"], &["accountId"]],
            )
        })
        .or_else(|| pick_string(id_token_claims.as_ref(), &[&["sub"], &["user_id"], &["uid"]]))
        .or_else(|| {
            pick_string(access_token_claims.as_ref(), &[&["sub"], &["user_id"], &["uid"]])
        });

        let login_provider = pick_string(
            Some(&auth_token),
            &[&["provider"], &["loginProvider"], &["login_option"], &["login_provider"]],
        )
        .map(|p| provider_from_login_option(&p).unwrap_or(p));

        let idc_region = resolve_idc_region(&auth_token, profile_arn.as_deref());
        let client_id = pick_string(
            Some(&auth_token),
            &[
                &["client_id"],
                &["clientId"],
                &["clientRegistration", "clientId"],
                &["registration", "clientId"],
                &["oidcClient", "clientId"],
            ],
        );
        let client_secret = pick_string(
            Some(&auth_token),
            &[
                &["client_secret"],
                &["clientSecret"],
                &["clientRegistration", "clientSecret"],
                &["clientRegistration", "client_secret"],
                &["registration", "clientSecret"],
                &["oidcClient", "clientSecret"],
            ],
        );
        let issuer_url = pick_string(
            Some(&auth_token),
            &[&["issuer_url"], &["issuerUrl"], &["issuer"]],
        );
        let scopes = pick_string(Some(&auth_token), &[&["scopes"], &["scope"]]);
        let login_hint = pick_string(Some(&auth_token), &[&["login_hint"], &["loginHint"]])
            .or_else(|| normalize_non_empty(Some(email.as_str())));

        let auth_method = if detect_idc(&auth_token, login_provider.as_deref(), &idc_region, &client_id, &client_secret) {
            "idc".to_string()
        } else {
            "social".to_string()
        };

        Ok(Self {
            access_token,
            refresh_token,
            token_type,
            expires_at,
            email,
            user_id,
            login_provider,
            auth_method,
            idc_region,
            issuer_url,
            client_id,
            client_secret,
            scopes,
            login_hint,
            profile_arn,
            raw_auth_token: auth_token,
        })
    }
}

/// 根据 storage 推导一个稳定的账号 id（DB 主键 + 列表展示用）。
pub fn derive_kiro_account_id(data: &KiroTokenData) -> String {
    let email = data.email.trim();
    if !email.is_empty() {
        return format!("kiro-{}", email.replace('/', "_"));
    }
    // 没邮箱就用 access_token 的指纹兜底，保证幂等
    use sha2::{Digest, Sha256};
    let mut h = Sha256::default();
    h.update(data.access_token.as_bytes());
    if let Some(rt) = data.refresh_token.as_deref() {
        h.update(rt.as_bytes());
    }
    let digest = h.finalize();
    format!("kiro-acc-{}", &hex::encode(digest)[..12])
}

/// 判断是否走 IdC / Builder-ID 刷新流。
fn detect_idc(
    auth_token: &Value,
    login_provider: Option<&str>,
    idc_region: &Option<String>,
    client_id: &Option<String>,
    client_secret: &Option<String>,
) -> bool {
    let auth_method_is_idc = normalize_ascii_lower(
        pick_string(Some(auth_token), &[&["authMethod"], &["auth_method"]]).as_deref(),
    )
    .map(|v| v == "idc")
    .unwrap_or(false);

    let provider_is_idc = normalize_ascii_lower(
        pick_string(
            Some(auth_token),
            &[&["provider"], &["loginProvider"], &["login_option"]],
        )
        .as_deref(),
    )
    .map(|v| {
        matches!(
            v.as_str(),
            "enterprise" | "builderid" | "internal" | "awsidc" | "external_idp"
        )
    })
    .unwrap_or(false);

    let login_provider_is_idc = normalize_ascii_lower(login_provider)
        .map(|v| matches!(v.as_str(), "enterprise" | "builderid" | "internal" | "awsidc"))
        .unwrap_or(false);

    // 有完整 IdC 凭据（region + client_id + client_secret）也按 IdC 处理
    let has_idc_material = idc_region.is_some() && client_id.is_some() && client_secret.is_some();

    auth_method_is_idc || provider_is_idc || login_provider_is_idc || has_idc_material
}

fn resolve_idc_region(auth_token: &Value, profile_arn: Option<&str>) -> Option<String> {
    pick_string(
        Some(auth_token),
        &[&["idc_region"], &["idcRegion"], &["region"]],
    )
    .or_else(|| profile_arn.and_then(parse_profile_arn_region))
}

// ===== region / endpoint =====

/// 根据 region 解析 Kiro runtime（getUsageLimits）endpoint。
pub fn runtime_endpoint_for_region(region: Option<&str>) -> String {
    let region = region.unwrap_or("us-east-1").trim().to_ascii_lowercase();
    match region.as_str() {
        "us-east-1" => "https://q.us-east-1.amazonaws.com".to_string(),
        "eu-central-1" => "https://q.eu-central-1.amazonaws.com".to_string(),
        "us-gov-east-1" => "https://q-fips.us-gov-east-1.amazonaws.com".to_string(),
        "us-gov-west-1" => "https://q-fips.us-gov-west-1.amazonaws.com".to_string(),
        "us-iso-east-1" => "https://q.us-iso-east-1.c2s.ic.gov".to_string(),
        "us-isob-east-1" => "https://q.us-isob-east-1.sc2s.sgov.gov".to_string(),
        "us-isof-south-1" => "https://q.us-isof-south-1.csp.hci.ic.gov".to_string(),
        "us-isof-east-1" => "https://q.us-isof-east-1.csp.hci.ic.gov".to_string(),
        _ => KIRO_RUNTIME_DEFAULT_ENDPOINT.to_string(),
    }
}

pub fn parse_profile_arn_region(profile_arn: &str) -> Option<String> {
    let mut segments = profile_arn.split(':');
    let prefix = segments.next()?.trim();
    if !prefix.eq_ignore_ascii_case("arn") {
        return None;
    }
    let _partition = segments.next()?;
    let _service = segments.next()?;
    let region = segments.next()?.trim();
    if region.is_empty() {
        None
    } else {
        Some(region.to_string())
    }
}

// ===== 通用 JSON 提取助手（从 cockpit-tools 移植，保持路径一致）=====

pub fn get_path_value<'a>(root: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = root;
    for key in path {
        current = current.as_object()?.get(*key)?;
    }
    Some(current)
}

pub fn pick_string(root: Option<&Value>, paths: &[&[&str]]) -> Option<String> {
    let root = root?;
    for path in paths {
        if let Some(value) = get_path_value(root, path) {
            if let Some(text) = value.as_str() {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
            if let Some(num) = value.as_i64() {
                return Some(num.to_string());
            }
            if let Some(num) = value.as_u64() {
                return Some(num.to_string());
            }
        }
    }
    None
}

pub fn pick_number(root: Option<&Value>, paths: &[&[&str]]) -> Option<f64> {
    let root = root?;
    for path in paths {
        if let Some(value) = get_path_value(root, path) {
            if let Some(num) = value.as_f64() {
                if num.is_finite() {
                    return Some(num);
                }
            }
            if let Some(text) = value.as_str() {
                if let Ok(num) = text.trim().parse::<f64>() {
                    if num.is_finite() {
                        return Some(num);
                    }
                }
            }
        }
    }
    None
}

fn extract_profile_arn(auth_token: Option<&Value>, profile: Option<&Value>) -> Option<String> {
    pick_string(
        profile,
        &[&["arn"], &["profileArn"], &["profile", "arn"], &["account", "arn"]],
    )
    .or_else(|| pick_string(auth_token, &[&["profileArn"], &["profile_arn"], &["arn"]]))
}

fn decode_jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice::<Value>(&decoded).ok()
}

pub fn normalize_email(value: Option<String>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() || !trimmed.contains('@') {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

pub fn normalize_non_empty(value: Option<&str>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn normalize_ascii_lower(value: Option<&str>) -> Option<String> {
    normalize_non_empty(value).map(|raw| raw.to_ascii_lowercase())
}

fn provider_from_login_option(login_option: &str) -> Option<String> {
    match login_option.trim().to_ascii_lowercase().as_str() {
        "google" => Some("Google".to_string()),
        "github" => Some("Github".to_string()),
        _ => None,
    }
}

/// 把各种时间表示统一成 unix 秒。毫秒会被压缩到秒。
pub fn parse_timestamp(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(seconds) = value.as_i64() {
        return normalize_timestamp(seconds);
    }
    if let Some(seconds) = value.as_u64() {
        return normalize_timestamp(seconds as i64);
    }
    if let Some(seconds) = value.as_f64() {
        if seconds.is_finite() {
            return normalize_timestamp(seconds.round() as i64);
        }
    }
    if let Some(text) = value.as_str() {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Ok(num) = trimmed.parse::<i64>() {
            return normalize_timestamp(num);
        }
        if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
            return Some(dt.timestamp());
        }
        if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S") {
            return Some(parsed.and_utc().timestamp());
        }
        if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y/%m/%d %H:%M:%S") {
            return Some(parsed.and_utc().timestamp());
        }
    }
    None
}

fn normalize_timestamp(raw: i64) -> Option<i64> {
    if raw <= 0 {
        return None;
    }
    if raw > 10_000_000_000 {
        return Some(raw / 1000); // 毫秒 -> 秒
    }
    Some(raw)
}

fn unix_seconds_to_dt(secs: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(secs, 0).unwrap_or_else(Utc::now)
}
