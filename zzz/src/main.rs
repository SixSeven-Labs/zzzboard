//! zzz: the zzzboard client for agent teams. Post, ask and read a shared
//! page; as a Claude Code PostToolUse hook, push teammates' new posts into an
//! agent's context mid-turn, at most once per interval.
//!
//! Teammates only hear each other's messages between turns, and a turn with
//! momentum can run for fifteen minutes. The hook fires after every tool call,
//! so the board reaches an agent without the agent having to remember to look.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read, Write as _};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use chrono::{DateTime, Local, NaiveTime, TimeDelta, TimeZone, Utc};
use serde::{Deserialize, Serialize};

const USAGE: &str = "\
zzz: zzzboard client for agent teams

  zzz post  <page> <me> <text|->        append \"[me] text\" (- reads stdin), then show what's new
  zzz ask   <page> <me> <to> <text|->   append \"[me] QUESTION for <to>: text\", then show what's new
  zzz check <page> <me> [--last N]      posts by others since your last check
  zzz read  <page> [--last N] [--since T]
                                        T: 2m | 90s | 1h | 23:40 | 11:40pm | 2026-10-15T03:40:00Z
  zzz leave <page> <me>                 stop the hook's checks for this agent
  zzz up                                start the board if it is down
  zzz hook                              Claude Code PostToolUse hook (reads the event on stdin)

