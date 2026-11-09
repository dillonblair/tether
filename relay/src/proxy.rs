//! `/d/<device>/…`: a signed-in viewer's requests, carried to that computer's agent over a
//! tunnel the agent opens on request. Each request gets its own tunnel connection; the
//! stream's WebSocket is one long-lived request.

use crate::{new_token, AppState, User};
use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{header, HeaderName, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use serde_json::json;
use std::time::Duration;
use tokio::sync::oneshot;

pub async fn add_slash(Path(device): Path<String>) -> Redirect {
    // The client loads its scripts and socket relative to the page, so the page's
    // address has to end in a slash.
    Redirect::permanent(&format!("/d/{device}/"))
}

pub async fn root(st: State<AppState>, user: User, Path(device): Path<String>, req: Request) -> Response {
    forward(st, user, device, String::new(), req).await
}

pub async fn path(
    st: State<AppState>,
    user: User,
    Path((device, rest)): Path<(String, String)>,
    req: Request,
) -> Response {
    forward(st, user, device, rest, req).await
}

fn fail(code: StatusCode, msg: &'static str) -> Response {
    (code, msg).into_response()
}

/// Whether a browser's request comes from one of the relay's own pages. Programs that
/// aren't browsers send no `Origin`.
fn same_origin(req: &Request) -> bool {
    let Some(origin) = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok());
    match (origin.split_once("://"), host) {
        (Some((_, o)), Some(h)) => o.eq_ignore_ascii_case(h),
        _ => false,
    }
}

async fn forward(
    State(st): State<AppState>,
    User(user): User,
    device: String,
    rest: String,
    mut req: Request,
) -> Response {
    if !st.db.owns(user, &device) {
        return fail(StatusCode::NOT_FOUND, "no such computer");
    }
    if !same_origin(&req) {
        return fail(StatusCode::FORBIDDEN, "requests from other sites are refused");
    }
    let (tx, port, password) = match st.agents.lock().get(&device) {
        Some(a) => (a.tx.clone(), a.port, a.password.clone()),
        None => return fail(StatusCode::BAD_GATEWAY, "that computer is offline"),
    };

    // Ask the agent for a tunnel connection and wait for it to arrive.
    let conn = new_token();
    let (ready_tx, ready_rx) = oneshot::channel();
    st.pending.lock().insert(conn.clone(), (device.clone(), ready_tx));
    let asked = tx.send(json!({ "open": conn }).to_string()).await.is_ok();
    let stream = match tokio::time::timeout(Duration::from_secs(10), ready_rx).await {
        Ok(Ok(s)) if asked => s,
        _ => {
            st.pending.lock().remove(&conn);
            return fail(StatusCode::GATEWAY_TIMEOUT, "that computer didn't answer");
        }
    };

    let (mut sender, connection) = match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await {
        Ok(x) => x,
        Err(_) => return fail(StatusCode::BAD_GATEWAY, "tunnel failed"),
    };
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });

    // Rewrite the request as one the agent's local server accepts: its own host and
    // origin, its password, and none of the viewer's relay credentials.
    let query = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
    let Ok(uri) = format!("/{rest}{query}").parse::<Uri>() else {
        return fail(StatusCode::BAD_REQUEST, "bad path");
    };
    *req.uri_mut() = uri;
    let local = format!("127.0.0.1:{port}");
    let had_origin = req.headers().contains_key(header::ORIGIN);
    let h = req.headers_mut();
    for name in [header::COOKIE, header::AUTHORIZATION, header::REFERER, header::ORIGIN] {
        h.remove(name);
    }
    for name in ["x-forwarded-for", "x-forwarded-host", "x-forwarded-proto", "forwarded"] {
        h.remove(HeaderName::from_static(name));
    }
    h.insert(header::HOST, HeaderValue::from_str(&local).expect("host"));
    if had_origin {
        h.insert(header::ORIGIN, HeaderValue::from_str(&format!("http://{local}")).expect("origin"));
    }
    h.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {password}")).expect("authorization"),
    );

    let viewer_upgrade = req.extensions_mut().remove::<OnUpgrade>();
    let mut resp = match sender.send_request(req).await {
        Ok(r) => r,
        Err(_) => return fail(StatusCode::BAD_GATEWAY, "that computer dropped the request"),
    };

    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        let Some(viewer_upgrade) = viewer_upgrade else {
            return fail(StatusCode::BAD_GATEWAY, "unexpected protocol switch");
        };
        let agent_upgrade = hyper::upgrade::on(&mut resp);
        tokio::spawn(async move {
            if let (Ok(viewer), Ok(agent)) = (viewer_upgrade.await, agent_upgrade.await) {
                let (mut viewer, mut agent) = (TokioIo::new(viewer), TokioIo::new(agent));
                let _ = tokio::io::copy_bidirectional(&mut viewer, &mut agent).await;
            }
        });
        let (parts, _) = resp.into_parts();
        return Response::from_parts(parts, Body::empty());
    }
    resp.map(Body::new)
}
