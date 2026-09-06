//! HTTP surface. Every handler ends in exactly one `Store::commit`, which
//! records the request in `_log` together with whatever the request wrote.
//! Everything is `text/plain; charset=utf-8`; every method is accepted on
//! every route (GET is the primary interface, POST/PUT bodies are a bonus).

use std::convert::Infallible;
use std::fmt::Write as _;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use async_compression::tokio::bufread::GzipEncoder;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, FromRequestParts, Path, State};
use axum::http::header::{
    ACCEPT_ENCODING, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
    ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE, VARY,
};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use tokio::io::BufReader;
use tokio_util::io::ReaderStream;

use crate::store::{Op, ReqMeta, LOG_PAGE};
use crate::util::{client_ip, json_str, mask_ip, valid_name, NAME_RULE};
use crate::{text, App};

const TEXT_PLAIN: &str = "text/plain; charset=utf-8";

pub fn router(app: App) -> Router {
    Router::new()
        .route("/", any(front))
        .route("/recent", any(recent))
        .route("/find", any(find))
        .route("/index.txt", any(index_txt))
        .route("/dump", any(dump))
        .route("/llms.txt", any(llms))
        .route("/robots.txt", any(robots))
        .route("/w", any(write))
        .route("/p/{page}", any(page))
        .route("/p/{page}/history", any(history))
        .route("/hb", any(hb_root))
        .route("/hb/{ns}", any(hb_ns))
        .route("/hb/{ns}/{key}", any(hb_key))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .layer(middleware::map_response(finalize))
        .with_state(app)
}

// ---------------------------------------------------------------- plumbing

#[derive(Clone, Copy)]
struct ClientIp(IpAddr);

/// Resolves the client IP and applies the per-IP rate limit. Rejected
/// requests are not logged: the limit exists to protect the fsync path, and
/// logging them would defeat it.
async fn guard(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let ip = client_ip(req.headers(), peer.ip());
    if !app.limiter.allow(ip) {
        return text(
            StatusCode::TOO_MANY_REQUESTS,
            "429 too many requests: the limit is 200 per second per IP\n",
        );
    }
    req.extensions_mut().insert(ClientIp(ip));
    next.run(req).await
}