env: ZZZBOARD_URL (default http://localhost:8080)
     ZZZ_EVERY   seconds between hook checks (default 120)
     ZZZBOARD_HOME  server checkout for `up` (default B:\\zzzboard)
";

/// Mark used when a page did not exist at the last check: everything is new.
const EPOCH: &str = "1970-01-01T00:00:00.000Z";
/// Most text one hook injection may carry; older posts beyond it are listed, not shown.
const HOOK_CAP: usize = 12_000;

type Res<T> = Result<T, String>;

/// stdout that shrugs off a closed pipe (`zzz check ... | Select-Object -First 3`)
/// instead of panicking the way `println!` does.
macro_rules! outln {
    ($($a:tt)*) => {{
        let _ = writeln!(io::stdout(), $($a)*);
    }};
}
macro_rules! out {
    ($($a:tt)*) => {{
        let _ = write!(io::stdout(), $($a)*);
    }};
}
fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let verb = args.first().map(String::as_str).unwrap_or("");
    let rest = args.get(1..).unwrap_or(&[]);
    let run = match verb {
        "post" => cmd_post(rest, None),
        "ask" => cmd_ask(rest),
        "check" => cmd_check(rest),
        "read" => cmd_read(rest),
        "leave" => cmd_leave(rest),
        "up" => cmd_up(),
        // A hook must never break the tool call it rides on: errors go to a
        // log file and the hook exits 0 with no output.
        "hook" => {
            if let Err(e) = cmd_hook() {
                log_hook_error(&e);
            }
            return ExitCode::SUCCESS;
        }
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zzz {verb}: {e}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------- commands

fn cmd_post(args: &[String], question_for: Option<&str>) -> Res<()> {
    let (pos, _) = split_flags(args)?;
    let [page, me, text] = pos.as_slice() else {
        return Err("usage: zzz post <page> <me> <text|->".into());
    };
    check_name("page", page)?;
    check_name("me", me)?;
    let text = text_arg(text)?;
    let body = match question_for {
        Some(to) => format!("[{me}] QUESTION for {to}: {text}"),
        None => format!("[{me}] {text}"),
    };
    let receipt = append(page, &body)?;
    outln!("posted #{} to {page} at {}", receipt.0, local_hms(&receipt.1));
    print_fresh(page, me, None)
}

fn cmd_ask(args: &[String]) -> Res<()> {
    let (pos, _) = split_flags(args)?;
    let [page, me, to, text] = pos.as_slice() else {
        return Err("usage: zzz ask <page> <me> <to> <text|->".into());
    };
    check_name("to", to)?;
    cmd_post(&[page.clone(), me.clone(), text.clone()], Some(to))
}

fn cmd_check(args: &[String]) -> Res<()> {
    let (pos, flags) = split_flags(args)?;
    let [page, me] = pos.as_slice() else {
        return Err("usage: zzz check <page> <me> [--last N]".into());
    };
    check_name("page", page)?;
    check_name("me", me)?;
    print_fresh(page, me, flags.last)
}

fn cmd_read(args: &[String]) -> Res<()> {
    let (pos, flags) = split_flags(args)?;
    let [page] = pos.as_slice() else {
        return Err("usage: zzz read <page> [--last N] [--since T]".into());
    };
    check_name("page", page)?;
    let mut q = Vec::new();
    if let Some(s) = &flags.since {
        q.push(format!("since={}", parse_since(s)?));
    }
    match flags.last {
        Some(n) => q.push(format!("tail={n}")),
        None if flags.since.is_none() => q.push("tail=10".into()),
        None => {}
    }
    let Some((total, posts)) = history(page, &q.join("&"), 5)? else {
        outln!("zzzboard {page}: no posts yet");
        return Ok(());
    };
    outln!("zzzboard {page}: {} of {total} posts", posts.len());
    out!("{}", render(&posts, None));
    Ok(())
}

/// The hook, not this command, does the leaving: only the hook knows which
/// agent ran it. This just confirms the words.
fn cmd_leave(args: &[String]) -> Res<()> {
    let (pos, _) = split_flags(args)?;
    let [page, me] = pos.as_slice() else {
        return Err("usage: zzz leave <page> <me>".into());
    };
    check_name("page", page)?;
    check_name("me", me)?;
    outln!("left {page} as {me}: the hook stops checking the board for this agent after this call");
    Ok(())
}

fn cmd_up() -> Res<()> {
    if board_up() {
        outln!("zzzboard is up at {}", base());
        return Ok(());
    }
    let home = PathBuf::from(env::var("ZZZBOARD_HOME").unwrap_or_else(|_| r"B:\zzzboard".into()));
    let exe = home.join("target").join("release").join("zzzboard.exe");
    if !exe.exists() {
        return Err(format!("no server binary at {} (cargo build --release there)", exe.display()));
    }
    let out = fs::File::create(home.join("server.out.log")).map_err(|e| e.to_string())?;
    let err = fs::File::create(home.join("server.err.log")).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&exe);
    cmd.current_dir(&home)
        .env("ZZZ_DATA_DIR", home.join("data"))
        .env("ZZZ_PORT", port_of(&base()))
        .env("ZZZ_BASE_URL", base())
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    spawn_detached(&mut cmd)?;
    for _ in 0..25 {
        std::thread::sleep(Duration::from_millis(200));
        if board_up() {
            outln!("zzzboard started at {} (logs: {}\\server.err.log)", base(), home.display());
            return Ok(());
        }
    }
    Err(format!("started {} but {} is not answering; see {}\\server.err.log", exe.display(), base(), home.display()))
}

/// PostToolUse. A tool call that ran `zzz post|ask|check <page> <me>` tells
/// us who this agent is on the board; every later tool call by the same agent
/// checks the board once per interval and injects what teammates posted.
fn cmd_hook() -> Res<()> {
    #[derive(Deserialize)]
    struct Event {
        session_id: Option<String>,
        agent_id: Option<String>,
        tool_input: Option<serde_json::Value>,
    }
    let mut raw = String::new();
    io::stdin().read_to_string(&mut raw).map_err(|e| e.to_string())?;
    let ev: Event = serde_json::from_str(&raw).map_err(|e| format!("bad hook input: {e}"))?;
    // Teammates and subagents carry agent_id; the lead does not, so it is
    // keyed by session.
    let key = match (&ev.agent_id, &ev.session_id) {
        (Some(a), _) => format!("agent-{a}"),
        (None, Some(s)) => format!("main-{s}"),
        (None, None) => return Ok(()),
    };
    let who_path = state_dir().join(format!("who.{}.json", file_safe(&key)));

    let command = ev
        .tool_input
        .as_ref()
        .and_then(|t| t.get("command"))
        .and_then(|c| c.as_str());
    if let Some((verb, page, me)) = command.and_then(board_identity) {
        if verb == "leave" {
            let _ = fs::remove_file(&who_path);
            return Ok(());
        }
        // That call already checked the board itself; just remember who ran it.
        return save_json(&who_path, &Who { page, me });
    }

    let Some(who) = load_json::<Who>(&who_path) else {
        return Ok(()); // not on a board
    };
    let every = env::var("ZZZ_EVERY").ok().and_then(|v| v.parse().ok()).unwrap_or(120i64);
    if let Some(m) = load_json::<Mark>(&mark_path(&who.page, &who.me)) {
        if Utc::now().timestamp() - m.checked < every {
            return Ok(());
        }
    }
    let f = fresh(&who.page, &who.me, None, 2)?;
    if f.posts.is_empty() {
        return Ok(());
    }

    // Newest posts win the space; anything older is pointed at, not dropped silently.
    let mut shown = f.posts.len();
    let mut body = render(&f.posts, Some(&who.me));
    while body.len() > HOOK_CAP && shown > 1 {
        shown -= 1;
        body = render(&f.posts[f.posts.len() - shown..], Some(&who.me));
    }
    let page = &who.page;
    let me = &who.me;
    let mut ctx = format!(
        "zzzboard: {} new post(s) on '{page}' from teammates since {}. This is the automatic \
         mid-turn board check (every {every}s), not a user message. Read them now. If one is a \
         question for you or changes your task, answer on the board before you continue:\n  \
         zzz post {page} {me} '@<who> <answer>'\n\n",
        f.posts.len(),
        local_hms(&f.since),
    );
    if shown < f.posts.len() {
        let _ = writeln!(
            ctx,
            "({} older new post(s) not shown: zzz read {page} --since {})\n",
            f.posts.len() - shown,
            f.since
        );
    }
    ctx.push_str(&body);
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": ctx,
        }
    });
    outln!("{out}");
    Ok(())
}

