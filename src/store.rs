//! The only persistent state: one append-only JSONL file plus an in-memory
//! index rebuilt from it at startup.
//!
//! Every line is one entry:
//!   {"t":"a","ts":"<utc>","p":"<page>","x":"<text>","r":"<sha256>"}   an append
//!   {"t":"b","ts":"<utc>","ns":"<ns>","k":"<key>"}                    a heartbeat
//!
//! Pages are revision logs: the current text of a page is every append to it,
//! newline-joined. Revision ids chain per page:
//! r = sha256(previous_r + "\n" + ts + "\n" + text), with previous_r = "" for
//! the first revision — so a page's history is tamper-evident from `/dump`.
//!
//! `commit` is the single write chokepoint. It takes the request metadata and
//! a list of ops, runs every piece of text (the user's appends and the
//! request's own `_log` line) through `redact::filter`, serializes them,
//! writes them with ONE write + ONE fdatasync, then updates the index. There
//! is no delete, no overwrite, no truncate anywhere in this module.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};

use axum::http::HeaderMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::redact;
use crate::util::{json_str, now_ts};

/// The server-written request log. Users cannot append to it directly.
pub const LOG_PAGE: &str = "_log";
pub const LOG_FILE: &str = "log.jsonl";

/// On-disk entry (owned; used when reading the log back).
#[derive(Deserialize, Debug)]
#[serde(tag = "t")]
pub enum Entry {
    #[serde(rename = "a")]
    Append {
        ts: String,
        p: String,
        x: String,
        r: String,
    },
    #[serde(rename = "b")]
    Beat { ts: String, ns: String, k: String },
}

/// Same shape, borrowed; used when writing.
#[derive(Serialize)]
#[serde(tag = "t")]
enum EntryRef<'a> {
    #[serde(rename = "a")]
    Append {
        ts: &'a str,
        p: &'a str,
        x: &'a str,
        r: &'a str,
    },
    #[serde(rename = "b")]
    Beat {
        ts: &'a str,
        ns: &'a str,
        k: &'a str,
    },
}

#[derive(Clone, Debug)]
pub struct Rev {
    pub ts: String,
    pub id: String,
    pub text: String,
}

#[derive(Default, Debug)]
pub struct PageIdx {
    pub revs: Vec<Rev>,
}

#[derive(Clone, Debug)]
pub struct Beat {
    pub count: u64,
    pub first: String,
    pub last: String,
}

#[derive(Default)]
pub struct Index {
    pub pages: BTreeMap<String, PageIdx>,
    pub beats: BTreeMap<String, BTreeMap<String, Beat>>,
    pub entries: u64,
    pub skipped: u64,
}

impl Index {
    fn apply(&mut self, e: Entry) {
        match e {
            Entry::Append { ts, p, x, r } => {
                self.pages.entry(p).or_default().revs.push(Rev {
                    ts,
                    id: r,
                    text: x,
                });
            }
            Entry::Beat { ts, ns, k } => {
                beat(self, ns, k, &ts);
            }
        }
        self.entries += 1;
    }
}

fn beat(idx: &mut Index, ns: String, key: String, ts: &str) -> Beat {
    let b = idx
        .beats
        .entry(ns)
        .or_default()
        .entry(key)
        .or_insert_with(|| Beat {
            count: 0,
            first: ts.to_owned(),
            last: String::new(),
        });
    b.count += 1;
    b.last = ts.to_owned();
    b.clone()
}

/// What we know about a request. Built by the `Meta` extractor in handlers.
#[derive(Clone, Debug)]
pub struct ReqMeta {
    /// Already masked (10.20.30.xxx).
    pub ip: String,
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub headers: HeaderMap,
}

impl ReqMeta {
    /// One line for the `_log` page. Authorization and Cookie are never read;
    /// only Referer, User-Agent and X-* headers are included. X-Forwarded-*
    /// and X-Real-IP are dropped because the (masked) `ip` field already
    /// represents them and the raw values would leak the full address.
    pub fn log_line(&self, ts: &str) -> String {
        let mut line = format!("{ts} {} {} {}", self.ip, self.method, self.path);
        if let Some(q) = &self.query {
            line.push_str(" q=");
            line.push_str(&json_str(q));
        }
        if let Some(v) = self.headers.get("referer") {
            push_kv(&mut line, "ref", v.as_bytes());
        }
        if let Some(v) = self.headers.get("user-agent") {
            push_kv(&mut line, "ua", v.as_bytes());
        }
        let mut xs: Vec<(&str, &[u8])> = self
            .headers
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_bytes()))
            .filter(|(n, _)| {
                n.starts_with("x-") && !n.starts_with("x-forwarded-") && *n != "x-real-ip"
            })
            .collect();
        xs.sort();
        for (n, v) in xs {
            push_kv(&mut line, n, v);
        }
        line
    }
}

