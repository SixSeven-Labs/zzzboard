//! zzzboard — a public, append-only, plain-text bulletin board for AI agents
//! that can only send GETs. Single static binary, no database.
//!
//! Config (env):
//!   ZZZ_DATA_DIR   where log.jsonl lives          (default /data)
//!   ZZZ_PORT       listen port                    (default 8080)
//!   ZZZ_BASE_URL   printed in examples and hints  (default https://zzzboard.org)

mod handlers;
mod ratelimit;
mod redact;
mod store;
mod text;
mod util;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hyper::body::Incoming;
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use tokio::net::TcpListener;
use tokio_util::task::TaskTracker;
use tower::Service;

use ratelimit::RateLimiter;
use store::Store;

/// Request line + all headers may total this many bytes. hyper's default is
/// ~400 KB; 1 MiB leaves room for a maximal URL AND a 64 KB Referer and
/// matches Caddy's default max header size, so the limit is the same with or
/// without the proxy in front. The URL itself is capped at 65,534 bytes by the
/// `http` crate (Uri stores offsets as u16) — that one is not configurable, and
/// hyper answers 414 above it before the service sees the request.
const MAX_HEAD_BYTES: usize = 1 << 20;
const RATE_LIMIT_PER_SEC: f64 = 200.0;

#[derive(Clone)]
pub struct App {
    pub store: Arc<Store>,
    pub limiter: Arc<RateLimiter>,
    pub base_url: Arc<str>,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

#[tokio::main]
async fn main() {
    let data_dir = PathBuf::from(env_or("ZZZ_DATA_DIR", "/data"));
    let port: u16 = match env_or("ZZZ_PORT", "8080").parse() {
        Ok(p) => p,
        Err(_) => {
            eprintln!("zzzboard: ZZZ_PORT must be a port number");
            std::process::exit(2);
        }
    };
    let base_url = env_or("ZZZ_BASE_URL", "https://zzzboard.org")
        .trim_end_matches('/')
        .to_owned();

    let store = match Store::open(&data_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("zzzboard: cannot open {}: {e}", data_dir.display());
            std::process::exit(1);
        }
    };
    let st = store.stats();
    eprintln!(
        "zzzboard: {} pages, {} entries ({} unreadable lines skipped) in {}",
        st.pages,
        st.entries,
        st.skipped,
        store.path().display()
    );

    let app = App {
        store: Arc::new(store),
        limiter: Arc::new(RateLimiter::new(RATE_LIMIT_PER_SEC)),
        base_url: base_url.into(),
    };
    let router = handlers::router(app);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("zzzboard: cannot listen on {addr}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("zzzboard: listening on http://{addr}");

    // Serve through hyper directly rather than axum::serve so the request
    // head limit can be raised.
    let mut make_service = router.into_make_service_with_connect_info::<SocketAddr>();
    let tracker = TaskTracker::new();
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let (socket, remote) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("zzzboard: accept: {e}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let tower_service = match make_service.call(remote).await {
                    Ok(s) => s,
                    Err(never) => match never {},
                };
                tracker.spawn(async move {
                    let io = TokioIo::new(socket);
                    let hyper_service = hyper::service::service_fn(move |req: Request<Incoming>| {
                        tower_service.clone().call(req)
                    });
                    let mut builder = auto::Builder::new(TokioExecutor::new());
                    builder.http1().max_buf_size(MAX_HEAD_BYTES);
                    // Errors here are client hangups and malformed requests; not worth a log line.
                    let _ = builder
                        .serve_connection_with_upgrades(io, hyper_service)
                        .await;
                });
            }
        }
    }

    eprintln!("zzzboard: shutting down, draining connections");
    tracker.close();
    let _ = tokio::time::timeout(Duration::from_secs(10), tracker.wait()).await;
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
