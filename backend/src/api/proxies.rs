use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;

use crate::proxy_pool::ProxyEntry;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct CreatePayload {
    pub url: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Deserialize)]
pub struct UpdatePayload {
    pub url: Option<String>,
    pub label: Option<String>,
}

pub async fn list(State(app): State<Arc<AppState>>) -> Json<Vec<ProxyEntry>> {
    Json(app.proxy_pool.list())
}

pub async fn create(
    State(app): State<Arc<AppState>>,
    Json(payload): Json<CreatePayload>,
) -> Response {
    match app.proxy_pool.add(payload.url, payload.label) {
        Ok(entry) => {
            info!(proxy_id = %entry.id, "proxy added");
            (StatusCode::CREATED, Json(entry)).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

pub async fn update(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<UpdatePayload>,
) -> Response {
    // 编辑前先拿到旧 url，编辑后让连接池失效，避免脏连接复用
    let old_url = app
        .proxy_pool
        .list()
        .into_iter()
        .find(|p| p.id == id)
        .map(|p| p.url);
    if let Err(e) = app.proxy_pool.update(&id, payload.url, payload.label) {
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    if let Some(u) = old_url {
        app.clients.invalidate(&u);
    }
    Json(json!({"ok": true})).into_response()
}

pub async fn delete_one(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let url = app.proxy_pool.url_by_id(&id);
    match app.proxy_pool.remove(&id) {
        Ok(true) => {
            if let Some(u) = url {
                app.clients.invalidate(&u);
            }
            Json(json!({"ok": true})).into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "proxy not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize, Default)]
pub struct RebalancePayload {
    /// only_unassigned=true 仅给当前没绑代理的账号分配，
    /// 默认 false 表示把所有账号重新轮询分配（破坏现有绑定）。
    #[serde(default)]
    pub only_unassigned: bool,
}

#[derive(serde::Serialize)]
pub struct RebalanceResult {
    pub assigned: usize,
    pub skipped_no_proxies: bool,
    pub failed: Vec<String>,
}

pub async fn rebalance(
    State(app): State<Arc<AppState>>,
    Json(payload): Json<RebalancePayload>,
) -> Response {
    let proxies = app.proxy_pool.list();
    if proxies.is_empty() {
        return Json(RebalanceResult {
            assigned: 0,
            skipped_no_proxies: true,
            failed: vec![],
        })
        .into_response();
    }

    let ids = if payload.only_unassigned {
        app.pool.unassigned_ids()
    } else {
        app.pool.all_ids_sorted()
    };

    let mut assigned = 0;
    let mut failed = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        let url = proxies[i % proxies.len()].url.clone();
        match app.pool.set_proxy(id, url) {
            Ok(()) => assigned += 1,
            Err(e) => failed.push(format!("{id}: {e}")),
        }
    }
    info!(
        assigned,
        only_unassigned = payload.only_unassigned,
        "rebalance done"
    );
    Json(RebalanceResult {
        assigned,
        skipped_no_proxies: false,
        failed,
    })
    .into_response()
}

#[derive(Serialize)]
pub struct ProxyTestResult {
    pub ok: bool,
    pub latency_ms: u128,
    pub ip: Option<String>,
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub isp: Option<String>,
    pub org: Option<String>,
    pub asn: Option<String>,
    pub reverse: Option<String>,
    /// 0-100，越高越纯净（越像住宅 IP）
    pub purity_score: u8,
    pub purity_label: String,
    pub purity_reasons: Vec<String>,
    pub error: Option<String>,
}

#[derive(Deserialize)]
struct IpApiResp {
    status: Option<String>,
    message: Option<String>,
    query: Option<String>,
    country: Option<String>,
    #[serde(rename = "regionName")]
    region_name: Option<String>,
    city: Option<String>,
    isp: Option<String>,
    org: Option<String>,
    #[serde(rename = "as")]
    as_field: Option<String>,
    reverse: Option<String>,
    hosting: Option<bool>,
    proxy: Option<bool>,
    mobile: Option<bool>,
}

pub async fn test_one(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let url = match app.proxy_pool.url_by_id(&id) {
        Some(u) => u,
        None => return (StatusCode::NOT_FOUND, "proxy not found").into_response(),
    };

    let client = match app.clients.get(&url) {
        Ok(c) => c,
        Err(e) => {
            return Json(ProxyTestResult {
                ok: false,
                latency_ms: 0,
                ip: None,
                country: None,
                region: None,
                city: None,
                isp: None,
                org: None,
                asn: None,
                reverse: None,
                purity_score: 0,
                purity_label: "未知".into(),
                purity_reasons: vec![],
                error: Some(format!("build proxied client failed: {e}")),
            })
            .into_response();
        }
    };

    // ip-api.com 免费接口，fields 里加上 proxy/hosting/mobile/reverse，便于纯净度判断
    let endpoint =
        "http://ip-api.com/json/?fields=status,message,query,country,regionName,city,isp,org,as,reverse,hosting,proxy,mobile";

    let started = Instant::now();
    let resp = client
        .get(endpoint)
        .timeout(Duration::from_secs(15))
        .send()
        .await;
    let latency_ms = started.elapsed().as_millis();

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            return Json(ProxyTestResult {
                ok: false,
                latency_ms,
                ip: None,
                country: None,
                region: None,
                city: None,
                isp: None,
                org: None,
                asn: None,
                reverse: None,
                purity_score: 0,
                purity_label: "不可用".into(),
                purity_reasons: vec![],
                error: Some(e.to_string()),
            })
            .into_response();
        }
    };

    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Json(ProxyTestResult {
            ok: false,
            latency_ms,
            ip: None,
            country: None,
            region: None,
            city: None,
            isp: None,
            org: None,
            asn: None,
            reverse: None,
            purity_score: 0,
            purity_label: "不可用".into(),
            purity_reasons: vec![],
            error: Some(format!("http {code}: {}", body.chars().take(200).collect::<String>())),
        })
        .into_response();
    }

    let data: IpApiResp = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return Json(ProxyTestResult {
                ok: false,
                latency_ms,
                ip: None,
                country: None,
                region: None,
                city: None,
                isp: None,
                org: None,
                asn: None,
                reverse: None,
                purity_score: 0,
                purity_label: "不可用".into(),
                purity_reasons: vec![],
                error: Some(format!("parse ip-api response: {e}")),
            })
            .into_response();
        }
    };

    if data.status.as_deref() != Some("success") {
        return Json(ProxyTestResult {
            ok: false,
            latency_ms,
            ip: data.query,
            country: None,
            region: None,
            city: None,
            isp: None,
            org: None,
            asn: None,
            reverse: None,
            purity_score: 0,
            purity_label: "不可用".into(),
            purity_reasons: vec![],
            error: data.message.or_else(|| Some("ip-api returned non-success".into())),
        })
        .into_response();
    }

    let (purity_score, purity_label, purity_reasons) = score_purity(&data, latency_ms);

    info!(
        proxy_id = %id,
        ip = ?data.query,
        country = ?data.country,
        purity = purity_score,
        latency_ms,
        "proxy test ok"
    );

    Json(ProxyTestResult {
        ok: true,
        latency_ms,
        ip: data.query,
        country: data.country,
        region: data.region_name,
        city: data.city,
        isp: data.isp,
        org: data.org,
        asn: data.as_field,
        reverse: data.reverse,
        purity_score,
        purity_label,
        purity_reasons,
        error: None,
    })
    .into_response()
}

/// 纯净度评分：从 100 分扣，扣分项 = 命中机房/IDC 关键字 / ip-api 标记 hosting/proxy / 反查为空。
/// 越像住宅/移动 IP 分越高。
fn score_purity(d: &IpApiResp, latency_ms: u128) -> (u8, String, Vec<String>) {
    let mut score: i32 = 100;
    let mut reasons: Vec<String> = Vec::new();

    if d.proxy == Some(true) {
        score -= 60;
        reasons.push("ip-api 标记为 proxy/VPN/Tor 出口".into());
    }
    if d.hosting == Some(true) {
        score -= 35;
        reasons.push("ip-api 标记为机房 / 数据中心 IP".into());
    }
    if d.mobile == Some(true) {
        score += 5;
        reasons.push("ip-api 标记为移动网络".into());
    }

    let blob = format!(
        "{} {} {}",
        d.isp.clone().unwrap_or_default(),
        d.org.clone().unwrap_or_default(),
        d.as_field.clone().unwrap_or_default(),
    )
    .to_lowercase();

    // 常见机房 / 云厂商 / VPS 关键字
    const HOST_KW: &[&str] = &[
        "amazon", "aws", "google", "gcp", "microsoft", "azure", "oracle",
        "digitalocean", "linode", "vultr", "ovh", "hetzner", "leaseweb",
        "choopa", "contabo", "datacamp", "m247", "psychz", "host", "hosting",
        "server", "cloud", "idc", "data center", "datacenter", "colocation",
        "tencent", "alibaba", "aliyun", "huawei cloud", "ucloud",
    ];
    let mut hit_host = false;
    for kw in HOST_KW {
        if blob.contains(kw) {
            hit_host = true;
            reasons.push(format!("ISP/ASN 含机房关键字 \"{kw}\""));
            break;
        }
    }
    if hit_host && d.hosting != Some(true) {
        score -= 25;
    }

    // 住宅 ISP 关键字（加分）
    const RESI_KW: &[&str] = &[
        "comcast", "verizon", "at&t", "att ", "spectrum", "charter", "cox",
        "telecom", "telekom", "broadband", "cable", "fiber", "fttx", "fios",
        "chinanet", "china unicom", "china telecom", "china mobile", "cmcc",
        "softbank", "kddi", "ntt", "korea telecom", "kt corp", "lg uplus",
        "bt ", "virgin media", "deutsche telekom", "vodafone", "orange",
    ];
    for kw in RESI_KW {
        if blob.contains(kw) {
            score += 10;
            reasons.push(format!("ISP 含住宅/电信关键字 \"{kw}\""));
            break;
        }
    }

    if d.reverse.as_deref().map(str::is_empty).unwrap_or(true) {
        score -= 5;
        reasons.push("反向 DNS 为空".into());
    }

    if latency_ms > 3000 {
        score -= 10;
        reasons.push(format!("延迟较高 {latency_ms}ms"));
    } else if latency_ms > 1500 {
        score -= 5;
        reasons.push(format!("延迟偏高 {latency_ms}ms"));
    }

    let score = score.clamp(0, 100) as u8;
    let label = match score {
        80..=100 => "优秀（疑似住宅）",
        60..=79 => "良好",
        40..=59 => "一般",
        20..=39 => "较差（疑似机房）",
        _ => "很差（机房/代理）",
    }
    .to_string();

    (score, label, reasons)
}
