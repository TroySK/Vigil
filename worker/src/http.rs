//! HTTP routing: auth gate, then dispatch to pages or JSON APIs.

use crate::api;
use crate::auth::{authed, deny};
use crate::db::Db;
use worker::{Env, Request, Response, Result};

/// Handle every incoming `fetch`: gate, migrate, then route by path.
pub async fn handle(req: Request, env: Env) -> Result<Response> {
    let url = req.url()?;
    // Brand assets stay public so browsers can fetch the tab icon and the
    // header logo without credentials.
    match url.path() {
        "/favicon.ico" => return crate::icons::favicon(),
        "/logo.svg" => return crate::icons::logo(),
        _ => {}
    }
    let site = vigil::site();
    if !authed(&req, &site, &env) {
        return deny();
    }
    let db = Db::of(&env)?;
    db.init().await?;
    match url.path() {
        "/" | "/index.html" => crate::page::render(&db, &site).await,
        "/monitor" => match detail_id(&url) {
            Some(id) => crate::detail::render(&db, &site, &id).await,
            None => Response::error("missing id", 400),
        },
        "/api/health" => api::health(&db, &site).await,
        "/api/badge" => api::badge(&db, &site, &url).await,
        "/api/history" => api::history(&db, &url).await,
        "/api/incidents" => api::incidents(&db, &url).await,
        "/api/latency" => api::latency(&db, &url).await,
        _ => Response::error("not found", 404),
    }
}

/// Extract `?id=` from a detail/API URL.
fn detail_id(url: &url::Url) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == "id")
        .map(|(_, value)| value.into_owned())
}
