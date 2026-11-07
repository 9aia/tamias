#!/usr/bin/env bash
# attack.sh — the adversarial battery for the `adversarial` peregrin
# campaign. Codifies the repro patterns from the T-008 (cf worker +
# vault-age findings) and T-009 (tamias serve PDP / wire-hardening
# findings) adversarial passes as executable checks, plus the T-006
# `tamias run` env-scrub and audit-redaction assertions.
#
# Every check runs against a throwaway age vault with FAKE values
# only — nothing here needs (or accepts) a real secret.
#
# Usage:
#   attack.sh [--full] [--keep-work]
#     --full       also run `cargo xtask test` (whole suite incl. the
#                  worker bun smoke tests when bun is installed)
#     --keep-work  don't rm the fixture tmpdir (debugging)
#
# Env:
#   TAMIAS_BIN   path to the tamias binary (default: ./target/debug/tamias,
#              built on demand)
#   FINDINGS   findings ndjson path (default: target/adversarial/findings.ndjson)
#
# Exit: 0 = clean, 1 = findings recorded, 2 = setup error.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TAMIAS="${TAMIAS_BIN:-$ROOT/target/debug/tamias}"
FINDINGS="${FINDINGS:-$ROOT/target/adversarial/findings.ndjson}"
FULL=0
KEEP_WORK=0
for arg in "$@"; do
    case "$arg" in
        --full) FULL=1 ;;
        --keep-work) KEEP_WORK=1 ;;
        *) echo "attack.sh: unknown arg $arg" >&2; exit 2 ;;
    esac
done

mkdir -p "$(dirname "$FINDINGS")"
: > "$FINDINGS"

if [ ! -x "$TAMIAS" ]; then
    echo "attack.sh: building tamias ($TAMIAS missing)…" >&2
    (cd "$ROOT" && cargo build -p tamias) || {
        echo "attack.sh: cargo build -p tamias failed" >&2
        exit 2
    }
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/tamias-adv.XXXXXX")"
[ "$KEEP_WORK" = 1 ] || trap 'rm -rf "$WORK"' EXIT

CANARY="CANARY-$(date +%s)-DO-NOT-USE"

finding() {
    # finding <check-id> <severity> <title> <detail>
    printf '{"check":"%s","severity":"%s","title":"%s","detail":"%s"}\n' \
        "$1" "$2" "${3//\"/\\\"}" "${4//\"/\\\"}" >> "$FINDINGS"
    echo "  FAIL $1: $3" >&2
}

pass() { echo "  ok   $1: $2" >&2; }

# --- fixtures ---------------------------------------------------------------

# mkvault <dir> — fresh age vault + identity + an tamias.toml, canary stored.
mkvault() {
    local d="$1"
    cat > "$d/tamias.toml" <<EOF
[vaults.local]
kind = "age"
path = "$d/vault.age"
identity = "$d/identity.txt"
EOF
    TAMIAS_CONFIG="$d/tamias.toml" "$TAMIAS" init --with-file >/dev/null 2>&1 \
        && printf '%s' "$CANARY" | TAMIAS_CONFIG="$d/tamias.toml" \
            "$TAMIAS" put tamias://test/key >/dev/null 2>&1
}

# serve <config> <input-file> — run the wire; replies on stdout,
# audit ndjson captured at <input-file>.audit.
serve() {
    "$TAMIAS" serve --config "$1" < "$2" 2>"$2.audit"
}

# pdp <dir> <body> — write a PDP script under <dir>, print its exec line.
pdp() {
    local script="$1/pdp.sh"
    printf '#!/bin/sh\n%s\n' "$2" > "$script"
    printf 'sh %s' "$script"
}

