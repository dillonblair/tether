//! tether-relay: accounts, the list of computers, and the tunnel that lets a viewer reach
//! a host agent from anywhere. Host agents dial out to the relay, so no router setup or
//! open ports are needed on the host's network.

mod db;
mod proxy;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        FromRequestParts, Path, State,
    },
    http::{header, request::Parts, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{any, delete, get, post},
    Json, Router,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
    sync::{mpsc, oneshot},
};

const COOKIE: &str = "tether_session";
const SESSION_DAYS: i64 = 30;

/// A host agent with a live control connection.
pub struct Agent {
    /// Control messages for the agent (JSON text).
    pub tx: mpsc::Sender<String>,
    /// The local port the agent's streaming server listens on.
    pub port: u16,
    /// The password of that server, made up by the agent for each run.
    pub password: String,
    epoch: u64,
}

pub struct Inner {
    pub db: db::Db,
    pub agents: Mutex<HashMap<String, Agent>>,
    /// Tunnel connections the relay asked for and the agent hasn't opened yet.
    pub pending: Mutex<HashMap<String, (String, oneshot::Sender<DuplexStream>)>>,
    /// Failed logins per email: count and when the window started.
    fails: Mutex<HashMap<String, (u32, Instant)>>,
    secure_cookies: bool,
    epoch: Mutex<u64>,
}

pub type AppState = Arc<Inner>;

pub fn new_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

pub fn hash_token(t: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(t.as_bytes()))
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v)
}

/// The signed-in user, from the session cookie (browsers) or a bearer token (apps).
pub struct User(pub i64);

impl FromRequestParts<AppState> for User {
    type Rejection = (StatusCode, &'static str);
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        cookie(&parts.headers)
            .or_else(|| bearer(&parts.headers))
            .and_then(|t| st.db.session_user(&hash_token(t)))
            .map(User)
            .ok_or((StatusCode::UNAUTHORIZED, "sign in first"))
    }
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

#[derive(Deserialize)]
struct Credentials {
    email: String,
    password: String,
}

fn session_response(st: &AppState, user: i64) -> Response {
    let token = new_token();
    st.db.add_session(&hash_token(&token), user, SESSION_DAYS);
    let cookie = format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}",
        SESSION_DAYS * 86400,
        if st.secure_cookies { "; Secure" } else { "" }
    );
    // The token is also returned for the native apps, which send it as a bearer token.
    ([(header::SET_COOKIE, cookie)], Json(json!({ "token": token }))).into_response()
}

async fn register(State(st): State<AppState>, Json(c): Json<Credentials>) -> Response {
    let email = c.email.trim().to_lowercase();
    if !email.contains('@') || email.len() > 254 {
        return err(StatusCode::BAD_REQUEST, "enter an email address");
    }
    if c.password.len() < 10 || c.password.len() > 256 {
        return err(StatusCode::BAD_REQUEST, "use a password of at least 10 characters");
    }
    let hash = tokio::task::spawn_blocking(move || db::hash_password(&c.password))
        .await
        .expect("hash");
    match st.db.add_user(&email, &hash) {
        Some(id) => session_response(&st, id),
        None => err(StatusCode::CONFLICT, "that email already has an account"),
    }
}

async fn login(State(st): State<AppState>, Json(c): Json<Credentials>) -> Response {
    let email = c.email.trim().to_lowercase();
    {
        let mut fails = st.fails.lock();
        fails.retain(|_, (_, t)| t.elapsed() < Duration::from_secs(300));
        if fails.get(&email).is_some_and(|(n, _)| *n >= 5) {
            return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts; try again in 5 minutes");
        }
    }
    let found = st.db.user_by_email(&email);
    let ok = tokio::task::spawn_blocking(move || match found {
        Some((id, hash)) => db::verify_password(&c.password, &hash).then_some(id),
        // Hash anyway, so a missing account takes as long as a wrong password.
        None => {
            db::hash_password(&c.password);
            None
        }
    })
    .await
    .expect("verify");
    match ok {
        Some(id) => session_response(&st, id),
        None => {
            st.fails.lock().entry(email).or_insert((0, Instant::now())).0 += 1;
            err(StatusCode::UNAUTHORIZED, "wrong email or password")
        }
    }
}

async fn logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(t) = cookie(&headers).or_else(|| bearer(&headers)) {
        st.db.drop_session(&hash_token(t));
    }
    let gone = format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    ([(header::SET_COOKIE, gone)], Json(json!({}))).into_response()
}

async fn devices(State(st): State<AppState>, User(user): User) -> Response {
    let agents = st.agents.lock();
    let list: Vec<_> = st
        .db
        .devices(user)
        .into_iter()
        .map(|d| {
            json!({ "id": d.id, "name": d.name, "online": agents.contains_key(&d.id),
                "last_seen": d.last_seen })
        })
        .collect();
    Json(json!({ "devices": list })).into_response()
}

async fn remove_device(State(st): State<AppState>, User(user): User, Path(id): Path<String>) -> Response {
    if !st.db.remove_device(user, &id) {
        return err(StatusCode::NOT_FOUND, "no such computer");
    }
    // Dropping the agent's sender ends its control connection.
    st.agents.lock().remove(&id);
    Json(json!({})).into_response()
}

