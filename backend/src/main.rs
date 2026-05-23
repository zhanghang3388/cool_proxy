mod api;
mod auth;
mod config;
mod pool;
mod proxy;
mod proxy_pool;
mod state;
mod store;

use std::path::PathBuf;
use std::sync::Arc;

use axum::routing::any;
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::auth::refresher::run_refresh_loop;
use crate::config::Config;
use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg_path: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.yaml"));

    let config = if cfg_path.exists() {
        Config::load(&cfg_path)?
    } else {
        let example = PathBuf::from("config.example.yaml");
        if !example.exists() {
            anyhow::bail!(
                "config not found: {:?}. copy config.example.yaml to config.yaml first",
                cfg_path
            );
        }
        eprintln!(
            "config {:?} not found, falling back to {:?}",
            cfg_path, example
        );
        Config::load(&example)?
    };

    init_logging(&config.log.level);
    info!("cool_proxy starting on {}", config.bind_addr());

    let config = Arc::new(config);
    let state = Arc::new(AppState::new(config.clone())?);

    // 后台 token 刷新
    {
        let cfg = config.clone();
        let p = state.pool.clone();
        let r = state.refresher.clone();
        tokio::spawn(run_refresh_loop(cfg, p, r));
    }

    let cors = CorsLayer::new()
        .allow_methods(Any)
        .allow_headers(Any)
        .allow_origin(Any);

    // /v1/* 反代（OpenAI 兼容）
    let proxy_router = Router::new()
        .route("/v1/*rest", any(proxy::proxy_handler))
        .with_state(state.clone());

    // /api/* 管理面板接口
    let admin_router = api::router(state.clone());

    let app = Router::new()
        .merge(proxy_router)
        .nest("/api", admin_router)
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .layer(TraceLayer::new_for_http())
        .layer(cors);

    let listener = tokio::net::TcpListener::bind(config.bind_addr()).await?;
    info!("listening on {}", config.bind_addr());
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn init_logging(level: &str) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("cool_proxy={level},tower_http=info,axum=info")));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown signal received");
}
