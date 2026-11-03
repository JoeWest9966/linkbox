//! Local control panel. Bound to 127.0.0.1 by default; if you want it
//! reachable from outside, front it with your existing gateway (panel-gate)
//! rather than widening the bind address.

use crate::config::{self, Config};
use crate::state::App;
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;
#[derive(Serialize)]
struct StateResponse {
    now: u64,
    started_at: u64,
    config_path: String,
    tunnels: Vec<crate::state::Status>,
}

pub fn router(app: Arc<App>, token: Option<String>) -> Router {
    let s = app.clone();
    Router::new()
        .route("/", get(index))
        .route("/api/state", get(api_state))
        .route("/api/config", get(api_get_config).post(api_set_config))
        .route("/api/tunnels/{name}/restart", post(api_restart))
        .route("/api/tunnels/{name}/enable", post(api_enable))
        .route("/api/tunnels/{name}/disable", post(api_disable))
        .with_state(AppState { app: s, token })
}

#[derive(Clone)]
struct AppState {
    app: Arc<App>,
    token: Option<String>,
}

fn authed(st: &AppState, headers: &HeaderMap) -> bool {
    let Some(expect) = &st.token else {
        return true;
    };
    let got = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v).trim().to_string());
    got.as_deref() == Some(expect.as_str())
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "missing or wrong token").into_response()
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../assets/index.html"))
}

async fn api_state(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !authed(&st, &headers) {
        return unauthorized();
    }
    let cfg = st.app.cfg.read().unwrap();
    let resp = StateResponse {
        now: crate::state::now_epoch(),
        started_at: st.app.started_at,
        config_path: st.app.config_path.display().to_string(),
        tunnels: st.app.snapshot(),
    };
    drop(cfg);
    Json(resp).into_response()
}

async fn api_get_config(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !authed(&st, &headers) {
        return unauthorized();
    }
    let text = st.app.config_text.read().unwrap().clone();
    Json(json!({ "text": text, "path": st.app.config_path.display().to_string() })).into_response()
}

async fn api_set_config(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !authed(&st, &headers) {
        return unauthorized();
    }
    let Some(text) = body.get("text").and_then(|v| v.as_str()) else {
        return (StatusCode::BAD_REQUEST, "body must be {\"text\": \"...\"}").into_response();
    };
    match Config::parse_text(text) {
        Err(e) => (StatusCode::BAD_REQUEST, format!("invalid config: {e}")).into_response(),
        Ok(cfg) => {
            if let Err(e) = config::write_atomic(&st.app.config_path, text) {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("write failed: {e}"))
                    .into_response();
            }
            st.app.apply_config(cfg, text.to_string());
            Json(json!({"ok": true})).into_response()
        }
    }
}

async fn api_restart(State(st): State<AppState>, headers: HeaderMap, AxPath(name): AxPath<String>) -> Response {
    if !authed(&st, &headers) {
        return unauthorized();
    }
    if st.app.resolve_named(&name).is_none() {
        return (StatusCode::NOT_FOUND, "no such tunnel").into_response();
    }
    let c = st.app.control_for(&name);
    c.restart.store(true, Ordering::Relaxed);
    Json(json!({"ok": true, "name": name})).into_response()
}

async fn api_enable(State(st): State<AppState>, headers: HeaderMap, AxPath(name): AxPath<String>) -> Response {
    set_enabled(st, headers, name, true)
}

async fn api_disable(State(st): State<AppState>, headers: HeaderMap, AxPath(name): AxPath<String>) -> Response {
    set_enabled(st, headers, name, false)
}

fn set_enabled(st: AppState, headers: HeaderMap, name: String, on: bool) -> Response {
    if !authed(&st, &headers) {
        return unauthorized();
    }
    let text = st.app.config_text.read().unwrap().clone();
    let new_text = match config::set_enabled_in_text(&text, &name, on) {
        Ok(t) => t,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("{e}")).into_response(),
    };
    match Config::parse_text(&new_text) {
        Err(e) => (StatusCode::BAD_REQUEST, format!("refusing to write: {e}")).into_response(),
        Ok(cfg) => {
            if let Err(e) = config::write_atomic(&st.app.config_path, &new_text) {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response();
            }
            let c = st.app.control_for(&name);
            c.config_epoch.fetch_add(1, Ordering::Relaxed);
            c.restart.store(true, Ordering::Relaxed);
            st.app.apply_config(cfg, new_text);
            Json(json!({"ok": true, "enabled": on})).into_response()
        }
    }
}