/// A one-time code that lets a host agent join this account.
async fn pairing(State(st): State<AppState>, User(user): User) -> Response {
    use rand::Rng;
    // No 0/O or 1/I, so the code can be read off one screen and typed on another.
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let code: String = (0..10)
        .map(|_| ALPHABET[rand::rngs::OsRng.gen_range(0..ALPHABET.len())] as char)
        .collect();
    st.db.add_pairing(&hash_token(&code), user, 600);
    Json(json!({ "code": code, "expires_in": 600 })).into_response()
}

#[derive(Deserialize)]
struct Enroll {
    code: String,
    name: String,
}

async fn enroll(State(st): State<AppState>, Json(e): Json<Enroll>) -> Response {
    let code = e.code.trim().to_uppercase();
    let Some(user) = st.db.take_pairing(&hash_token(&code)) else {
        return err(StatusCode::FORBIDDEN, "that code is wrong or has expired");
    };
    let id = new_token()[..16].to_string();
    let token = new_token();
    let name: String = e.name.trim().chars().take(64).collect();
    let name = if name.is_empty() { "Computer".into() } else { name };
    st.db.add_device(&id, user, &name, &hash_token(&token));
    Json(json!({ "device_id": id, "token": token })).into_response()
}

fn agent_device(st: &AppState, headers: &HeaderMap) -> Option<String> {
    st.db.device_by_token(&hash_token(bearer(headers)?))
}

#[derive(Deserialize)]
struct Hello {
    port: u16,
    password: String,
}

async fn agent_control(State(st): State<AppState>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let Some(dev) = agent_device(&st, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "unknown device token");
    };
    ws.on_upgrade(move |sock| control_loop(st, dev, sock))
}

async fn control_loop(st: AppState, dev: String, mut sock: WebSocket) {
    let hello = match tokio::time::timeout(Duration::from_secs(10), sock.recv()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<Hello>(&t).ok(),
        _ => None,
    };
    let Some(hello) = hello else { return };
    let (tx, mut rx) = mpsc::channel::<String>(64);
    let epoch = {
        let mut e = st.epoch.lock();
        *e += 1;
        *e
    };
    st.agents.lock().insert(
        dev.clone(),
        Agent { tx, port: hello.port, password: hello.password, epoch },
    );
    st.db.touch_device(&dev);
    log::info!("agent {dev} online");
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    loop {
        tokio::select! {
            m = rx.recv() => {
                let Some(m) = m else { break };
                if sock.send(Message::Text(m.into())).await.is_err() { break }
            }
            m = sock.recv() => match m {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                _ => {}
            },
            _ = ping.tick() => {
                if sock.send(Message::Ping(Bytes::new())).await.is_err() { break }
            }
        }
    }
    // A newer connection from the same agent may have replaced this one already.
    let mut agents = st.agents.lock();
    if agents.get(&dev).is_some_and(|a| a.epoch == epoch) {
        agents.remove(&dev);
    }
    drop(agents);
    st.db.touch_device(&dev);
    log::info!("agent {dev} offline");
}

async fn agent_data(
    State(st): State<AppState>,
    Path(conn): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(dev) = agent_device(&st, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "unknown device token");
    };
    let Some((want, tx)) = st.pending.lock().remove(&conn) else {
        return err(StatusCode::NOT_FOUND, "no such connection");
    };
    if want != dev {
        return err(StatusCode::FORBIDDEN, "not this device's connection");
    }
    ws.max_message_size(1 << 20).on_upgrade(move |sock| async move {
        let (ours, theirs) = tokio::io::duplex(256 * 1024);
        if tx.send(theirs).is_ok() {
            pump(sock, ours).await;
        }
    })
}

/// Carries a tunnel's bytes between the agent's WebSocket and the proxy's stream.
async fn pump(sock: WebSocket, io: DuplexStream) {
    let (mut ws_tx, mut ws_rx) = sock.split();
    let (mut rd, mut wr) = tokio::io::split(io);
    let to_agent = async {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ws_tx.send(Message::Binary(Bytes::copy_from_slice(&buf[..n]))).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = ws_tx.close().await;
    };
    let from_agent = async {
        while let Some(Ok(m)) = ws_rx.next().await {
            match m {
                Message::Binary(b) => {
                    if wr.write_all(&b).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        let _ = wr.shutdown().await;
    };
    tokio::select! { _ = to_agent => {}, _ = from_agent => {} }
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let addr = std::env::var("TETHER_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let db_path = std::env::var("TETHER_DB").unwrap_or_else(|_| "tether.db".into());
    // Set TETHER_PUBLIC_URL=https://… in production so cookies are only sent over TLS.
    let secure_cookies = std::env::var("TETHER_PUBLIC_URL").is_ok_and(|u| u.starts_with("https://"));
    let st: AppState = Arc::new(Inner {
        db: db::Db::open(&db_path),
        agents: Mutex::default(),
        pending: Mutex::default(),
        fails: Mutex::default(),
        secure_cookies,
        epoch: Mutex::new(0),
    });
    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/register", post(register))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/devices", get(devices))
        .route("/api/devices/{id}", delete(remove_device))
        .route("/api/pairing", post(pairing))
        .route("/api/agent/enroll", post(enroll))
        .route("/agent/control", get(agent_control))
        .route("/agent/data/{conn}", get(agent_data))
        .route("/d/{device}", any(proxy::add_slash))
        .route("/d/{device}/", any(proxy::root))
        .route("/d/{device}/{*rest}", any(proxy::path))
        .with_state(st);
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    log::info!("tether-relay listening on http://{addr}");
    axum::serve(listener, app).await.expect("serve");
}
