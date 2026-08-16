//! Bearer-token auth + RBAC permission keys (spec §64).
//! Roles: admin / operator / viewer, each with default permission keys;
//! explicit `permissions` on a token extend the role's set.

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use config::{ApiConfig, ApiToken};

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub name: String,
    pub permissions: Vec<String>,
}

pub const PERMS: &[&str] = &[
    "jobs.read",
    "jobs.write",
    "runs.manage",
    "workers.read",
    "workers.write",
    "logs.read",
];

fn role_defaults(role: &str) -> &'static [&'static str] {
    match role {
        "admin" => PERMS,
        "operator" => &[
            "jobs.read",
            "jobs.write",
            "runs.manage",
            "workers.read",
            "logs.read",
        ],
        "viewer" => &["jobs.read", "workers.read", "logs.read"],
        _ => &[],
    }
}

fn find_token<'a>(cfg: &'a ApiConfig, token: &str) -> Option<&'a ApiToken> {
    cfg.tokens
        .iter()
        .find(|t| constant_time_eq(t.token.as_bytes(), token.as_bytes()))
}

/// Constant-time byte comparison (spec §64 — token compares must not leak
/// via early exit).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Extracts the Bearer token → AuthUser, or 401.
pub fn authenticate(
    cfg: &ApiConfig,
    headers: &axum::http::HeaderMap,
) -> Result<AuthUser, StatusCode> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let t = find_token(cfg, value).ok_or(StatusCode::UNAUTHORIZED)?;
    let mut permissions: Vec<String> = role_defaults(&t.role)
        .iter()
        .map(|s| s.to_string())
        .collect();
    for p in &t.permissions {
        if !permissions.iter().any(|x| x == p) {
            permissions.push(p.clone());
        }
    }
    Ok(AuthUser {
        name: t.name.clone(),
        permissions,
    })
}

pub fn has_perm(user: &AuthUser, perm: &str) -> bool {
    user.permissions.iter().any(|p| p == perm)
}

/// Axum middleware: authenticate and attach the user to request extensions.
pub async fn require_auth(
    axum::extract::State(state): axum::extract::State<crate::router::AppState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let cfg = state.engine.cfg.api.clone();
    let user = authenticate(&cfg, req.headers())?;
    let mut req = req;
    req.extensions_mut().insert(user);
    Ok(next.run(req).await)
}

/// Guard a handler by permission; 403 when missing.
pub fn require(user: &AuthUser, perm: &str) -> Result<(), StatusCode> {
    if has_perm(user, perm) {
        Ok(())
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}
