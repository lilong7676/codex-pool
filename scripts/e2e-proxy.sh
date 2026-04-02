#!/usr/bin/env bash
#
# End-to-end smoke test for the local proxy.
#
# Prerequisites:
#   - codex-pool has at least one usable account
#   - codex CLI is installed and authorized
#
# Usage:
#   ./scripts/e2e-proxy.sh [--port PORT] [--api-key KEY] [--timeout SECONDS]
#
set -euo pipefail

PORT="${E2E_PORT:-0}"
API_KEY="${E2E_API_KEY:-e2e-test-key}"
TIMEOUT="${E2E_TIMEOUT:-120}"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

pass=0
fail=0
TMPDIR_E2E=""
SERVER_PID=""
SERVER_LOG=""

log_pass() { echo -e "${GREEN}[PASS]${NC} $1"; pass=$((pass + 1)); }
log_fail() { echo -e "${RED}[FAIL]${NC} $1"; fail=$((fail + 1)); }
log_info() { echo -e "${YELLOW}[INFO]${NC} $1"; }

cleanup() {
    if [[ -n "$SERVER_PID" ]]; then
        log_info "Stopping proxy server (PID $SERVER_PID)..."
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    if [[ -n "$TMPDIR_E2E" && -d "$TMPDIR_E2E" ]]; then
        rm -rf "$TMPDIR_E2E"
    fi
}
trap cleanup EXIT

usage() {
    cat <<EOF
Usage: ./scripts/e2e-proxy.sh [--port PORT] [--api-key KEY] [--timeout SECONDS]
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --port) PORT="$2"; shift 2 ;;
        --api-key) API_KEY="$2"; shift 2 ;;
        --timeout) TIMEOUT="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1"; usage; exit 1 ;;
    esac
done

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "ERROR: required command not found: $1" >&2
        exit 1
    fi
}

python_json() {
    local expr="$1"
    python3 -c "import json,sys; data=json.load(sys.stdin); ${expr}"
}

assert_server_alive() {
    if [[ -n "$SERVER_PID" ]] && ! kill -0 "$SERVER_PID" 2>/dev/null; then
        log_fail "Proxy server exited prematurely"
        if [[ -n "$SERVER_LOG" && -f "$SERVER_LOG" ]]; then
            echo ""
            echo "----- proxy log -----"
            cat "$SERVER_LOG"
            echo "---------------------"
        fi
        exit 1
    fi
}

request_json() {
    local method="$1"
    local url="$2"
    local body="$3"
    local headers_file="$4"
    local body_file="$5"
    shift 5
    local status
    status=$(curl -sS --max-time "$TIMEOUT" -D "$headers_file" -o "$body_file" -w "%{http_code}" \
        -X "$method" "$url" "$@" \
        --data "$body")
    printf '%s' "$status"
}

request_get() {
    local url="$1"
    local headers_file="$2"
    local body_file="$3"
    local status
    status=$(curl -sS --max-time "$TIMEOUT" -D "$headers_file" -o "$body_file" -w "%{http_code}" "$url")
    printf '%s' "$status"
}

assert_header_present() {
    local headers_file="$1"
    local header_name="$2"
    local label="$3"
    if grep -qi "^${header_name}:" "$headers_file"; then
        log_pass "$label"
    else
        log_fail "$label"
    fi
}

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TMPDIR_E2E="$(mktemp -d)"
SERVER_LOG="$TMPDIR_E2E/proxy.log"

require_cmd cargo
require_cmd curl
require_cmd python3

log_info "Building codex-pool..."
cargo build --manifest-path "$PROJECT_DIR/Cargo.toml" >/dev/null
BINARY="$PROJECT_DIR/target/debug/codex-pool"
if [[ ! -x "$BINARY" ]]; then
    echo "ERROR: binary not found at $BINARY" >&2
    exit 1
fi

ACCOUNT_COUNT=$("$BINARY" list --json 2>/dev/null | python_json "print(len(data))" 2>/dev/null || echo "0")
if [[ "$ACCOUNT_COUNT" == "0" ]]; then
    echo "ERROR: No accounts configured in codex-pool. Run 'codex-pool add' first." >&2
    exit 1
fi
log_info "Found $ACCOUNT_COUNT account(s)"

if [[ "$PORT" == "0" ]]; then
    PORT=$(python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)
fi

LISTEN="127.0.0.1:$PORT"
BASE_URL="http://$LISTEN"