// ---------------------------------------------------------------- the mark

/// Where `me` has read `page` up to: the newest revision seen (its stamp and
/// number) and when the board was last asked. Stamps come from the server, so
/// "since my mark" never loses a post to clock skew between agents.
#[derive(Serialize, Deserialize, Clone)]
struct Mark {
    last: String,
    n: usize,
    checked: i64,
}

#[derive(Serialize, Deserialize)]
struct Who {
    page: String,
    me: String,
}

struct Fresh {
    /// Posts by others, oldest first.
    posts: Vec<Post>,
    /// New revisions that `--last` cut (or, on a first check, older history).
    skipped: usize,
    total: usize,
    /// The mark these posts are newer than (EPOCH on a first check).
    since: String,
    first: bool,
}

/// Posts by others since `me`'s mark on `page`, advancing the mark. With no
/// mark yet: the last 10 (or `tail`). Own posts advance the mark but are not
/// returned.
fn fresh(page: &str, me: &str, tail: Option<usize>, timeout: u64) -> Res<Fresh> {
    let path = mark_path(page, me);
    let old = load_json::<Mark>(&path);
    let query = match (&old, tail) {
        (Some(m), Some(n)) => format!("since={}&tail={n}", m.last),
        (Some(m), None) => format!("since={}", m.last),
        (None, n) => format!("tail={}", n.unwrap_or(10)),
    };
    let now = Utc::now().timestamp();
    let since = old.as_ref().map_or(EPOCH.to_owned(), |m| m.last.clone());
    let Some((total, posts)) = history(page, &query, timeout)? else {
        save_json(&path, &Mark { last: EPOCH.into(), n: 0, checked: now })?;
        return Ok(Fresh { posts: vec![], skipped: 0, total: 0, since, first: old.is_none() });
    };
    let prev_n = old.as_ref().map_or(0, |m| m.n);
    let skipped = posts.first().map_or(0, |p| p.n.saturating_sub(prev_n + 1));
    let mark = match posts.last() {
        Some(p) => Mark { last: p.ts.clone(), n: p.n, checked: now },
        None => Mark {
            checked: now,
            ..old.clone().unwrap_or(Mark { last: EPOCH.into(), n: 0, checked: now })
        },
    };
    save_json(&path, &mark)?;
    let mine = format!("[{me}]");
    let posts = posts.into_iter().filter(|p| !p.text.starts_with(&mine)).collect();
    Ok(Fresh { posts, skipped, total, since, first: old.is_none() })
}

fn print_fresh(page: &str, me: &str, tail: Option<usize>) -> Res<()> {
    let f = fresh(page, me, tail, 5)?;
    if f.first {
        outln!(
            "zzzboard {page}: first check. {} post(s) by others in the last {} of {}{}",
            f.posts.len(),
            tail.unwrap_or(10).min(f.total),
            f.total,
            if f.skipped > 0 { format!(" (older: zzz read {page} --last N)") } else { String::new() }
        );
    } else if f.posts.is_empty() {
        outln!("zzzboard {page}: nothing new from others since {}", local_hms(&f.since));
        return Ok(());
    } else {
        outln!("zzzboard {page}: {} new from others since {}", f.posts.len(), local_hms(&f.since));
        if f.skipped > 0 {
            outln!("({} newer than your mark were cut by --last: zzz read {page} --since {})", f.skipped, f.since);
        }
    }
    out!("{}", render(&f.posts, Some(me)));
    Ok(())
}

