#!/usr/bin/env bash
# Тест deploy/render.sh: без алиасов вывод не меняется (серверы владельца), с алиасами — имена в сертификатах и SNI.
#   bash deploy/tests/render_test.sh
set -euo pipefail
cd "$(dirname "$0")"
HERE=$(pwd)
PY=$(command -v python3 || command -v python)
fail() { echo "FAIL: $*" >&2; exit 1; }

run_render() { # run_render DIR — .env уже в DIR
  cp "$HERE/../render.sh" "$1/render.sh"
  (cd "$1" && sh ./render.sh >/dev/null)
}

base_env() {
  cat <<'EOF'
COMPOSE_PROFILES=media,caddy,backend
LK_DOMAIN=lk.example.com
TURN_DOMAIN=turn.example.com
API_DOMAIN=api.example.com
CADDY_SERVE_API=1
ACME_EMAIL=admin@example.com
PUBLIC_IP=203.0.113.10
LIVEKIT_API_KEY=APItestkey
LIVEKIT_API_SECRET=test-secret
LK_WEBHOOK_URL=http://127.0.0.1:8080/livekit/webhook
EOF
}

T=$(mktemp -d); trap 'rm -rf "$T"' EXIT

# 1. без алиасов — байт-в-байт как эталон (снят с render.sh до алиасов)
mkdir "$T/plain"; base_env > "$T/plain/.env"
run_render "$T/plain"
cmp -s "$T/plain/generated/caddy.json" golden/caddy.json || fail "caddy.json без алиасов изменился"
cmp -s "$T/plain/generated/livekit.yaml" golden/livekit.yaml || fail "livekit.yaml без алиасов изменился"
echo "ok: без алиасов — как раньше"

# 2. с алиасами
mkdir "$T/alias"; base_env > "$T/alias/.env"
cat >> "$T/alias/.env" <<'EOF'
LK_DOMAIN_ALIASES=lk.203-0-113-10.sslip.io,lk.old.example.org
TURN_DOMAIN_ALIASES=turn.203-0-113-10.sslip.io
API_DOMAIN_ALIASES=api.203-0-113-10.sslip.io
EOF
run_render "$T/alias"
"$PY" -X utf8 -I - "$T/alias/generated/caddy.json" <<'EOF' || fail "алиасы"
import json, sys
c = json.load(open(sys.argv[1], encoding="utf-8"))
auto = c["apps"]["tls"]["certificates"]["automate"]
want = {"lk.example.com", "turn.example.com", "api.example.com", "lk.203-0-113-10.sslip.io", "lk.old.example.org",
        "turn.203-0-113-10.sslip.io", "api.203-0-113-10.sslip.io"}
assert set(auto) == want, auto
routes = c["apps"]["layer4"]["servers"]["main"]["routes"][1]["handle"][0]["routes"]
sni = {r["handle"][-1]["upstreams"][0]["dial"][0]: set(r["match"][0]["tls"]["sni"]) for r in routes}
assert sni["127.0.0.1:7880"] == {"lk.example.com", "lk.203-0-113-10.sslip.io", "lk.old.example.org"}, sni
assert sni["127.0.0.1:5349"] == {"turn.example.com", "turn.203-0-113-10.sslip.io"}, sni
assert sni["127.0.0.1:8080"] == {"api.example.com", "api.203-0-113-10.sslip.io"}, sni
EOF
echo "ok: алиасы в сертификатах и SNI"

# 3. мусор в алиасах — ошибка render.sh
mkdir "$T/bad"; base_env > "$T/bad/.env"
echo 'LK_DOMAIN_ALIASES="lk.x.com\"},{evil"' >> "$T/bad/.env"
cp "$HERE/../render.sh" "$T/bad/render.sh"
if (cd "$T/bad" && sh ./render.sh >/dev/null 2>&1); then fail "мусор в алиасах принят"; fi
echo "ok: мусор в алиасах отвергнут"
echo "PASS"