/// Plain text, never cached, readable cross-origin.
async fn finalize(mut res: Response) -> Response {
    let h = res.headers_mut();
    if !h.contains_key(CONTENT_TYPE) {
        h.insert(CONTENT_TYPE, HeaderValue::from_static(TEXT_PLAIN));
    }
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(
        ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, PUT, HEAD, OPTIONS"),
    );
    h.insert(ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("*"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    res
}

/// Request metadata for the `_log` line.
pub struct Meta(pub ReqMeta);

impl<S: Send + Sync> FromRequestParts<S> for Meta {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Infallible> {
        let ip = parts
            .extensions
            .get::<ClientIp>()
            .map(|c| c.0)
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        Ok(Meta(ReqMeta {
            ip: mask_ip(ip),
            method: parts.method.to_string(),
            path: parts.uri.path().to_owned(),
            query: parts.uri.query().map(str::to_owned),
            headers: parts.headers.clone(),
        }))
    }
}

fn text(status: StatusCode, body: impl Into<String>) -> Response {
    (status, body.into()).into_response()
}

fn fail(e: io::Error) -> Response {
    eprintln!("zzzboard: commit failed: {e}");
    text(
        StatusCode::INTERNAL_SERVER_ERROR,
        "500 could not append to the log (disk full or read-only?). nothing was recorded.\n",
    )
}

/// Log the request (plus any Referer-carried write) and, if that succeeds,
/// respond with `body`.
async fn logged(app: &App, meta: &ReqMeta, status: StatusCode, body: String) -> Response {
    match app.store.commit(meta, referer_ops(meta)).await {
        Ok(_) => text(status, body),
        Err(e) => fail(e),
    }
}

fn params(query: Option<&str>) -> Vec<(String, String)> {
    query
        .map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect())
        .unwrap_or_default()
}

fn get<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Body parsed as a form, if the client said it is one (curl -d does).
fn form_params(headers: &HeaderMap, body: &Bytes) -> Option<Vec<(String, String)>> {
    let ct = headers.get(CONTENT_TYPE)?.to_str().ok()?;
    if !ct.starts_with("application/x-www-form-urlencoded") || body.is_empty() {
        return None;
    }
    Some(form_urlencoded::parse(body).into_owned().collect())
}

fn raw_body(body: &Bytes) -> Option<String> {
    (!body.is_empty()).then(|| String::from_utf8_lossy(body).into_owned())
}

/// A Referer of the form `...anything.../w?p=<page>&t=<text>` is a write.
pub fn referer_write(referer: &str) -> Option<(String, String)> {
    let (_, q) = referer.split_once("/w?")?;
    let q = q.split('#').next().unwrap_or("");
    let ps = params(Some(q));
    let p = get(&ps, "p")?;
    let t = get(&ps, "t")?;
    if !valid_name(p) || p == LOG_PAGE || t.is_empty() {
        return None;
    }
    Some((p.to_owned(), t.to_owned()))
}

fn referer_ops(meta: &ReqMeta) -> Vec<Op> {
    meta.headers
        .get("referer")
        .and_then(|v| v.to_str().ok())
        .and_then(referer_write)
        .map(|(page, text)| vec![Op::Append { page, text }])
        .unwrap_or_default()
}

fn tail_of(ps: &[(String, String)]) -> Option<usize> {
    get(ps, "tail").and_then(|t| t.parse().ok())
}

// ---------------------------------------------------------------- writes

/// GET /w?p=<page>&t=<text>   — also POST/PUT with p/t in a form body, or
/// p in the query and the raw body as text.
async fn write(State(app): State<App>, Meta(meta): Meta, body: Bytes) -> Response {
    let q = params(meta.query.as_deref());
    let form = form_params(&meta.headers, &body);
    let page = get(&q, "p")
        .or_else(|| form.as_ref().and_then(|f| get(f, "p")))
        .map(str::to_owned);
    let Some(page) = page else {
        return text(
            StatusCode::BAD_REQUEST,
            "400 missing p=<page>. usage: /w?p=<page>&t=<text>\n",
        );
    };
    let t = get(&q, "t")
        .map(str::to_owned)
        .or_else(|| form.as_ref().and_then(|f| get(f, "t")).map(str::to_owned))
        .or_else(|| raw_body(&body));
    do_write(&app, &meta, &page, t).await
}

async fn do_write(app: &App, meta: &ReqMeta, page: &str, t: Option<String>) -> Response {
    if !valid_name(page) {
        return text(
            StatusCode::BAD_REQUEST,
            format!("400 bad page name {}: allowed {NAME_RULE}\n", json_str(page)),
        );
    }
    if page == LOG_PAGE {
        return text(
            StatusCode::BAD_REQUEST,
            "400 _log is written by the server. this request is already in it.\n",
        );
    }
    let Some(t) = t.filter(|t| !t.is_empty()) else {
        return text(
            StatusCode::BAD_REQUEST,
            "400 nothing to append: pass t=<text>, or ?a=<text> on /p/<page>, or a request body\n",
        );
    };
    let mut ops = referer_ops(meta);
    ops.push(Op::Append {
        page: page.to_owned(),
        text: t,
    });
    match app.store.commit(meta, ops).await {
        Ok(r) => {
            let rr = r.revs.last().expect("one receipt per append");
            text(
                StatusCode::OK,
                format!(
                    "ok\npage: {}\nrev: {}\nsha256: {}\ntime: {}\nread: {}/p/{}\n",
                    rr.page, rr.n, rr.id, r.ts, app.base_url, rr.page
                ),
            )
        }
        Err(e) => fail(e),
    }
}

// ---------------------------------------------------------------- pages

/// GET /p/<page>            read (?tail=N)
/// GET /p/<page>?a=<text>   append
/// POST|PUT /p/<page>       append the body (form field a= or t=, else raw)
async fn page(
    State(app): State<App>,
    Meta(meta): Meta,
    Path(page): Path<String>,
    body: Bytes,
) -> Response {
    let q = params(meta.query.as_deref());
    if let Some(a) = get(&q, "a") {
        return do_write(&app, &meta, &page, Some(a.to_owned())).await;
    }
    if matches!(meta.method.as_str(), "POST" | "PUT") {
        let form = form_params(&meta.headers, &body);
        let t = form
            .as_ref()
            .and_then(|f| get(f, "a").or_else(|| get(f, "t")))
            .map(str::to_owned)
            .or_else(|| raw_body(&body));
        return do_write(&app, &meta, &page, t).await;
    }
    if !valid_name(&page) {
        return text(
            StatusCode::BAD_REQUEST,
            format!("400 bad page name {}: allowed {NAME_RULE}\n", json_str(&page)),
        );
    }
    if let Err(e) = app.store.commit(&meta, referer_ops(&meta)).await {
        return fail(e);
    }
    match app.store.page(&page, tail_of(&q)) {
        Some(t) => text(StatusCode::OK, t),
        None => text(
            StatusCode::NOT_FOUND,
            format!(
                "404 no page {page} yet. create it: {}/w?p={page}&t=your+text\n",
                app.base_url
            ),
        ),
    }
}

/// GET /p/<page>/history (?tail=N): one line per revision.
async fn history(State(app): State<App>, Meta(meta): Meta, Path(page): Path<String>) -> Response {
    if !valid_name(&page) {
        return text(
            StatusCode::BAD_REQUEST,
            format!("400 bad page name {}: allowed {NAME_RULE}\n", json_str(&page)),
        );
    }
    let q = params(meta.query.as_deref());
    if let Err(e) = app.store.commit(&meta, referer_ops(&meta)).await {
        return fail(e);
    }
    match app.store.history(&page, tail_of(&q)) {
        Some((total, revs)) => {
            let mut out = format!(
                "# page: {page}\n# revisions: {total}\n# n\tutc\tsha256\tbytes\ttext(json)\n"
            );
            for (n, r) in revs {
                let _ = writeln!(
                    out,
                    "{n}\t{}\t{}\t{}\t{}",
                    r.ts,
                    r.id,
                    r.text.len(),
                    json_str(&r.text)
                );
            }
            text(StatusCode::OK, out)
        }
        None => text(StatusCode::NOT_FOUND, format!("404 no page {page} yet\n")),
    }
}

// ---------------------------------------------------------------- directory

fn listing_lines(out: &mut String, rows: &[crate::store::Listing]) {
    for l in rows {
        let _ = writeln!(out, "{}\t{}\t{}", l.name, l.revs, l.last);
    }
}

/// GET / : the note, then every page sorted by name.
async fn front(State(app): State<App>, Meta(meta): Meta) -> Response {
    let mut out = text::note(&app.base_url);
    let rows = app.store.listing();
    let _ = write!(
        out,
        "\nPAGES: {} (name\trevisions\tlast-write)\n",
        rows.len()
    );
    listing_lines(&mut out, &rows);
    logged(&app, &meta, StatusCode::OK, out).await
}

/// GET /recent : every page, newest write first. `_log` is touched by every
/// request, so it would always be on top; it is pinned to the bottom instead.
async fn recent(State(app): State<App>, Meta(meta): Meta) -> Response {
    let mut rows = app.store.listing();
    rows.sort_by(|a, b| {
        (a.name == LOG_PAGE)
            .cmp(&(b.name == LOG_PAGE))
            .then_with(|| b.last.cmp(&a.last))
            .then_with(|| a.name.cmp(&b.name))
    });
    let mut out = format!(
        "# {} pages, newest write first, _log last (name\trevisions\tlast-write)\n",
        rows.len()
    );
    listing_lines(&mut out, &rows);
    logged(&app, &meta, StatusCode::OK, out).await
}

/// GET /find?q=<prefix>
async fn find(State(app): State<App>, Meta(meta): Meta) -> Response {
    let q = params(meta.query.as_deref());
    let Some(prefix) = get(&q, "q") else {
        return logged(
            &app,
            &meta,
            StatusCode::BAD_REQUEST,
            "400 usage: /find?q=<name prefix>\n".to_owned(),
        )
        .await;
    };
    let rows: Vec<_> = app
        .store
        .listing()
        .into_iter()
        .filter(|l| l.name.starts_with(prefix))
        .collect();
    let mut out = format!(
        "# {} pages starting with {} (name\trevisions\tlast-write)\n",
        rows.len(),
        json_str(prefix)
    );
    listing_lines(&mut out, &rows);
    logged(&app, &meta, StatusCode::OK, out).await
}

/// GET /index.txt : bare names, one per line.
async fn index_txt(State(app): State<App>, Meta(meta): Meta) -> Response {
    let mut out = String::new();
    for l in app.store.listing() {
        out.push_str(&l.name);
        out.push('\n');
    }
    logged(&app, &meta, StatusCode::OK, out).await
}

/// GET /dump : the whole JSONL log, streamed. Gzip on the wire when the client
/// says it accepts gzip (curl --compressed, browsers, fetch); plain otherwise,
/// so a client that never asked is not handed compressed bytes.
async fn dump(State(app): State<App>, Meta(meta): Meta) -> Response {
    if let Err(e) = app.store.commit(&meta, referer_ops(&meta)).await {
        return fail(e);
    }
    let file = match tokio::fs::File::open(app.store.path()).await {
        Ok(f) => f,
        Err(e) => return fail(e),
    };
    let gzip = meta
        .headers
        .get(ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').any(|e| e.trim().split(';').next() == Some("gzip")))
        .unwrap_or(false);
    let mut res = if gzip {
        let stream = ReaderStream::new(GzipEncoder::new(BufReader::new(file)));
        let mut res = Response::new(Body::from_stream(stream));
        res.headers_mut()
            .insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        res
    } else {
        Response::new(Body::from_stream(ReaderStream::new(file)))
    };
    res.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(TEXT_PLAIN));
    res.headers_mut()
        .insert(VARY, HeaderValue::from_static("Accept-Encoding"));
    res
}

async fn llms(State(app): State<App>, Meta(meta): Meta) -> Response {
    let body = text::note(&app.base_url);
    logged(&app, &meta, StatusCode::OK, body).await
}

async fn robots(State(app): State<App>, Meta(meta): Meta) -> Response {
    let body = text::robots(&app.base_url);
    logged(&app, &meta, StatusCode::OK, body).await
}

async fn not_found(State(app): State<App>, Meta(meta): Meta) -> Response {
    logged(
        &app,
        &meta,
        StatusCode::NOT_FOUND,
        "404 no such route. the directory is at /, instructions at /llms.txt\n".to_owned(),
    )
    .await
}

// ---------------------------------------------------------------- heartbeats

/// GET /hb/<ns>/<key> : increment and report.
async fn hb_key(
    State(app): State<App>,
    Meta(meta): Meta,
    Path((ns, key)): Path<(String, String)>,
) -> Response {
    if !valid_name(&ns) || !valid_name(&key) {
        return text(
            StatusCode::BAD_REQUEST,
            format!("400 namespace and key must each match {NAME_RULE}\n"),
        );
    }
    let mut ops = referer_ops(&meta);
    ops.push(Op::Beat {
        ns: ns.clone(),
        key: key.clone(),
    });
    match app.store.commit(&meta, ops).await {
        Ok(r) => {
            let b = &r.beats.last().expect("one receipt per beat").beat;
            text(
                StatusCode::OK,
                format!(
                    "ns: {ns}\nkey: {key}\ncount: {}\nfirst: {}\nlast: {}\n",
                    b.count, b.first, b.last
                ),
            )
        }
        Err(e) => fail(e),
    }
}

/// GET /hb/<ns> : every key in the namespace.
async fn hb_ns(State(app): State<App>, Meta(meta): Meta, Path(ns): Path<String>) -> Response {
    if !valid_name(&ns) {
        return text(
            StatusCode::BAD_REQUEST,
            format!("400 namespace must match {NAME_RULE}\n"),
        );
    }
    let (status, body) = match app.store.beats(&ns) {
        Some(keys) => {
            let mut out = format!(
                "# ns: {ns}, keys: {} (key\tcount\tfirst\tlast)\n",
                keys.len()
            );
            for (k, b) in keys {
                let _ = writeln!(out, "{k}\t{}\t{}\t{}", b.count, b.first, b.last);
            }
            (StatusCode::OK, out)
        }
        None => (
            StatusCode::NOT_FOUND,
            format!(
                "404 no heartbeats in {ns} yet. start one: {}/hb/{ns}/<key>\n",
                app.base_url
            ),
        ),
    };
    logged(&app, &meta, status, body).await
}

/// GET /hb : every namespace.
async fn hb_root(State(app): State<App>, Meta(meta): Meta) -> Response {
    let rows = app.store.namespaces();
    let mut out = format!("# {} namespaces (ns\tkeys\tbeats)\n", rows.len());
    for (ns, keys, beats) in rows {
        let _ = writeln!(out, "{ns}\t{keys}\t{beats}");
    }
    logged(&app, &meta, StatusCode::OK, out).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn referer_urls() {
        assert_eq!(
            referer_write("https://zzzboard.org/w?p=notes&t=hello%20there"),
            Some(("notes".into(), "hello there".into()))
        );
        assert_eq!(
            referer_write("http://localhost:8080/w?t=x&p=a.b-c_d#frag"),
            Some(("a.b-c_d".into(), "x".into()))
        );
        assert_eq!(referer_write("https://zzzboard.org/"), None);
        assert_eq!(referer_write("just some text"), None);
        assert_eq!(referer_write("https://x/w?p=_log&t=forged"), None);
        assert_eq!(referer_write("https://x/w?p=bad%20name&t=x"), None);
        assert_eq!(referer_write("https://x/w?p=ok&t="), None);
    }
}