// ---------------------------------------------------------------- the board

struct Post {
    n: usize,
    ts: String,
    text: String,
}

fn base() -> String {
    env::var("ZZZBOARD_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| v.trim_end_matches('/').to_owned())
        .unwrap_or_else(|| "http://localhost:8080".into())
}

fn agent(timeout_secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout_secs)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn unreachable(e: ureq::Error) -> String {
    format!("zzzboard not reachable at {} ({e}). start it: zzz up", base())
}

fn board_up() -> bool {
    agent(2)
        .get(format!("{}/llms.txt", base()))
        .call()
        .is_ok_and(|r| r.status().is_success())
}

/// `/p/<page>/history?<query>` parsed; None when the page does not exist yet.
fn history(page: &str, query: &str, timeout: u64) -> Res<Option<(usize, Vec<Post>)>> {
    let url = format!("{}/p/{page}/history?{query}", base());
    let mut res = agent(timeout).get(&url).call().map_err(unreachable)?;
    let status = res.status().as_u16();
    let body = res.body_mut().read_to_string().map_err(|e| e.to_string())?;
    match status {
        200 => Ok(Some(parse_history(&body))),
        404 => Ok(None),
        s => Err(format!("board answered {s}: {}", body.trim())),
    }
}

/// History lines are `n \t utc \t sha256 \t bytes \t text-as-json`.
fn parse_history(body: &str) -> (usize, Vec<Post>) {
    let mut total = 0;
    let mut posts = Vec::new();
    for line in body.lines() {
        if let Some(t) = line.strip_prefix("# revisions: ") {
            total = t.trim().parse().unwrap_or(0);
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.splitn(5, '\t').collect();
        let [n, ts, _id, _bytes, json] = f.as_slice() else { continue };
        let Ok(text) = serde_json::from_str::<String>(json) else { continue };
        posts.push(Post { n: n.parse().unwrap_or(0), ts: (*ts).to_owned(), text });
    }
    (total, posts)
}

/// POST the text as the raw body (no escaping limits, multi-line safe).
/// Returns (revision number, server time).
fn append(page: &str, text: &str) -> Res<(String, String)> {
    let url = format!("{}/p/{page}", base());
    let mut res = agent(5)
        .post(&url)
        .header("Content-Type", "text/plain; charset=utf-8")
        .send(text.as_bytes())
        .map_err(unreachable)?;
    let status = res.status().as_u16();
    let body = res.body_mut().read_to_string().map_err(|e| e.to_string())?;
    if status != 200 {
        return Err(format!("board answered {status}: {}", body.trim()));
    }
    let field = |k: &str| {
        body.lines()
            .find_map(|l| l.strip_prefix(k))
            .map(|v| v.trim().to_owned())
            .unwrap_or_default()
    };
    Ok((field("rev:"), field("time:")))
}

// ---------------------------------------------------------------- rendering

fn render(posts: &[Post], me: Option<&str>) -> String {
    let mut out = String::new();
    for p in posts {
        let _ = writeln!(out, "--- #{} {}{}", p.n, local_hms(&p.ts), flag(&p.text, me));
        out.push_str(p.text.trim_end());
        out.push('\n');
    }
    out
}

/// Calls out posts addressed to `me`: `[x] QUESTION for me: ...` or `@me`.
fn flag(text: &str, me: Option<&str>) -> &'static str {
    let Some(me) = me else { return "" };
    let ends_name = |rest: &str| !rest.starts_with(|c: char| is_name_char(c));
    let asked = text
        .split_once("] ")
        .and_then(|(_, r)| r.strip_prefix("QUESTION for "))
        .and_then(|r| r.strip_prefix(me))
        .is_some_and(ends_name);
    if asked {
        return "  <<< QUESTION FOR YOU";
    }
    let at = format!("@{me}");
    if text.match_indices(&at).any(|(i, _)| ends_name(&text[i + at.len()..])) {
        return "  <<< mentions you";
    }
    ""
}

fn local_hms(ts: &str) -> String {
    if ts == EPOCH {
        return "the start".into();
    }
    DateTime::parse_from_rfc3339(ts)
        .map(|t| t.with_timezone(&Local).format("%H:%M:%S").to_string())
        .unwrap_or_else(|_| ts.to_owned())
}

// ---------------------------------------------------------------- parsing

#[derive(Default)]
struct Flags {
    last: Option<usize>,
    since: Option<String>,
}

fn split_flags(args: &[String]) -> Res<(Vec<String>, Flags)> {
    let mut pos = Vec::new();
    let mut flags = Flags::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--last" => {
                let v = it.next().ok_or("--last needs a number")?;
                flags.last = Some(v.parse().map_err(|_| format!("--last {v}: not a number"))?);
            }
            "--since" => flags.since = Some(it.next().ok_or("--since needs a time")?.clone()),
            _ => pos.push(a.clone()),
        }
    }
    Ok((pos, flags))
}

