//! Kiro 账号的 token 数据模型 + 解析助手。
//!
//! 仿 D:/java_projects/kiro-account-manager（KAM）的 Account 模型：把任意来源的 JSON
//! （Kiro IDE 本地 `~/.aws/sso/cache/kiro-auth-token.json`、KAM 导出的账号对象、AWS
//! OIDC `/token` 响应、Kiro Desktop `/refreshToken` 响应）统一成 [`KiroTokenData`]。
//!
//! 与之前实现的不同点：
//! - **provider 显式化**：Google / Github / BuilderId / Enterprise 直接落到字段，
//!   不再由 auth_method 反推（IDE 真实缓存就是显式存的）。
//! - **auth_method 派生规则**：provider 一旦确定，auth_method 自动跟随
//!   （Google/Github → social；BuilderId/Enterprise → IdC）。
//! - **start_url / client_id_hash 显式存储**：企业 SSO 切号 / 加密文件名都依赖这俩，
//!   不能再每次现算。

use base64::Engine;
use chrono::{DateTime, Utc};
use serde_json::Value;

/// Kiro runtime（CodeWhisperer / Q）默认 endpoint，按 region 解析失败时兜底。
pub const KIRO_RUNTIME_DEFAULT_ENDPOINT: &str = "https://q.us-east-1.amazonaws.com";

/// 账号状态枚举 —— 与 KAM 对齐：active / banned / invalid / capped / overage / error。
pub const KIRO_STATUS_ACTIVE: &str = "active";
pub const KIRO_STATUS_BANNED: &str = "banned";
pub const KIRO_STATUS_INVALID: &str = "invalid";
pub const KIRO_STATUS_CAPPED: &str = "capped";
pub const KIRO_STATUS_OVERAGE: &str = "overage";
pub const KIRO_STATUS_ERROR: &str = "error";

/// `auth_method` 字面量 —— 与 KAM / Kiro IDE token 文件一致。
pub const KIRO_AUTH_METHOD_SOCIAL: &str = "social";
pub const KIRO_AUTH_METHOD_IDC: &str = "IdC";

/// `provider` 字面量。
pub const KIRO_PROVIDER_GOOGLE: &str = "Google";
pub const KIRO_PROVIDER_GITHUB: &str = "Github";
pub const KIRO_PROVIDER_BUILDER_ID: &str = "BuilderId";
pub const KIRO_PROVIDER_ENTERPRISE: &str = "Enterprise";

/// BuilderId 默认 SSO Start URL（KAM `KIRO_BUILDER_ID_START_URL`）。
pub const KIRO_BUILDER_ID_START_URL: &str = "https://view.awsapps.com/start";
/// BuilderId 的固定 clientIdHash（与 `sha1(JSON.stringify({ startUrl: BUILDER_ID_START_URL }))` 一致）。
pub const KIRO_BUILDER_ID_CLIENT_ID_HASH: &str = "e909a0580879b06ece1202964fbe9dda95ea4ce3";

/// 解析后的规整 token 数据。token 字段交给上层落库（serde skip），不在这里序列化。
#[derive(Debug, Clone)]
pub struct KiroTokenData {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,

    pub email: String,
    pub user_id: Option<String>,

    /// "Google" / "Github" / "BuilderId" / "Enterprise"。
    pub provider: String,
    /// "social" 或 "IdC"，与 provider 派生一致。
    pub auth_method: String,

    pub idc_region: Option<String>,
    pub issuer_url: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub client_id_hash: Option<String>,
    pub start_url: Option<String>,
    pub scopes: Option<String>,
    pub login_hint: Option<String>,
    pub profile_arn: Option<String>,
    pub id_token: Option<String>,
    pub sso_session_id: Option<String>,
    pub machine_id: Option<String>,

    /// 原始 auth token JSON（合并了 kiro_auth_token_raw 等多来源），刷新 / 调试时保留。
    pub raw_auth_token: Value,
    /// 原始 usage（getUsageLimits 响应或导入时附带的 usage_data）。
    pub raw_usage: Value,
}

