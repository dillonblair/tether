//! tether-agent: runs on the computer to be controlled. It streams the screen with voxl,
//! listening only on the loopback address, and keeps an outgoing connection to the relay
//! through which signed-in viewers reach it.
//!
//!   tether-agent enroll --relay https://relay.example --code ABCDEFGHJK [--name "Studio Mac"]
//!   tether-agent run [voxl flags, e.g. --framerate=60 --audio-enabled=false]

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue, Message};

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    relay: String,
    device_id: String,
    token: String,
}

fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("TETHER_AGENT_CONFIG") {
        return p.into();
    }
    dirs::config_dir()
        .expect("a config directory")
        .join("tether")
        .join("agent.json")
}

fn flag(args: &[String], name: &str) -> Option<String> {
    let eq = format!("--{name}=");
    args.iter().enumerate().find_map(|(i, a)| {
        a.strip_prefix(&eq)
            .map(str::to_string)
            .or_else(|| (a == &format!("--{name}")).then(|| args.get(i + 1).cloned()).flatten())
    })
}

fn die(msg: &str) -> ! {
    eprintln!("tether-agent: {msg}");
    std::process::exit(1);
}

fn enroll(args: &[String]) {
    let relay = flag(args, "relay").unwrap_or_else(|| die("enroll needs --relay <url>"));
    let relay = relay.trim_end_matches('/').to_string();
    let code = flag(args, "code").unwrap_or_else(|| die("enroll needs --code <code>"));
    let name = flag(args, "name")
        .unwrap_or_else(|| gethostname::gethostname().to_string_lossy().into_owned());
    let resp = ureq::post(&format!("{relay}/api/agent/enroll"))
        .send_json(serde_json::json!({ "code": code, "name": name }));
    let body: serde_json::Value = match resp {
        Ok(r) => r.into_json().unwrap_or_else(|e| die(&format!("bad reply from the relay: {e}"))),
        Err(ureq::Error::Status(_, r)) => {
            let msg = r
                .into_json::<serde_json::Value>()
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or_else(|| "the relay refused".into());
            die(&msg)
        }
        Err(e) => die(&format!("couldn't reach the relay: {e}")),
    };
    let cfg = Config {
        relay,
        device_id: body["device_id"].as_str().unwrap_or_default().into(),
        token: body["token"].as_str().unwrap_or_default().into(),
    };
    let path = config_path();
    std::fs::create_dir_all(path.parent().expect("parent")).expect("config directory");
    std::fs::write(&path, serde_json::to_vec_pretty(&cfg).expect("json")).expect("write config");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    println!("Enrolled \"{name}\". Start it with: tether-agent run");
}

fn random_hex() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("random");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn run(extra: &[String]) {
    let path = config_path();
    let cfg: Config = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| die("not enrolled yet: run `tether-agent enroll --relay <url> --code <code>`"));
    // A free loopback port and a fresh password each run: only the relay, over the
    // agent's authenticated connection, learns them.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("a free port")
        .port();
    let password = random_hex();
    let (c, p) = (cfg.clone(), password.clone());
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(tunnel(c, port, p));
    });
    let mut args = vec![
        "--addr=127.0.0.1".to_string(),
        format!("--port={port}"),
        format!("--basic-auth-password={password}"),
        "--title=Tether".to_string(),
    ];
    args.extend(extra.iter().cloned());
    // voxl takes over this thread (on macOS it has to be the main thread).
    voxl::run(args);
}

fn ws_url(relay: &str, path: &str) -> String {
    let base = match relay.strip_prefix("https://") {
        Some(rest) => format!("wss://{rest}"),
        None => format!("ws://{}", relay.strip_prefix("http://").unwrap_or(relay)),
    };
    format!("{base}{path}")
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(cfg: &Config, path: &str) -> Result<Ws, String> {
    let mut req = ws_url(&cfg.relay, path)
        .into_client_request()
        .map_err(|e| e.to_string())?;
    req.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", cfg.token)).map_err(|e| e.to_string())?,
    );
    let (ws, _) = tokio_tungstenite::connect_async(req).await.map_err(|e| e.to_string())?;
    Ok(ws)
}

/// Keeps the control connection up, reconnecting with a growing delay.
async fn tunnel(cfg: Config, port: u16, password: String) {
    let mut delay = 1;
    loop {
        let started = std::time::Instant::now();
        match control(&cfg, port, &password).await {
            Ok(()) => log::info!("relay closed the connection"),
            Err(e) => log::warn!("relay connection: {e}"),
        }
        if started.elapsed() > Duration::from_secs(60) {
            delay = 1;
        }
        tokio::time::sleep(Duration::from_secs(delay)).await;
        delay = (delay * 2).min(30);
    }
}

async fn control(cfg: &Config, port: u16, password: &str) -> Result<(), String> {
    let mut ws = connect(cfg, "/agent/control").await?;
    let hello = serde_json::json!({ "port": port, "password": password }).to_string();
    ws.send(Message::Text(hello.into())).await.map_err(|e| e.to_string())?;
    log::info!("connected to {} as device {}", cfg.relay, cfg.device_id);
    loop {
        // The relay pings every 20 s; a minute of silence means the link is dead.
        let msg = tokio::time::timeout(Duration::from_secs(60), ws.next())
            .await
            .map_err(|_| "relay went quiet".to_string())?;
        match msg {
            Some(Ok(Message::Text(t))) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap_or_default();
                if let Some(conn) = v["open"].as_str() {
                    let (cfg, conn) = (cfg.clone(), conn.to_string());
                    tokio::spawn(async move {
                        if let Err(e) = data(&cfg, &conn, port).await {
                            log::debug!("tunnel connection: {e}");
                        }
                    });
                }
            }
            // Reading answers pings; flush so the pong goes out now.
            Some(Ok(Message::Ping(_))) => ws.flush().await.map_err(|e| e.to_string())?,
            Some(Ok(Message::Close(_))) | None => return Ok(()),
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(e.to_string()),
        }
    }
}

/// One tunnel connection: the relay's bytes to the local streaming server and back.
async fn data(cfg: &Config, conn: &str, port: u16) -> Result<(), String> {
    let ws = connect(cfg, &format!("/agent/data/{conn}")).await?;
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| e.to_string())?;
    let _ = tcp.set_nodelay(true);
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (mut rd, mut wr) = tcp.into_split();
    let to_relay = async {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ws_tx.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = ws_tx.close().await;
    };
    let from_relay = async {
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
    tokio::select! { _ = to_relay => {}, _ = from_relay => {} }
    Ok(())
}

fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("enroll") => enroll(&args[1..]),
        Some("run") => run(&args[1..]),
        None => run(&[]),
        _ => die("usage: tether-agent enroll --relay <url> --code <code> [--name <name>] | tether-agent run [voxl flags]"),
    }
}