fn text_arg(t: &str) -> Res<String> {
    let text = if t == "-" {
        let mut s = String::new();
        io::stdin().read_to_string(&mut s).map_err(|e| e.to_string())?;
        s
    } else {
        t.to_owned()
    };
    let text = text.trim_end().to_owned();
    if text.trim().is_empty() {
        return Err("nothing to post".into());
    }
    Ok(text)
}

/// `2m`, `90s`, `1h` ago; a local clock time today (yesterday if that is
/// still ahead); or any RFC 3339 time. Returned in the board's stamp format.
fn parse_since(s: &str) -> Res<String> {
    let s = s.trim();
    let stamp = |t: DateTime<Utc>| t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    if s.is_ascii() && s.len() > 1 {
        let (num, unit) = s.split_at(s.len() - 1);
        if let Ok(n) = num.parse::<i64>() {
            let secs = match unit {
                "s" => Some(n),
                "m" => Some(n * 60),
                "h" => Some(n * 3600),
                _ => None,
            };
            if let Some(secs) = secs {
                return Ok(stamp(Utc::now() - TimeDelta::seconds(secs)));
            }
        }
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(stamp(t.with_timezone(&Utc)));
    }
    let clock = s.to_ascii_uppercase().replace(' ', "");
    for f in ["%H:%M", "%H:%M:%S", "%I:%M%p", "%I:%M:%S%p"] {
        if let Ok(t) = NaiveTime::parse_from_str(&clock, f) {
            let now = Local::now();
            if let Some(mut at) = Local.from_local_datetime(&now.date_naive().and_time(t)).earliest() {
                if at > now {
                    at -= TimeDelta::days(1);
                }
                return Ok(stamp(at.with_timezone(&Utc)));
            }
        }
    }
    Err(format!(
        "can't read --since {s:?}: use 2m / 90s / 1h, a clock time like 23:40 or 11:40pm, or 2026-10-15T03:40:00Z"
    ))
}

/// `(verb, page, me)` from a shell command that runs
/// `zzz post|ask|check|leave <page> <me> ...`.
fn board_identity(command: &str) -> Option<(String, String, String)> {
    let toks: Vec<&str> = command
        .split_whitespace()
        .map(|t| t.trim_start_matches('&').trim_matches(|c| c == '"' || c == '\''))
        .collect();
    toks.windows(4).find_map(|w| {
        let name = w[0].rsplit(['/', '\\']).next().unwrap_or("").to_ascii_lowercase();
        let is_zzz = name == "zzz" || name == "zzz.exe";
        (is_zzz
            && matches!(w[1], "post" | "ask" | "check" | "leave")
            && valid_name(w[2])
            && valid_name(w[3]))
        .then(|| (w[1].to_owned(), w[2].to_owned(), w[3].to_owned()))
    })
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.chars().all(is_name_char)
}

fn check_name(what: &str, s: &str) -> Res<()> {
    if valid_name(s) {
        Ok(())
    } else {
        Err(format!("bad {what} {s:?}: allowed [A-Za-z0-9_.-]{{1,128}}"))
    }
}

fn port_of(base: &str) -> String {
    base.rsplit(':')
        .next()
        .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or("8080")
        .to_owned()
}

// ---------------------------------------------------------------- files

fn state_dir() -> PathBuf {
    env::temp_dir().join("zzzboard")
}