fn push_kv(line: &mut String, k: &str, v: &[u8]) {
    line.push(' ');
    line.push_str(k);
    line.push('=');
    line.push_str(&json_str(&String::from_utf8_lossy(v)));
}

pub enum Op {
    Append { page: String, text: String },
    Beat { ns: String, key: String },
}

#[derive(Default, Debug)]
pub struct Receipt {
    pub ts: String,
    /// One per user `Op::Append`, in order. The `_log` line is not reported.
    pub revs: Vec<RevReceipt>,
    /// One per `Op::Beat`, in order.
    pub beats: Vec<BeatReceipt>,
}

#[derive(Debug)]
pub struct RevReceipt {
    pub page: String,
    pub n: usize,
    pub id: String,
}

#[derive(Debug)]
pub struct BeatReceipt {
    pub ns: String,
    pub key: String,
    pub beat: Beat,
}

pub struct Listing {
    pub name: String,
    pub revs: usize,
    pub last: String,
}

pub struct Stats {
    pub pages: usize,
    pub entries: u64,
    pub skipped: u64,
}

pub fn rev_id(prev: &str, ts: &str, text: &str) -> String {
    let mut h = Sha256::new();
    h.update(prev.as_bytes());
    h.update(b"\n");
    h.update(ts.as_bytes());
    h.update(b"\n");
    h.update(text.as_bytes());
    hex::encode(h.finalize())
}

pub struct Store {
    path: PathBuf,
    /// Serializes commits. Held across the write + fdatasync.
    writer: Mutex<tokio::fs::File>,
    /// Held only for the few microseconds it takes to read or insert.
    index: RwLock<Index>,
}

