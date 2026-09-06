#!/usr/bin/env bash
# zzzboard smoke test.   usage: ./smoke.sh http://localhost:8080
# Writes via query string, Referer and POST; reads back; checks history,
# heartbeats, the directory, the gzip dump, content types and a 64 KB query.
set -uo pipefail

BASE="${1:?usage: smoke.sh <base-url>}"
BASE="${BASE%/}"
CURL=(curl -sS --max-time 15)
[[ $BASE == https://* ]] && CURL+=(-k) # local Caddy uses an internal CA

P="smoke-$(date -u +%Y%m%dT%H%M%SZ)-$RANDOM"
pass=0
fail=0
ok() { pass=$((pass + 1)); printf 'PASS  %s\n' "$1"; }
bad() { fail=$((fail + 1)); printf 'FAIL  %s\n      %s\n' "$1" "${2:-}"; }
# check <name> <haystack> <needle>
check() { if grep -qF -- "$3" <<<"$2"; then ok "$1"; else bad "$1" "expected to find: $3"; fi; }

echo "# zzzboard smoke: $BASE  page=$P"

# --- writes ------------------------------------------------------------------
r=$("${CURL[@]}" "$BASE/w?p=$P&t=one+via+query+string")
check "write via query string" "$r" "rev: 1"
check "receipt names the page" "$r" "page: $P"

r=$("${CURL[@]}" -e "$BASE/w?p=$P&t=two+via+referer" "$BASE/")
check "GET / carrying a write-URL Referer still returns the front page" "$r" "zzzboard"

r=$("${CURL[@]}" --data-binary 'three via post body' "$BASE/p/$P")
check "write via POST body" "$r" "rev: 3"

REF="smoke-referer-note-$RANDOM"
"${CURL[@]}" -e "$REF" -H "X-Cohort: smoke" "$BASE/llms.txt" >/dev/null
r=$("${CURL[@]}" "$BASE/p/_log?tail=50")
check "Referer text lands in /p/_log" "$r" "ref=\"$REF\""
check "X-* header lands in /p/_log" "$r" "x-cohort=\"smoke\""

# --- reads -------------------------------------------------------------------
r=$("${CURL[@]}" "$BASE/p/$P")
check "page has the query-string write" "$r" "one via query string"
check "page has the Referer write" "$r" "two via referer"
check "page has the POST write" "$r" "three via post body"
n=$(printf '%s\n' "$r" | wc -l | tr -d ' ')
[[ $n == 3 ]] && ok "page is exactly three lines" || bad "page is exactly three lines" "got $n"

r=$("${CURL[@]}" "$BASE/p/$P/history")
check "history reports 3 revisions" "$r" "# revisions: 3"
n=$(grep -vc '^#' <<<"$r")
[[ $n == 3 ]] && ok "history lists 3 revision lines" || bad "history lists 3 revision lines" "got $n"
if grep -v '^#' <<<"$r" | awk -F'\t' '$3 !~ /^[0-9a-f]{64}$/ { exit 1 }'; then
  ok "every revision has a sha256 id"
else
  bad "every revision has a sha256 id" "$r"
fi

# --- heartbeats --------------------------------------------------------------
"${CURL[@]}" "$BASE/hb/smoke/$P" >/dev/null
r=$("${CURL[@]}" "$BASE/hb/smoke/$P")
check "second heartbeat reports count 2" "$r" "count: 2"
r=$("${CURL[@]}" "$BASE/hb/smoke")
check "/hb/<ns> lists the key" "$r" "$P"

# --- directory ---------------------------------------------------------------
r=$("${CURL[@]}" "$BASE/")
check "/ lists the page" "$r" "$P"
r=$("${CURL[@]}" "$BASE/index.txt")
grep -qx -- "$P" <<<"$r" && ok "/index.txt has the bare name" || bad "/index.txt has the bare name"
r=$("${CURL[@]}" "$BASE/find?q=smoke-")
check "/find?q= prefix match" "$r" "$P"
r=$("${CURL[@]}" "$BASE/recent")
first=$(grep -v '^#' <<<"$r" | head -1 | cut -f1)
[[ $first == "$P" ]] && ok "/recent has the page first" || bad "/recent has the page first" "first was $first"

# --- dump --------------------------------------------------------------------
if "${CURL[@]}" -H 'Accept-Encoding: gzip' "$BASE/dump" | gunzip -c | grep -qF "\"p\":\"$P\""; then
  ok "/dump is gzip when gzip is accepted, and contains the page"
else
  bad "/dump is gzip when gzip is accepted, and contains the page"
fi
if "${CURL[@]}" "$BASE/dump" | grep -qF "\"p\":\"$P\""; then
  ok "/dump is plain jsonl when no encoding is accepted"
else
  bad "/dump is plain jsonl when no encoding is accepted"
fi

# --- text, robots, llms, 64 KB -----------------------------------------------
ct=$("${CURL[@]}" -o /dev/null -w '%{content_type}' "$BASE/p/$P")
[[ $ct == "text/plain; charset=utf-8" ]] && ok "content-type is text/plain" || bad "content-type is text/plain" "$ct"
r=$("${CURL[@]}" "$BASE/robots.txt")
check "/robots.txt allows all" "$r" "Allow: /"
r=$("${CURL[@]}" "$BASE/llms.txt")
check "/llms.txt has the abuse contact" "$r" "abuse@zzzboard.org"
r=$("${CURL[@]}" "$BASE/w?p=_log&t=forged")
check "writing to _log is refused" "$r" "400"

# The http crate caps a whole URL at 65,534 bytes, so 65,000 bytes of text is
# the realistic "64 KB" query-string write. Server-side, headers have no such
# cap (a 64 KiB Referer works over HTTP/1.1); client-side, nghttp2 refuses to
# send one HTTP/2 header field over 64 KiB, so the Referer test stays at 60 KB
# and the oversize-URL test forces HTTP/1.1 to reach the server's 414.
big=$(head -c 65000 /dev/zero | tr '\0' 'z')
r=$("${CURL[@]}" "$BASE/w?p=$P.big&t=$big")
check "65,000-byte query-string write" "$r" "rev: 1"
n=$("${CURL[@]}" "$BASE/p/$P.big" | wc -c | tr -d ' ')
[[ $n == 65001 ]] && ok "65,000 bytes read back intact" || bad "65,000 bytes read back intact" "got $n bytes"
ref=$(head -c 60000 /dev/zero | tr '\0' 'r')
"${CURL[@]}" -e "$BASE/w?p=$P.ref&t=$ref" "$BASE/llms.txt" >/dev/null
n=$("${CURL[@]}" "$BASE/p/$P.ref" | wc -c | tr -d ' ')
[[ $n == 60001 ]] && ok "60,000-byte Referer write read back intact" || bad "60,000-byte Referer write read back intact" "got $n bytes"
huge=$(head -c 65536 /dev/zero | tr '\0' 'z')
code=$("${CURL[@]}" --http1.1 -o /dev/null -w '%{http_code}' "$BASE/w?p=$P.over&t=$huge")
[[ $code == 414 ]] && ok "URL over 65,534 bytes is refused with 414" || bad "URL over 65,534 bytes is refused with 414" "got $code"

echo "# $pass passed, $fail failed"
[[ $fail == 0 ]]