fn mark_path(page: &str, me: &str) -> PathBuf {
    state_dir().join(format!("{page}.{me}.json"))
}

fn file_safe(s: &str) -> String {
    s.chars().map(|c| if is_name_char(c) { c } else { '_' }).collect()
}

fn load_json<T: for<'de> Deserialize<'de>>(path: &PathBuf) -> Option<T> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn save_json<T: Serialize>(path: &PathBuf, v: &T) -> Res<()> {
    fs::create_dir_all(state_dir()).map_err(|e| e.to_string())?;
    let s = serde_json::to_string(v).map_err(|e| e.to_string())?;
    fs::write(path, s).map_err(|e| format!("{}: {e}", path.display()))
}

fn log_hook_error(e: &str) {
    let line = format!("{} {e}\n", Utc::now().to_rfc3339());
    let _ = fs::create_dir_all(state_dir());
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir().join("hook.err.log"))
        .and_then(|mut f| io::Write::write_all(&mut f, line.as_bytes()));
}

/// Start the server outside our console and, when the job allows it, outside
/// our job object, so it outlives the tool call that launched it.
#[cfg(windows)]
fn spawn_detached(cmd: &mut Command) -> Res<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    if cmd.creation_flags(base | CREATE_BREAKAWAY_FROM_JOB).spawn().is_ok() {
        return Ok(());
    }
    cmd.creation_flags(base).spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn spawn_detached(cmd: &mut Command) -> Res<()> {
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_from_commands() {
        let id = |c| board_identity(c);
        let want = |v: &str| Some((v.to_owned(), "team".to_owned(), "lane-a".to_owned()));
        assert_eq!(id(r"C:\Users\me\.cargo\bin\zzz.exe check team lane-a"), want("check"));
        assert_eq!(id(r"& 'C:\x\zzz.exe' post team lane-a 'hello there'"), want("post"));
        assert_eq!(id("zzz ask team lane-a lead 'ok to touch Foo.cs?'"), want("ask"));
        assert_eq!(id("/c/x/zzz.exe check team lane-a --last 5"), want("check"));
        assert_eq!(id("zzz leave team lane-a"), want("leave"));
        assert_eq!(id("zzz read team --last 5"), None);
        assert_eq!(id("git status"), None);
        assert_eq!(id("echo zzz check"), None);
    }

    #[test]
    fn flags_questions_and_mentions() {
        assert_eq!(flag("[lead] QUESTION for lane-a: go?", Some("lane-a")), "  <<< QUESTION FOR YOU");
        assert_eq!(flag("[lead] QUESTION for lane-ab: go?", Some("lane-a")), "");
        assert_eq!(flag("[lead] @lane-a done, take it", Some("lane-a")), "  <<< mentions you");
        assert_eq!(flag("[lead] @lane-ab done", Some("lane-a")), "");
        assert_eq!(flag("[lead] all clear", Some("lane-a")), "");
    }

    #[test]
    fn since_forms() {
        assert_eq!(parse_since("2026-10-14T23:40:00-04:00").unwrap(), "2026-10-15T03:40:00.000Z");
        for s in ["2m", "90s", "1h", "23:40", "11:40pm", "11:40 PM", "23:40:15"] {
            let got = parse_since(s).unwrap();
            let t = DateTime::parse_from_rfc3339(&got).unwrap().with_timezone(&Utc);
            assert!(t <= Utc::now(), "{s} -> {got} is in the future");
            assert!(Utc::now() - t <= TimeDelta::days(1), "{s} -> {got} is over a day back");
        }
        assert!(parse_since("yesterday").is_err());
        assert!(parse_since("m").is_err());
    }

    #[test]
    fn history_lines() {
        let body = "# page: t\n# revisions: 7\n# n\tutc\tsha256\tbytes\ttext(json)\n\
                    6\t2026-10-15T03:40:00.000Z\tabc\t9\t\"[a] hi\\nyo\"\n";
        let (total, posts) = parse_history(body);
        assert_eq!(total, 7);
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].n, 6);
        assert_eq!(posts[0].text, "[a] hi\nyo");
    }

    #[test]
    fn ports() {
        assert_eq!(port_of("http://localhost:8080"), "8080");
        assert_eq!(port_of("http://localhost:9000"), "9000");
        assert_eq!(port_of("https://zzzboard.org"), "8080");
    }
}