log_info "Starting proxy on $LISTEN ..."
"$BINARY" serve \
    --listen "$LISTEN" \
    --api-key "$API_KEY" \
    --sandbox "workspace-write" \
    --approval-policy "never" \
    >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 40); do
    if curl -sf "$BASE_URL/healthz" >/dev/null 2>&1; then
        break
    fi
    assert_server_alive
    sleep 0.5
done
assert_server_alive

HEALTH_HEADERS="$TMPDIR_E2E/health.headers"
HEALTH_BODY="$TMPDIR_E2E/health.json"
HEALTH_STATUS=$(request_get "$BASE_URL/healthz" "$HEALTH_HEADERS" "$HEALTH_BODY")
if [[ "$HEALTH_STATUS" == "200" ]]; then
    IS_OK=$(python_json "print(data.get('ok', False))" <"$HEALTH_BODY" 2>/dev/null || echo "False")
    ACCOUNT_COUNT_REPORTED=$(python_json "print(data.get('account_count', 0))" <"$HEALTH_BODY" 2>/dev/null || echo "0")
    if [[ "$IS_OK" == "True" ]]; then
        log_pass "/healthz returns ok=true"
    else
        log_fail "/healthz returned HTTP 200 but ok=false"
    fi
    if [[ "$ACCOUNT_COUNT_REPORTED" -ge 1 ]]; then
        log_pass "/healthz reports at least one account"
    else
        log_fail "/healthz reports zero accounts"
    fi
else
    log_fail "/healthz returned HTTP $HEALTH_STATUS"
fi

MODELS_HEADERS="$TMPDIR_E2E/models.headers"
MODELS_BODY="$TMPDIR_E2E/models.json"
MODELS_STATUS=$(request_get "$BASE_URL/v1/models" "$MODELS_HEADERS" "$MODELS_BODY")
if [[ "$MODELS_STATUS" == "200" ]]; then
    HAS_CODEX=$(python_json "print(any(item.get('id') == 'codex' for item in data.get('data', [])))" <"$MODELS_BODY" 2>/dev/null || echo "False")
    if [[ "$HAS_CODEX" == "True" ]]; then
        log_pass "/v1/models lists model alias 'codex'"
    else
        log_fail "/v1/models missing model alias 'codex'"
    fi
else
    log_fail "/v1/models returned HTTP $MODELS_STATUS"
fi

ADMIN_HEADERS="$TMPDIR_E2E/admin.headers"
ADMIN_BODY="$TMPDIR_E2E/admin.json"
ADMIN_STATUS=$(request_get "$BASE_URL/admin/accounts" "$ADMIN_HEADERS" "$ADMIN_BODY")
if [[ "$ADMIN_STATUS" == "200" ]]; then
    ADMIN_COUNT=$(python_json "print(len(data))" <"$ADMIN_BODY" 2>/dev/null || echo "0")
    HAS_STATUS=$(python_json "print(all('status' in item and 'inflight' in item and 'cooldown_until' in item for item in data))" <"$ADMIN_BODY" 2>/dev/null || echo "False")
    if [[ "$ADMIN_COUNT" -ge 1 ]]; then
        log_pass "/admin/accounts returns account rows"
    else
        log_fail "/admin/accounts returned zero rows"
    fi
    if [[ "$HAS_STATUS" == "True" ]]; then
        log_pass "/admin/accounts includes scheduler fields"
    else
        log_fail "/admin/accounts missing expected scheduler fields"
    fi
else
    log_fail "/admin/accounts returned HTTP $ADMIN_STATUS"
fi

