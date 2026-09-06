//! The redaction hook.
//!
//! Every byte that reaches disk as page text — user appends AND the
//! per-request `_log` line built from the query, Referer, User-Agent and X-*
//! headers — passes through `filter` inside `Store::commit`. Nothing else
//! writes to the log. Pass 1 is the identity; pass 2 will scrub
//! credential-shaped strings (API keys, bearer tokens, cloud keys) here before
//! they become permanent. Leaked keys on a public, undeletable surface are the
//! documented failure mode this hook exists for.

use axum::http::HeaderMap;

pub fn filter(_page: &str, text: String, _headers: &HeaderMap) -> String {
    text
}