# --- A-01: deny blocks every vault-touching op; manifest still answers ------
check_deny_consults() {
    local d="$WORK/a01"; mkdir -p "$d"; mkvault "$d" || return
    cat >> "$d/tamias.toml" <<EOF
[serve.pdp]
exec = "$(pdp "$d" 'cat > /dev/null; printf %s "{\"allow\":false,\"reason\":\"closed\"}"')"
EOF
    printf '%s\n' \
        '{"id":1,"op":"get","ref":"tamias://test/key"}' \
        '{"id":2,"op":"list"}' \
        '{"id":3,"op":"check"}' \
        '{"id":4,"op":"manifest"}' > "$d/in.ndjson"
    serve "$d/tamias.toml" "$d/in.ndjson" > "$d/out.json"
    local bad=0
    # reply field order is {"error":{…},"id":N,…} — match per-id line
    grep '"id":1,' "$d/out.json" | grep -q '"denied"' || bad=1
    grep '"id":2,' "$d/out.json" | grep -q '"denied"' || bad=1
    grep '"id":3,' "$d/out.json" | grep -q '"denied"' || bad=1
    grep -q '"id":4,"ok":true' "$d/out.json" || bad=1
    if [ "$bad" = 1 ]; then
        finding A-01 high "deny-all PDP did not block get/list/check, or manifest stopped answering" "$(cat "$d/out.json")"
    else
        pass A-01 "deny consult gates get/list/check; manifest is a meta-op"
    fi
}

# --- A-02: a doomed request never spawns a PDP consult -----------------------
check_doomed_no_consult() {
    local d="$WORK/a02"; mkdir -p "$d"; mkvault "$d" || return
    cat >> "$d/tamias.toml" <<EOF
[serve.pdp]
exec = "$(pdp "$d" "cat > '$d/consulted'; printf %s '{\"allow\":true}'")"
EOF
    printf '%s\n' '{"id":1,"op":"put","ref":"tamias://test/key"}' > "$d/in.ndjson"
    serve "$d/tamias.toml" "$d/in.ndjson" > "$d/out.json"
    if ! grep -q '"bad_request"' "$d/out.json"; then
        finding A-02 med "value-less put was not rejected as bad_request" "$(cat "$d/out.json")"
    elif [ -e "$d/consulted" ]; then
        finding A-02 high "a doomed request spawned a PDP consult — validation must precede policy" "consult captured at $d/consulted"
    else
        pass A-02 "bad_request precedes the consult; no PDP spawn"
    fi
}

# --- A-03: a hung PDP is killed at timeout_ms and fails CLOSED --------------
check_hung_pdp() {
    local d="$WORK/a03"; mkdir -p "$d"; mkvault "$d" || return
    cat >> "$d/tamias.toml" <<'EOF'
[serve.pdp]
exec = "sleep 60"
timeout_ms = 400
EOF
    printf '%s\n' '{"id":1,"op":"get","ref":"tamias://test/key"}' > "$d/in.ndjson"
    local t0 t1
    t0=$(date +%s)
    serve "$d/tamias.toml" "$d/in.ndjson" > "$d/out.json"
    t1=$(date +%s)
    if ! grep -q '"pdp_unreachable"' "$d/out.json"; then
        finding A-03 crit "hung PDP was not reported pdp_unreachable (fail-closed violated)" "$(cat "$d/out.json")"
    elif [ $((t1 - t0)) -gt 10 ]; then
        finding A-03 high "hung PDP consult was not killed near timeout_ms" "elapsed $((t1 - t0))s"
    else
        pass A-03 "hung PDP killed at deadline; op denied pdp_unreachable"
    fi
}

# --- A-04: a flooding verdict is capped and fails CLOSED ---------------------
check_flood_pdp() {
    local d="$WORK/a04"; mkdir -p "$d"; mkvault "$d" || return
    cat >> "$d/tamias.toml" <<'EOF'
[serve.pdp]
exec = "cat >/dev/null; head -c 100000 /dev/zero | tr '\\0' 'x'"
timeout_ms = 3000
EOF
    printf '%s\n' '{"id":1,"op":"get","ref":"tamias://test/key"}' > "$d/in.ndjson"
    serve "$d/tamias.toml" "$d/in.ndjson" > "$d/out.json"
    if ! grep -q '"pdp_unreachable"' "$d/out.json"; then
        finding A-04 high "a >64KiB PDP verdict was not treated as unreachable" "$(cat "$d/out.json")"
    else
        pass A-04 "oversized verdict capped at 64KiB → pdp_unreachable"
    fi
}

# --- A-05: garbage verdict fails closed; allow_on_error degrades openly ------
check_garbage_pdp() {
    local d="$WORK/a05"; mkdir -p "$d"; mkvault "$d" || return
    cat >> "$d/tamias.toml" <<'EOF'
[serve.pdp]
exec = "cat >/dev/null; printf garbage"
EOF
    printf '%s\n' '{"id":1,"op":"get","ref":"tamias://test/key"}' > "$d/in.ndjson"
    serve "$d/tamias.toml" "$d/in.ndjson" > "$d/out.json"
    grep -q '"pdp_unreachable"' "$d/out.json" \
        || finding A-05 high "unparseable PDP verdict did not fail closed" "$(cat "$d/out.json")"
    # same misbehaving PDP under allow_on_error — op runs, degraded
    # verdict recorded on the reply + audit event
    cat > "$d/tamias.toml" <<EOF
[vaults.local]
kind = "age"
path = "$d/vault.age"
identity = "$d/identity.txt"

[serve.pdp]
exec = "cat >/dev/null; printf garbage"
allow_on_error = true
EOF
    printf '%s\n' '{"id":1,"op":"get","ref":"tamias://test/key"}' > "$d/in2.ndjson"
    serve "$d/tamias.toml" "$d/in2.ndjson" > "$d/out2.json"
    if ! grep -q '"ok":true' "$d/out2.json" || ! grep -q 'allow_on_error' "$d/out2.json"; then
        finding A-05 med "allow_on_error did not run the op with a recorded degraded verdict" "$(cat "$d/out2.json")"
    else
        pass A-05 "garbage verdict → fail closed; allow_on_error degrades with reason"
    fi
}

# --- A-06: the PDP child's env is scrubbed of parent credentials ------------
check_pdp_env_scrub() {
    local d="$WORK/a06"; mkdir -p "$d"; mkvault "$d" || return
    cat >> "$d/tamias.toml" <<EOF
[serve.pdp]
exec = "$(pdp "$d" "env > '$d/pdp-env'; printf %s '{\"allow\":true}'")"
EOF
    printf '%s\n' '{"id":1,"op":"get","ref":"tamias://test/key"}' > "$d/in.ndjson"
    TAMIAS_CF_TOKEN=fake CLOUDFLARE_API_TOKEN=fake TAMIAS_ADV_MARKER=leak \
        serve "$d/tamias.toml" "$d/in.ndjson" > /dev/null
    if grep -qE 'TAMIAS_CF_TOKEN|CLOUDFLARE_API_TOKEN|TAMIAS_ADV_MARKER' "$d/pdp-env" 2>/dev/null; then
        finding A-06 crit "parent credentials leaked into the PDP child's environment" "$(grep -E 'TAMIAS_|CLOUDFLARE' "$d/pdp-env")"
    else
        pass A-06 "PDP child env scrubbed to the tamias-shell allowlist"
    fi
}

# --- A-07: an oversized request line gets bad_request + resync ---------------
check_oversized_line() {
    local d="$WORK/a07"; mkdir -p "$d"; mkvault "$d" || return
    {
        printf '{'
        head -c 4300000 /dev/zero | tr '\0' 'x'
        printf '}\n'
        printf '{"id":9,"op":"manifest"}\n'
    } > "$d/in.ndjson"
    serve "$d/tamias.toml" "$d/in.ndjson" > "$d/out.json"
    if ! grep -q '"bad_request"' "$d/out.json"; then
        finding A-07 high ">4MiB request line was not rejected" "$(head -c 300 "$d/out.json")"
    elif ! grep -q '"id":9,"ok":true' "$d/out.json"; then
        finding A-07 med "reader did not resync to the next line after an oversized one" "$(head -c 300 "$d/out.json")"
    else
        pass A-07 "oversized line → bad_request, reader resyncs"
    fi
}

# --- A-08: oversized fields are rejected before any consult ------------------
check_oversized_fields() {
    local d="$WORK/a08"; mkdir -p "$d"; mkvault "$d" || return
    cat >> "$d/tamias.toml" <<EOF
[serve.pdp]
exec = "$(pdp "$d" "cat > '$d/consulted'; printf %s '{\"allow\":true}'")"
EOF
    local big_actor big_ref
    big_actor=$(head -c 300 /dev/zero | tr '\0' 'a')
    big_ref=$(head -c 1100 /dev/zero | tr '\0' 'r')
    printf '{"id":1,"op":"get","ref":"tamias://test/key","actor":"%s"}\n' "$big_actor" > "$d/in.ndjson"
    printf '{"id":2,"op":"get","ref":"%s"}\n' "$big_ref" >> "$d/in.ndjson"
    serve "$d/tamias.toml" "$d/in.ndjson" > "$d/out.json"
    local bad=0
    grep '"id":1,' "$d/out.json" | grep -q '"bad_request"' || bad=1
    grep '"id":2,' "$d/out.json" | grep -q '"bad_request"' || bad=1
    [ -e "$d/consulted" ] && bad=1
    if [ "$bad" = 1 ]; then
        finding A-08 high "oversized actor/ref fields were not capped before the consult" "$(cat "$d/out.json")"
    else
        pass A-08 "field caps reject over-size actor/ref before policy"
    fi
}

# --- A-09: `tamias run` scrubs the child env to an allowlist + declared secrets -
check_run_env_scrub() {
    local d="$WORK/a09"; mkdir -p "$d"; mkvault "$d" || return
    cat >> "$d/tamias.toml" <<'EOF'
[secrets]
CAMPAIGN_TOKEN = "tamias://test/key"
EOF
    local env_out
    env_out=$(TAMIAS_CF_TOKEN=fake CLOUDFLARE_API_TOKEN=fake TAMIAS_ADV_MARKER=leak \
        "$TAMIAS" run --config "$d/tamias.toml" -- 'env' 2>/dev/null)
    if printf '%s' "$env_out" | grep -qE '^(TAMIAS_CF_TOKEN|CLOUDFLARE_API_TOKEN|TAMIAS_ADV_MARKER)='; then
        finding A-09 crit "tamias run leaks undeclared parent env into the child" "$(printf '%s' "$env_out" | grep -E 'TAMIAS_|CLOUDFLARE')"
        return
    fi
    # positive control: the declared secret DOES inject
    local got
    got=$("$TAMIAS" run --config "$d/tamias.toml" -- 'printf %s "$CAMPAIGN_TOKEN"' 2>/dev/null)
    if [ "$got" != "$CANARY" ]; then
        finding A-09 high "declared [secrets] ref did not materialize into the child env" "got '$got'"
    else
        pass A-09 "child env = allowlist + declared secrets only"
    fi
}

# --- A-10: secret values never reach the audit stream -------------------------
check_audit_redaction() {
    local d="$WORK/a10"; mkdir -p "$d"; mkvault "$d" || return
    printf '%s\n' '{"id":1,"op":"get","ref":"tamias://test/key"}' > "$d/in.ndjson"
    serve "$d/tamias.toml" "$d/in.ndjson" > /dev/null
    if grep -q "$CANARY" "$d/in.ndjson.audit"; then
        finding A-10 crit "secret value leaked into the audit ndjson stream" "$(cat "$d/in.ndjson.audit")"
    else
        pass A-10 "audit events carry ref/actor/outcome — never the value"
    fi
}

# --- A-11: implicit ./tamias.toml carrying [serve.pdp] warns on stderr ----------
check_implicit_config_warning() {
    local d="$WORK/a11"; mkdir -p "$d"; mkvault "$d" || return
    cat >> "$d/tamias.toml" <<'EOF'
[serve.pdp]
exec = "cat >/dev/null; printf %s '{\"allow\":true}'"
EOF
    local err
    err=$(cd "$d" && "$TAMIAS" serve <<< '{"op":"shutdown"}' 2>&1 >/dev/null)
    if printf '%s' "$err" | grep -qiE 'implicit|trust'; then
        pass A-11 "implicit ./tamias.toml with pdp config warns on stderr"
    else
        finding A-11 med "no trust-root warning when [serve.pdp] loads from implicit ./tamias.toml" "$err"
    fi
}

# --- A-12: vault file/dir permission notes are non-fatal but visible ----------
check_permission_notes() {
    local d="$WORK/a12"; mkdir -p "$d"; mkvault "$d" || return
    chmod 0666 "$d/vault.age" 2>/dev/null || return 0
    local err
    err=$(TAMIAS_CONFIG="$d/tamias.toml" "$TAMIAS" list 2>&1 >/dev/null)
    if printf '%s' "$err" | grep -qiE 'perm|0600|0700'; then
        pass A-12 "permissive vault file surfaces a stderr note (F-3)"
    else
        # non-fatal is the contract — warn-level finding only if silent
        finding A-12 low "vault.age mode 0666 produced no permission note" "stderr: $err"
    fi
}

# --- A-FULL: the repo test suite (xtask test → worker smoke too) --------------
check_full_suite() {
    [ "$FULL" = 1 ] || return 0
    local d="$WORK/full"; mkdir -p "$d"
    if (cd "$ROOT" && cargo xtask test) > "$d/xtask-test.log" 2>&1; then
        pass A-FULL "cargo xtask test green (incl. worker smoke when bun present)"
    else
        finding A-FULL high "cargo xtask test failed" "$(tail -30 "$d/xtask-test.log")"
    fi
}

echo "attack.sh: battery against $TAMIAS (work: $WORK)" >&2
check_deny_consults
check_doomed_no_consult
check_hung_pdp
check_flood_pdp
check_garbage_pdp
check_pdp_env_scrub
check_oversized_line
check_oversized_fields
check_run_env_scrub
check_audit_redaction
check_implicit_config_warning
check_permission_notes
check_full_suite

count=$(wc -l < "$FINDINGS" | tr -d ' ')
echo "attack.sh: $count finding(s) → $FINDINGS" >&2
[ "$count" -eq 0 ] && exit 0 || exit 1