BAD_OAI_HEADERS="$TMPDIR_E2E/bad-openai.headers"
BAD_OAI_BODY="$TMPDIR_E2E/bad-openai.json"
BAD_OAI_STATUS=$(request_json "POST" "$BASE_URL/v1/chat/completions" \
    '{"model":"codex","messages":[{"role":"user","content":"hi"}]}' \
    "$BAD_OAI_HEADERS" "$BAD_OAI_BODY" \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer wrong-key")
if [[ "$BAD_OAI_STATUS" == "401" ]]; then
    log_pass "OpenAI endpoint rejects invalid API key"
else
    log_fail "OpenAI endpoint expected HTTP 401, got $BAD_OAI_STATUS"
fi

BAD_ANTH_HEADERS="$TMPDIR_E2E/bad-anthropic.headers"
BAD_ANTH_BODY="$TMPDIR_E2E/bad-anthropic.json"
BAD_ANTH_STATUS=$(request_json "POST" "$BASE_URL/v1/messages" \
    '{"model":"codex","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}' \
    "$BAD_ANTH_HEADERS" "$BAD_ANTH_BODY" \
    -H "Content-Type: application/json" \
    -H "x-api-key: wrong-key" \
    -H "anthropic-version: 2023-06-01")
if [[ "$BAD_ANTH_STATUS" == "401" ]]; then
    log_pass "Anthropic endpoint rejects invalid API key"
else
    log_fail "Anthropic endpoint expected HTTP 401, got $BAD_ANTH_STATUS"
fi

BAD_VERSION_HEADERS="$TMPDIR_E2E/bad-version.headers"
BAD_VERSION_BODY="$TMPDIR_E2E/bad-version.json"
BAD_VERSION_STATUS=$(request_json "POST" "$BASE_URL/v1/messages" \
    '{"model":"codex","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}' \
    "$BAD_VERSION_HEADERS" "$BAD_VERSION_BODY" \
    -H "Content-Type: application/json" \
    -H "x-api-key: $API_KEY" \
    -H "anthropic-version: 2024-01-01")
if [[ "$BAD_VERSION_STATUS" == "400" ]]; then
    log_pass "Anthropic endpoint rejects unsupported anthropic-version"
else
    log_fail "Anthropic endpoint expected HTTP 400 for bad version, got $BAD_VERSION_STATUS"
fi

log_info "--- Test: OpenAI chat completion (non-streaming) ---"
OPENAI_HEADERS="$TMPDIR_E2E/openai.headers"
OPENAI_BODY="$TMPDIR_E2E/openai.json"
OPENAI_STATUS=$(request_json "POST" "$BASE_URL/v1/chat/completions" \
    '{"model":"codex","messages":[{"role":"system","content":"Be concise."},{"role":"user","content":"Reply in one short sentence about this proxy smoke test."}]}' \
    "$OPENAI_HEADERS" "$OPENAI_BODY" \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer $API_KEY")
if [[ "$OPENAI_STATUS" == "200" ]]; then
    OPENAI_TEXT=$(python_json "print(data['choices'][0]['message']['content'])" <"$OPENAI_BODY" 2>/dev/null || echo "")
    if [[ -n "${OPENAI_TEXT// }" ]]; then
        log_pass "OpenAI non-streaming returns assistant content"
    else
        log_fail "OpenAI non-streaming returned empty assistant content"
    fi
    assert_header_present "$OPENAI_HEADERS" "X-Codex-Pool-Account-Id" "OpenAI non-streaming includes X-Codex-Pool-Account-Id"
    assert_header_present "$OPENAI_HEADERS" "X-Codex-Pool-Account-Label" "OpenAI non-streaming includes X-Codex-Pool-Account-Label"
else
    log_fail "OpenAI non-streaming returned HTTP $OPENAI_STATUS"
    log_info "Body: $(cat "$OPENAI_BODY")"
fi

log_info "--- Test: OpenAI chat completion (streaming) ---"
OPENAI_STREAM_HEADERS="$TMPDIR_E2E/openai-stream.headers"
OPENAI_STREAM_BODY="$TMPDIR_E2E/openai-stream.txt"
OPENAI_STREAM_STATUS=$(request_json "POST" "$BASE_URL/v1/chat/completions" \
    '{"model":"codex","stream":true,"messages":[{"role":"user","content":"Reply in one short sentence about streaming."}]}' \
    "$OPENAI_STREAM_HEADERS" "$OPENAI_STREAM_BODY" \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer $API_KEY")
if [[ "$OPENAI_STREAM_STATUS" == "200" ]]; then
    if grep -q "chat.completion.chunk" "$OPENAI_STREAM_BODY"; then
        log_pass "OpenAI streaming returns chunk objects"
    else
        log_fail "OpenAI streaming missing chunk objects"
    fi
    if grep -q "data: \\[DONE\\]" "$OPENAI_STREAM_BODY"; then
        log_pass "OpenAI streaming ends with [DONE]"
    else
        log_fail "OpenAI streaming missing [DONE]"
    fi
    if grep -qi '^content-type: text/event-stream' "$OPENAI_STREAM_HEADERS"; then
        log_pass "OpenAI streaming returns text/event-stream"
    else
        log_fail "OpenAI streaming missing text/event-stream content type"
    fi
    assert_header_present "$OPENAI_STREAM_HEADERS" "X-Codex-Pool-Account-Id" "OpenAI streaming includes X-Codex-Pool-Account-Id"
else
    log_fail "OpenAI streaming returned HTTP $OPENAI_STREAM_STATUS"
    log_info "Body: $(head -c 400 "$OPENAI_STREAM_BODY")"
fi

log_info "--- Test: Anthropic messages (non-streaming) ---"
ANTHROPIC_HEADERS="$TMPDIR_E2E/anthropic.headers"
ANTHROPIC_BODY="$TMPDIR_E2E/anthropic.json"
ANTHROPIC_STATUS=$(request_json "POST" "$BASE_URL/v1/messages" \
    '{"model":"codex","system":"Be concise.","max_tokens":128,"messages":[{"role":"user","content":"Reply in one short sentence about this anthropic smoke test."}]}' \
    "$ANTHROPIC_HEADERS" "$ANTHROPIC_BODY" \
    -H "Content-Type: application/json" \
    -H "x-api-key: $API_KEY" \
    -H "anthropic-version: 2023-06-01")
if [[ "$ANTHROPIC_STATUS" == "200" ]]; then
    ANTHROPIC_TEXT=$(python_json "print(data['content'][0]['text'])" <"$ANTHROPIC_BODY" 2>/dev/null || echo "")
    if [[ -n "${ANTHROPIC_TEXT// }" ]]; then
        log_pass "Anthropic non-streaming returns assistant content"
    else
        log_fail "Anthropic non-streaming returned empty assistant content"
    fi
    assert_header_present "$ANTHROPIC_HEADERS" "X-Codex-Pool-Account-Id" "Anthropic non-streaming includes X-Codex-Pool-Account-Id"
    assert_header_present "$ANTHROPIC_HEADERS" "X-Codex-Pool-Account-Label" "Anthropic non-streaming includes X-Codex-Pool-Account-Label"
else
    log_fail "Anthropic non-streaming returned HTTP $ANTHROPIC_STATUS"
    log_info "Body: $(cat "$ANTHROPIC_BODY")"
fi

log_info "--- Test: Anthropic messages (streaming) ---"
ANTHROPIC_STREAM_HEADERS="$TMPDIR_E2E/anthropic-stream.headers"
ANTHROPIC_STREAM_BODY="$TMPDIR_E2E/anthropic-stream.txt"
ANTHROPIC_STREAM_STATUS=$(request_json "POST" "$BASE_URL/v1/messages" \
    '{"model":"codex","stream":true,"max_tokens":128,"messages":[{"role":"user","content":"Reply in one short sentence about anthropic streaming."}]}' \
    "$ANTHROPIC_STREAM_HEADERS" "$ANTHROPIC_STREAM_BODY" \
    -H "Content-Type: application/json" \
    -H "x-api-key: $API_KEY" \
    -H "anthropic-version: 2023-06-01")
if [[ "$ANTHROPIC_STREAM_STATUS" == "200" ]]; then
    if grep -q "event: message_start" "$ANTHROPIC_STREAM_BODY"; then
        log_pass "Anthropic streaming returns message_start"
    else
        log_fail "Anthropic streaming missing message_start"
    fi
    if grep -q "event: content_block_delta" "$ANTHROPIC_STREAM_BODY"; then
        log_pass "Anthropic streaming returns content_block_delta"
    else
        log_fail "Anthropic streaming missing content_block_delta"
    fi
    if grep -q "event: message_stop" "$ANTHROPIC_STREAM_BODY"; then
        log_pass "Anthropic streaming returns message_stop"
    else
        log_fail "Anthropic streaming missing message_stop"
    fi
    if grep -qi '^content-type: text/event-stream' "$ANTHROPIC_STREAM_HEADERS"; then
        log_pass "Anthropic streaming returns text/event-stream"
    else
        log_fail "Anthropic streaming missing text/event-stream content type"
    fi
    assert_header_present "$ANTHROPIC_STREAM_HEADERS" "X-Codex-Pool-Account-Id" "Anthropic streaming includes X-Codex-Pool-Account-Id"
else
    log_fail "Anthropic streaming returned HTTP $ANTHROPIC_STREAM_STATUS"
    log_info "Body: $(head -c 400 "$ANTHROPIC_STREAM_BODY")"
fi

echo ""
echo "========================================"
TOTAL=$((pass + fail))
echo -e "Results: ${GREEN}$pass passed${NC}, ${RED}$fail failed${NC} / $TOTAL total"
echo "========================================"

if [[ -n "$SERVER_LOG" && -f "$SERVER_LOG" ]]; then
    log_info "Proxy log saved at $SERVER_LOG"
fi

if [[ $fail -gt 0 ]]; then
    exit 1
fi