impl KiroTokenData {
    /// 从任意 Kiro 授权 JSON 解析。接受三类来源：
    /// 1. Kiro IDE `kiro-auth-token.json`（顶层就是 token 对象）；
    /// 2. KAM / cockpit 导出的账号对象（snake_case 顶层字段，可能带 `usageData`）；
    /// 3. `/refreshToken` 或 AWS OIDC `/token` 响应（camelCase）。
    pub fn from_value(input: &Value) -> Result<Self, String> {
        // 合并：以输入顶层为主，再补上 kiro_auth_token_raw 里缺的键（兼容旧导出格式）。
        let mut auth_token = input.clone();
        if let Some(obj) = auth_token.as_object_mut() {
            if let Some(raw) = input.get("kiro_auth_token_raw").and_then(|v| v.as_object()) {
                for (k, v) in raw {
                    obj.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }

        let profile = input.get("kiro_profile_raw");
        // KAM Account 把 usage_data 作为顶层字段；导入时一并保留。
        let raw_usage = input
            .get("usage_data")
            .or_else(|| input.get("usageData"))
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));

        let access_token = pick_string(
            Some(&auth_token),
            &[
                &["accessToken"],
                &["access_token"],
                &["token"],
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

        // 解码 JWT claims，给 email / user_id 多一条兜底来源。
        let id_token_claims = pick_string(
            Some(&auth_token),
            &[&["idToken"], &["id_token"], &["idTokenJwt"], &["id_token_jwt"]],
        )
        .and_then(|raw| decode_jwt_claims(&raw));
        let access_token_claims = decode_jwt_claims(&access_token);

        // 优先从 usage_data.userInfo 抽 email/userId（KAM 的真相源）。
        let usage_user_info = raw_usage.get("userInfo");
        let email = normalize_email(pick_string(
            usage_user_info,
            &[&["email"], &["primaryEmail"]],
        ))
        .or_else(|| {
            normalize_email(pick_string(
                profile,
                &[
                    &["email"],
                    &["user", "email"],
                    &["account", "email"],
                    &["primaryEmail"],
                ],
            ))
        })
        .or_else(|| {
            normalize_email(pick_string(
                Some(&auth_token),
                &[&["email"], &["userEmail"]],
            ))
        })
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
            usage_user_info,
            &[&["userId"], &["id"], &["sub"]],
        )
        .or_else(|| {
            pick_string(
                profile,
                &[&["userId"], &["user_id"], &["id"], &["sub"], &["account", "id"]],
            )
        })
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

        // 显式 provider 字段（snake_case / camelCase 都接收）。
        let raw_provider = pick_string(
            Some(&auth_token),
            &[&["provider"], &["loginProvider"], &["login_provider"], &["login_option"]],
        );

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

        // start_url：账号自带优先；否则尝试从 clientSecret JWT 解出来。
        let start_url_explicit = pick_string(
            Some(&auth_token),
            &[&["start_url"], &["startUrl"]],
        )
        .map(|v| normalize_start_url(&v));
        let start_url_from_secret = client_secret
            .as_deref()
            .and_then(extract_start_url_from_client_secret);
        let start_url = start_url_explicit.or(start_url_from_secret);

        // client_id_hash：账号自带优先；否则按 start_url 现算（BuilderId 缺 url 时用常量）。
        let client_id_hash_explicit = pick_string(
            Some(&auth_token),
            &[&["client_id_hash"], &["clientIdHash"]],
        );

        let region_explicit = pick_string(
            Some(&auth_token),
            &[&["idc_region"], &["idcRegion"], &["region"]],
        );

        let id_token_field = pick_string(
            Some(&auth_token),
            &[&["idToken"], &["id_token"], &["idTokenJwt"], &["id_token_jwt"]],
        );
        let sso_session_id = pick_string(
            Some(&auth_token),
            &[&["sso_session_id"], &["ssoSessionId"], &["aws_sso_app_session_id"]],
        );
        let machine_id = pick_string(
            Some(&auth_token),
            &[&["machine_id"], &["machineId"]],
        );

        // === 推导 provider + auth_method ===
        // 优先级：原始 provider 字符串（normalize 一次）→ start_url / client_secret 形态推断 →
        // 邮箱形态兜底（gmail/github 为 social；否则 BuilderId）。
        let normalized_provider = raw_provider
            .as_deref()
            .and_then(normalize_provider_name);

        let auth_method_hint = pick_string(
            Some(&auth_token),
            &[&["authMethod"], &["auth_method"]],
        )
        .map(|v| v.to_ascii_lowercase());

        let provider = normalized_provider
            .clone()
            .or_else(|| infer_provider_from_start_url(start_url.as_deref()))
            .or_else(|| infer_provider_from_client_secret(client_secret.as_deref()))
            .or_else(|| {
                // 有 IdC 凭据 → BuilderId 兜底；否则按 email/auth_method 推断
                if client_id.is_some()
                    && client_secret.is_some()
                    && idc_credential_implied(&auth_method_hint)
                {
                    Some(KIRO_PROVIDER_BUILDER_ID.to_string())
                } else if let Some(p) = infer_provider_from_email(&email) {
                    Some(p)
                } else if matches!(auth_method_hint.as_deref(), Some("idc")) {
                    Some(KIRO_PROVIDER_BUILDER_ID.to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| KIRO_PROVIDER_GOOGLE.to_string());

        let auth_method = auth_method_for_provider(&provider).to_string();

        // 区域：显式字段 > profile_arn 解析；IdC 缺则后续刷新会报错。
        let idc_region = region_explicit
            .or_else(|| profile_arn.as_deref().and_then(parse_profile_arn_region));

        // 计算最终 client_id_hash：仅 IdC 才需要。
        let client_id_hash = if auth_method == KIRO_AUTH_METHOD_IDC {
            client_id_hash_explicit
                .or_else(|| start_url.as_deref().map(calculate_client_id_hash))
                .or_else(|| {
                    if provider == KIRO_PROVIDER_BUILDER_ID {
                        Some(KIRO_BUILDER_ID_CLIENT_ID_HASH.to_string())
                    } else {
                        None
                    }
                })
        } else {
            None
        };

        Ok(Self {
            access_token,
            refresh_token,
            token_type,
            expires_at,
            email,
            user_id,
            provider,
            auth_method,
            idc_region,
            issuer_url,
            client_id,
            client_secret,
            client_id_hash,
            start_url,
            scopes,
            login_hint,
            profile_arn,
            id_token: id_token_field,
            sso_session_id,
            machine_id,
            raw_auth_token: auth_token,
            raw_usage,
        })
    }
}

/// 根据 storage 推导一个稳定的账号 id（DB 主键 + 列表展示用）。
///
/// 策略与 KAM 一致的优先级：user_id > email > token 指纹。生成的 id 同时编码 provider，
/// 避免同一邮箱在不同 provider（Google/Github/BuilderId/Enterprise）下被误合并。
pub fn derive_kiro_account_id(data: &KiroTokenData) -> String {
    let provider_slug = data.provider.to_ascii_lowercase();
    let user_id = data.user_id.as_deref().map(str::trim).unwrap_or("");
    if !user_id.is_empty() {
        return format!("kiro-{}-{}", provider_slug, sanitize_id(user_id));
    }
    let email = data.email.trim();
    if !email.is_empty() {
        return format!("kiro-{}-{}", provider_slug, sanitize_id(email));
    }
    use sha2::{Digest, Sha256};
    let mut h = Sha256::default();
    h.update(data.access_token.as_bytes());
    if let Some(rt) = data.refresh_token.as_deref() {
        h.update(rt.as_bytes());
    }
    let digest = h.finalize();
    format!("kiro-{}-acc-{}", provider_slug, &hex::encode(digest)[..12])
}

fn sanitize_id(s: &str) -> String {
    s.replace('/', "_")
}

/// 给定 provider，返回它对应的 auth_method（与 KAM 完全一致）。
pub fn auth_method_for_provider(provider: &str) -> &'static str {
    match provider {
        KIRO_PROVIDER_BUILDER_ID | KIRO_PROVIDER_ENTERPRISE => KIRO_AUTH_METHOD_IDC,
        _ => KIRO_AUTH_METHOD_SOCIAL,
    }
}

/// 把任意大小写形态的 provider 字符串规范化成枚举形态。
pub fn normalize_provider_name(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let lower = s.to_ascii_lowercase();
    match lower.as_str() {
        "google" => Some(KIRO_PROVIDER_GOOGLE.to_string()),
        "github" => Some(KIRO_PROVIDER_GITHUB.to_string()),
        "builderid" | "builder-id" | "builder_id" => Some(KIRO_PROVIDER_BUILDER_ID.to_string()),
        "enterprise" | "external_idp" | "internal" | "awsidc" => {
            Some(KIRO_PROVIDER_ENTERPRISE.to_string())
        }
        _ => None,
    }
}

fn infer_provider_from_start_url(start_url: Option<&str>) -> Option<String> {
    let url = start_url?.trim();
    if url.is_empty() {
        return None;
    }
    if is_builder_id_start_url(url) {
        Some(KIRO_PROVIDER_BUILDER_ID.to_string())
    } else if url.contains("awsapps.com") {
        Some(KIRO_PROVIDER_ENTERPRISE.to_string())
    } else {
        None
    }
}

fn infer_provider_from_client_secret(secret: Option<&str>) -> Option<String> {
    let url = extract_start_url_from_client_secret(secret?)?;
    infer_provider_from_start_url(Some(&url))
}

fn infer_provider_from_email(email: &str) -> Option<String> {
    let e = email.trim().to_ascii_lowercase();
    if e.is_empty() {
        return None;
    }
    if e.ends_with("@gmail.com") || e.ends_with(".gserviceaccount.com") {
        return Some(KIRO_PROVIDER_GOOGLE.to_string());
    }
    if e.contains("github") {
        return Some(KIRO_PROVIDER_GITHUB.to_string());
    }
    None
}

/// 当账号字段里写了 IdC（authMethod=idc）或没明确写但带了 IdC 凭据时，认为 provider 应是 IdC 系。
fn idc_credential_implied(auth_method_hint: &Option<String>) -> bool {
    matches!(auth_method_hint.as_deref(), Some("idc")) || auth_method_hint.is_none()
}

/// 是否为 BuilderId 默认 start url（去尾斜杠后比较）。
pub fn is_builder_id_start_url(start_url: &str) -> bool {
    start_url.trim().trim_end_matches('/') == KIRO_BUILDER_ID_START_URL.trim_end_matches('/')
}

/// 规范化 start_url：trim + 去尾部斜杠。和 KAM 一致。
pub fn normalize_start_url(start_url: &str) -> String {
    start_url.trim().trim_end_matches('/').to_string()
}

/// 按 Kiro IDE 算法计算 clientIdHash：`sha1(JSON.stringify({ startUrl: normalize(url) }))`。
pub fn calculate_client_id_hash(start_url: &str) -> String {
    use sha1::{Digest, Sha1};
    let normalized = normalize_start_url(start_url);
    let input = serde_json::json!({ "startUrl": normalized }).to_string();
    let mut h = Sha1::new();
    h.update(input.as_bytes());
    hex::encode(h.finalize())
}

/// 从 clientSecret（JWT）payload 里抽出 startUrl。
pub fn extract_start_url_from_client_secret(client_secret: &str) -> Option<String> {
    let parts: Vec<&str> = client_secret.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(parts[1]))
        .ok()?;
    let payload_str = String::from_utf8(decoded).ok()?;
    let payload_json: Value = serde_json::from_str(&payload_str).ok()?;
    let serialized_str = payload_json.get("serialized")?.as_str()?;
    let serialized: Value = serde_json::from_str(serialized_str).ok()?;
    serialized
        .get("initiateLoginUri")?
        .as_str()
        .map(normalize_start_url)
}

// ===== region / endpoint =====

/// 根据 region 解析 Kiro runtime（getUsageLimits）endpoint。
pub fn runtime_endpoint_for_region(region: Option<&str>) -> String {
    let region = region.unwrap_or("us-east-1").trim().to_ascii_lowercase();
    match region.as_str() {
        "us-gov-east-1" => "https://q-fips.us-gov-east-1.amazonaws.com".to_string(),
        "us-gov-west-1" => "https://q-fips.us-gov-west-1.amazonaws.com".to_string(),
        "us-iso-east-1" => "https://q.us-iso-east-1.c2s.ic.gov".to_string(),
        "us-isob-east-1" => "https://q.us-isob-east-1.sc2s.sgov.gov".to_string(),
        "us-isof-south-1" => "https://q.us-isof-south-1.csp.hci.ic.gov".to_string(),
        "us-isof-east-1" => "https://q.us-isof-east-1.csp.hci.ic.gov".to_string(),
        "" => KIRO_RUNTIME_DEFAULT_ENDPOINT.to_string(),
        other => format!("https://q.{other}.amazonaws.com"),
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

// ===== 通用 JSON 提取助手 =====

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_id_client_id_hash_constant_matches_algorithm() {
        // 算法应与常量自洽：sha1(JSON.stringify({ startUrl: BUILDER_ID_START_URL }))
        let computed = calculate_client_id_hash(KIRO_BUILDER_ID_START_URL);
        assert_eq!(computed, KIRO_BUILDER_ID_CLIENT_ID_HASH);
    }

    #[test]
    fn builder_id_client_id_hash_ignores_trailing_slash() {
        let with_slash = calculate_client_id_hash("https://view.awsapps.com/start/");
        assert_eq!(with_slash, KIRO_BUILDER_ID_CLIENT_ID_HASH);
    }

    #[test]
    fn enterprise_client_id_hash_matches_real_kiro_value() {
        // 与 KAM 单测对齐：真实企业 d-90660ceab3 的 hash 必须算对。
        let h = calculate_client_id_hash("https://d-90660ceab3.awsapps.com/start");
        assert_eq!(h, "a96ec6ff09e0c558ceca191cdaa0ff2b0e4e3e35");
    }

    #[test]
    fn social_provider_inferred_from_email() {
        let v = serde_json::json!({
            "accessToken": "atk",
            "refreshToken": "rtk",
            "email": "alice@gmail.com",
        });
        let d = KiroTokenData::from_value(&v).unwrap();
        assert_eq!(d.provider, "Google");
        assert_eq!(d.auth_method, "social");
        assert_eq!(d.email, "alice@gmail.com");
    }

    #[test]
    fn explicit_provider_wins_over_email() {
        let v = serde_json::json!({
            "accessToken": "atk",
            "refreshToken": "rtk",
            "email": "alice@gmail.com",
            "provider": "Github",
        });
        let d = KiroTokenData::from_value(&v).unwrap();
        assert_eq!(d.provider, "Github");
        assert_eq!(d.auth_method, "social");
    }

    #[test]
    fn idc_inferred_from_credentials() {
        let v = serde_json::json!({
            "accessToken": "atk",
            "refreshToken": "rtk",
            "clientId": "cid",
            "clientSecret": "csec",
            "region": "us-east-1",
        });
        let d = KiroTokenData::from_value(&v).unwrap();
        assert_eq!(d.provider, "BuilderId");
        assert_eq!(d.auth_method, "IdC");
        // BuilderId 没显式 hash 时用常量兜底
        assert_eq!(
            d.client_id_hash.as_deref(),
            Some(KIRO_BUILDER_ID_CLIENT_ID_HASH)
        );
    }

    #[test]
    fn enterprise_inferred_from_start_url() {
        let v = serde_json::json!({
            "accessToken": "atk",
            "refreshToken": "rtk",
            "clientId": "cid",
            "clientSecret": "csec",
            "region": "us-east-1",
            "startUrl": "https://d-90660ceab3.awsapps.com/start/",
        });
        let d = KiroTokenData::from_value(&v).unwrap();
        assert_eq!(d.provider, "Enterprise");
        assert_eq!(d.auth_method, "IdC");
        // Enterprise 自动算出 hash（带尾斜杠会被规范化）
        assert_eq!(
            d.client_id_hash.as_deref(),
            Some("a96ec6ff09e0c558ceca191cdaa0ff2b0e4e3e35")
        );
        assert_eq!(d.start_url.as_deref(), Some("https://d-90660ceab3.awsapps.com/start"));
    }

    #[test]
    fn extracts_user_id_from_usage_data() {
        let v = serde_json::json!({
            "accessToken": "atk",
            "refreshToken": "rtk",
            "provider": "Enterprise",
            "clientId": "cid",
            "clientSecret": "csec",
            "region": "us-east-1",
            "usage_data": {
                "userInfo": { "userId": "u-123", "email": null }
            }
        });
        let d = KiroTokenData::from_value(&v).unwrap();
        assert_eq!(d.user_id.as_deref(), Some("u-123"));
        assert_eq!(d.email, "");
    }
}