impl Store {
    /// Open (or create) `<dir>/log.jsonl` and rebuild the index from it.
    /// A trailing line without `\n` (crash mid-write) is left in place — it is
    /// skipped by the parser — and a `\n` is appended so the next entry starts
    /// clean. Bytes are never removed.
    pub fn open(dir: &Path) -> io::Result<Store> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(LOG_FILE);
        let mut index = Index::default();
        let mut unterminated = false;
        if path.exists() {
            let mut reader = BufReader::with_capacity(1 << 20, std::fs::File::open(&path)?);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                if reader.read_until(b'\n', &mut buf)? == 0 {
                    break;
                }
                let complete = buf.ends_with(b"\n");
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim_end_matches(['\n', '\r']);
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Entry>(line) {
                    Ok(e) if complete => index.apply(e),
                    Ok(_) | Err(_) => index.skipped += 1,
                }
                if !complete {
                    unterminated = true;
                }
            }
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        if unterminated {
            file.write_all(b"\n")?;
            file.sync_data()?;
            eprintln!("zzzboard: log ended mid-line; terminated it, the partial line is skipped");
        }
        Ok(Store {
            path,
            writer: Mutex::new(tokio::fs::File::from_std(file)),
            index: RwLock::new(index),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// THE write path. Appends `ops` plus this request's `_log` line in one
    /// fdatasync'd write, then updates the index.
    pub async fn commit(&self, meta: &ReqMeta, ops: Vec<Op>) -> io::Result<Receipt> {
        let mut file = self.writer.lock().await;
        let ts = now_ts();

        let mut appends: Vec<(String, String)> = Vec::with_capacity(ops.len() + 1);
        let mut beats: Vec<(String, String)> = Vec::new();
        for op in ops {
            match op {
                Op::Append { page, text } => {
                    let text = redact::filter(&page, text, &meta.headers);
                    appends.push((page, text));
                }
                Op::Beat { ns, key } => beats.push((ns, key)),
            }
        }
        let user_appends = appends.len();
        let line = redact::filter(LOG_PAGE, meta.log_line(&ts), &meta.headers);
        appends.push((LOG_PAGE.to_owned(), line));

        // Serialize under a read lock so revision ids chain off the true tail.
        let mut buf = Vec::new();
        let mut new_revs: Vec<(String, Rev)> = Vec::with_capacity(appends.len());
        {
            let idx = self.index.read();
            let mut chain: HashMap<String, String> = HashMap::new();
            for (page, text) in appends {
                let prev = match chain.get(&page) {
                    Some(p) => p.clone(),
                    None => idx
                        .pages
                        .get(&page)
                        .and_then(|p| p.revs.last())
                        .map(|r| r.id.clone())
                        .unwrap_or_default(),
                };
                let id = rev_id(&prev, &ts, &text);
                chain.insert(page.clone(), id.clone());
                let e = EntryRef::Append {
                    ts: &ts,
                    p: &page,
                    x: &text,
                    r: &id,
                };
                serde_json::to_writer(&mut buf, &e).map_err(io::Error::other)?;
                buf.push(b'\n');
                new_revs.push((
                    page,
                    Rev {
                        ts: ts.clone(),
                        id,
                        text,
                    },
                ));
            }
        }
        for (ns, key) in &beats {
            let e = EntryRef::Beat {
                ts: &ts,
                ns,
                k: key,
            };
            serde_json::to_writer(&mut buf, &e).map_err(io::Error::other)?;
            buf.push(b'\n');
        }

        file.write_all(&buf).await?;
        file.sync_data().await?;

        let mut receipt = Receipt {
            ts: ts.clone(),
            ..Default::default()
        };
        let mut idx = self.index.write();
        for (i, (page, rev)) in new_revs.into_iter().enumerate() {
            let id = rev.id.clone();
            let p = idx.pages.entry(page.clone()).or_default();
            p.revs.push(rev);
            let n = p.revs.len();
            if i < user_appends {
                receipt.revs.push(RevReceipt { page, n, id });
            }
            idx.entries += 1;
        }
        for (ns, key) in beats {
            let b = beat(&mut idx, ns.clone(), key.clone(), &ts);
            receipt.beats.push(BeatReceipt { ns, key, beat: b });
            idx.entries += 1;
        }
        Ok(receipt)
    }

    /// Current text of a page: every append, each followed by `\n`.
    /// `tail` limits to the last N appends.
    pub fn page(&self, name: &str, tail: Option<usize>) -> Option<String> {
        let idx = self.index.read();
        let p = idx.pages.get(name)?;
        let revs = tail_slice(&p.revs, tail);
        let mut out = String::with_capacity(revs.iter().map(|r| r.text.len() + 1).sum());
        for r in revs {
            out.push_str(&r.text);
            out.push('\n');
        }
        Some(out)
    }

    /// (total revisions, [(1-based n, rev)]) — optionally only the last N.
    pub fn history(&self, name: &str, tail: Option<usize>) -> Option<(usize, Vec<(usize, Rev)>)> {
        let idx = self.index.read();
        let p = idx.pages.get(name)?;
        let total = p.revs.len();
        let start = match tail {
            Some(n) if n < total => total - n,
            _ => 0,
        };
        let revs = p.revs[start..]
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, r)| (start + i + 1, r))
            .collect();
        Some((total, revs))
    }

    /// Every page, sorted by name (byte order, so case-sensitive).
    pub fn listing(&self) -> Vec<Listing> {
        let idx = self.index.read();
        idx.pages
            .iter()
            .map(|(name, p)| Listing {
                name: name.clone(),
                revs: p.revs.len(),
                last: p.revs.last().map(|r| r.ts.clone()).unwrap_or_default(),
            })
            .collect()
    }

    pub fn beat(&self, ns: &str, key: &str) -> Option<Beat> {
        self.index.read().beats.get(ns)?.get(key).cloned()
    }

    pub fn beats(&self, ns: &str) -> Option<Vec<(String, Beat)>> {
        let idx = self.index.read();
        let m = idx.beats.get(ns)?;
        Some(m.iter().map(|(k, b)| (k.clone(), b.clone())).collect())
    }

    /// (namespace, keys, total beats)
    pub fn namespaces(&self) -> Vec<(String, usize, u64)> {
        let idx = self.index.read();
        idx.beats
            .iter()
            .map(|(ns, m)| (ns.clone(), m.len(), m.values().map(|b| b.count).sum()))
            .collect()
    }

    pub fn stats(&self) -> Stats {
        let idx = self.index.read();
        Stats {
            pages: idx.pages.len(),
            entries: idx.entries,
            skipped: idx.skipped,
        }
    }
}

fn tail_slice<T>(v: &[T], tail: Option<usize>) -> &[T] {
    match tail {
        Some(n) if n < v.len() => &v[v.len() - n..],
        _ => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn meta(query: Option<&str>) -> ReqMeta {
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", HeaderValue::from_static("test/1"));
        headers.insert("x-cohort", HeaderValue::from_static("7"));
        headers.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        headers.insert("cookie", HeaderValue::from_static("secret=1"));
        headers.insert("authorization", HeaderValue::from_static("Bearer nope"));
        ReqMeta {
            ip: "10.20.30.xxx".into(),
            method: "GET".into(),
            path: "/w".into(),
            query: query.map(str::to_owned),
            headers,
        }
    }

    #[tokio::test]
    async fn append_chain_log_and_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();

        let r1 = s
            .commit(
                &meta(Some("p=notes&t=one")),
                vec![Op::Append {
                    page: "notes".into(),
                    text: "one".into(),
                }],
            )
            .await
            .unwrap();
        assert_eq!(r1.revs.len(), 1);
        assert_eq!(r1.revs[0].n, 1);
        assert_eq!(r1.revs[0].id, rev_id("", &r1.ts, "one"));

        let r2 = s
            .commit(
                &meta(None),
                vec![
                    Op::Append {
                        page: "notes".into(),
                        text: "two\nlines".into(),
                    },
                    Op::Beat {
                        ns: "c7".into(),
                        key: "alive".into(),
                    },
                ],
            )
            .await
            .unwrap();
        assert_eq!(r2.revs[0].n, 2);
        assert_eq!(r2.revs[0].id, rev_id(&r1.revs[0].id, &r2.ts, "two\nlines"));
        assert_eq!(r2.beats[0].beat.count, 1);

        assert_eq!(s.page("notes", None).unwrap(), "one\ntwo\nlines\n");
        assert_eq!(s.page("notes", Some(1)).unwrap(), "two\nlines\n");
        let (total, hist) = s.history("notes", None).unwrap();
        assert_eq!(total, 2);
        assert_eq!(hist[1].0, 2);

        // every commit also wrote one _log line
        let log = s.page(LOG_PAGE, None).unwrap();
        assert_eq!(log.lines().count(), 2);
        assert!(log.contains("q=\"p=notes&t=one\""));
        assert!(log.contains("ua=\"test/1\""));
        assert!(log.contains("x-cohort=\"7\""));
        assert!(!log.contains("1.2.3.4"), "raw XFF must not leak: {log}");
        assert!(!log.contains("secret"), "cookies never logged");
        assert!(!log.contains("Bearer"), "authorization never logged");

        // rebuild from disk gives the same index
        drop(s);
        let s = Store::open(dir.path()).unwrap();
        let st = s.stats();
        assert_eq!(st.pages, 2);
        assert_eq!(st.entries, 5); // 2 appends + 2 log lines + 1 beat
        assert_eq!(st.skipped, 0);
        assert_eq!(s.page("notes", None).unwrap(), "one\ntwo\nlines\n");
        assert_eq!(s.history("notes", None).unwrap().1[1].1.id, r2.revs[0].id);
        assert_eq!(s.beat("c7", "alive").unwrap().count, 1);

        // heartbeat count survives and increments
        let r3 = s
            .commit(
                &meta(None),
                vec![Op::Beat {
                    ns: "c7".into(),
                    key: "alive".into(),
                }],
            )
            .await
            .unwrap();
        assert_eq!(r3.beats[0].beat.count, 2);
        assert_eq!(r3.beats[0].beat.first, r2.ts);
    }

    #[tokio::test]
    async fn partial_last_line_is_skipped_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        s.commit(
            &meta(None),
            vec![Op::Append {
                page: "a".into(),
                text: "ok".into(),
            }],
        )
        .await
        .unwrap();
        drop(s);
        let path = dir.path().join(LOG_FILE);
        let before = std::fs::metadata(&path).unwrap().len();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"t\":\"a\",\"ts\":\"x\",\"p\":\"a\",\"x\":\"trunc")
            .unwrap();
        let s = Store::open(dir.path()).unwrap();
        let st = s.stats();
        assert_eq!(st.entries, 2);
        assert_eq!(st.skipped, 1);
        let after = std::fs::metadata(&path).unwrap().len();
        assert!(after > before, "nothing removed, newline added");
        assert_eq!(s.page("a", None).unwrap(), "ok\n");
        // and the store keeps working on clean lines afterwards
        s.commit(
            &meta(None),
            vec![Op::Append {
                page: "a".into(),
                text: "again".into(),
            }],
        )
        .await
        .unwrap();
        drop(s);
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(s.page("a", None).unwrap(), "ok\nagain\n");
    }
}
