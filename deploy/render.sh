#!/bin/sh
# Генерирует deploy/generated/{livekit.yaml,caddy.json} из deploy/.env.
# Запуск: sh deploy/render.sh   (вызывается и из deploy/up.sh)
set -eu
cd "$(dirname "$0")"
[ -f .env ] || { echo "нет deploy/.env — скопируйте .env.example и запустите scripts/gen-keys.sh" >&2; exit 1; }
set -a
# shellcheck disable=SC1091 # .env — файл установки, не исходник
. ./.env
set +a

: "${LK_DOMAIN:?}" "${TURN_DOMAIN:?}"
: "${LK_TCP_PORT:=7881}" "${LK_UDP_PORT:=7882}" "${TURN_UDP_PORT:=3478}"
: "${CADDY_LISTEN:=:443}" "${CADDY_SERVE_API:=0}" "${LK_MAX_PARTICIPANTS:=20}" "${ACME_EMAIL:=}"
: "${BACKEND_BIND:=127.0.0.1:8080}" "${ACME_CA:=}" "${CADDY_LOG_LEVEL:=INFO}"
TURN_TLS_INTERNAL=5349   # только loopback: сюда Caddy отдаёт расшифрованный TURN/TLS

# Имена для сертификата и SNI: основное + алиасы (*_DOMAIN_ALIASES через запятую — например, прежние имена sslip.io
# после перехода на свой домен, чтобы у старых клиентов ничего не сломалось). Вывод: "a", "b"
names() {
  out="\"$1\""
  old_ifs=$IFS; IFS=,
  for n in ${2:-}; do
    case "$n" in
      ''|*[!a-z0-9.-]*) echo "недопустимое имя в *_DOMAIN_ALIASES: '$n' (только a-z 0-9 . -, через запятую)" >&2; exit 1 ;;
    esac
    out="$out, \"$n\""
  done
  IFS=$old_ifs
  printf '%s' "$out"
}
LK_SNI=$(names "$LK_DOMAIN" "${LK_DOMAIN_ALIASES:-}")
TURN_SNI=$(names "$TURN_DOMAIN" "${TURN_DOMAIN_ALIASES:-}")

[ -n "${LIVEKIT_API_KEY:-}" ] && [ -n "${LIVEKIT_API_SECRET:-}" ] || {
  echo "LIVEKIT_API_KEY/SECRET пусты — запустите scripts/gen-keys.sh" >&2; exit 1; }

mkdir -p generated
umask 077

# ── LiveKit ──
if [ -n "${PUBLIC_IP:-}" ]; then IPCONF="  use_external_ip: false
  node_ip: ${PUBLIC_IP}"; else IPCONF="  use_external_ip: true"; fi
# Ограничить медиа сетевыми интерфейсами (через запятую, напр. "eth0"). Нужно, если на хосте есть туннели (WireGuard и т.п.):
# иначе LiveKit слушает UDP на всех адресах и может ответить клиенту с адреса туннеля — провайдер такие пакеты выбрасывает.
if [ -n "${LK_RTC_INTERFACES:-}" ]; then IPCONF="${IPCONF}
  interfaces:
    includes: [${LK_RTC_INTERFACES}]"; fi

cat > generated/livekit.yaml <<EOF
# СГЕНЕРИРОВАНО deploy/render.sh — не править руками
port: 7880
# Сигнальный порт — только для Caddy на этом же хосте (127.0.0.1:7880); наружу его не выставляем
bind_addresses: ["127.0.0.1"]
rtc:
  tcp_port: ${LK_TCP_PORT}
  udp_port: ${LK_UDP_PORT}
${IPCONF}
  # Как часто SFU передаёт стримеру запрос ключевого кадра (PLI) от зрителя. По умолчанию 1 с для «высокого» слоя:
  # если первый ключевой кадр после потери тоже побился, зритель ждёт ещё секунду. Ключевые кадры Murmur маленькие
  # (~60 КБ на 1080p) — можно чаще.
  pli_throttle:
    low_quality: 500ms
    mid_quality: 500ms
    high_quality: 500ms
turn:
  enabled: true
  domain: ${TURN_DOMAIN}
  udp_port: ${TURN_UDP_PORT}
  tls_port: ${TURN_TLS_INTERNAL}
  external_tls: true          # TLS снимает Caddy
  proxy_protocol: true        # Caddy передаёт настоящий IP клиента
  proxy_protocol_trusted_cidrs: ["127.0.0.0/8", "::1/128"]
room:
  empty_timeout: 300
  departure_timeout: 20
  max_participants: ${LK_MAX_PARTICIPANTS}
logging:
  level: info
keys:
  ${LIVEKIT_API_KEY}: ${LIVEKIT_API_SECRET}
EOF
# Вебхуки (кто вошёл/вышел) → бэкенд; подпись — тем же ключом API.
if [ -n "${LK_WEBHOOK_URL:-}" ]; then cat >> generated/livekit.yaml <<EOF
webhook:
  api_key: ${LIVEKIT_API_KEY}
  urls:
    - ${LK_WEBHOOK_URL}
EOF
fi

# ── Caddy (layer4): TLS по SNI ──
# Если перед Caddy стоит SNI-прокси с PROXY protocol (nginx stream) — заголовок разбирается (только с loopback).
# Без заголовка соединения тоже принимаются (standalone).
API_ROUTE=""; API_NAME=""
if [ "${CADDY_SERVE_API}" = "1" ]; then
  : "${API_DOMAIN:?}"
  API_SNI=$(names "$API_DOMAIN" "${API_DOMAIN_ALIASES:-}")
  API_NAME=", ${API_SNI}"
  API_ROUTE=",
          { \"match\": [{ \"tls\": { \"sni\": [${API_SNI}] } }],
            \"handle\": [ { \"handler\": \"tls\", \"connection_policies\": [{ \"alpn\": [\"http/1.1\"] }] },
                        { \"handler\": \"proxy\", \"proxy_protocol\": \"v2\", \"upstreams\": [{ \"dial\": [\"${BACKEND_BIND}\"] }] } ] }"
fi

SNI_ROUTES="[
          { \"match\": [{ \"tls\": { \"sni\": [${TURN_SNI}] } }],
            \"handle\": [ { \"handler\": \"tls\" },
                        { \"handler\": \"proxy\", \"proxy_protocol\": \"v2\", \"upstreams\": [{ \"dial\": [\"127.0.0.1:${TURN_TLS_INTERNAL}\"] }] } ] },
          { \"match\": [{ \"tls\": { \"sni\": [${LK_SNI}] } }],
            \"handle\": [ { \"handler\": \"tls\", \"connection_policies\": [{ \"alpn\": [\"http/1.1\"] }] },
                        { \"handler\": \"proxy\", \"upstreams\": [{ \"dial\": [\"127.0.0.1:7880\"] }] } ] }${API_ROUTE}
        ]"

EMAIL_JSON=""; [ -n "${ACME_EMAIL}" ] && EMAIL_JSON="\"email\": \"${ACME_EMAIL}\","
[ -n "${ACME_CA}" ] && EMAIL_JSON="${EMAIL_JSON} \"ca\": \"${ACME_CA}\","

cat > generated/caddy.json <<EOF
{
  "admin": { "disabled": true },
  "logging": { "logs": { "default": { "level": "${CADDY_LOG_LEVEL}" } } },
  "storage": { "module": "file_system", "root": "/data" },
  "apps": {
    "tls": {
      "certificates": { "automate": [${LK_SNI}, ${TURN_SNI}${API_NAME}] },
      "automation": { "policies": [ { "issuers": [ { "module": "acme", ${EMAIL_JSON}
        "challenges": { "http": { "disabled": true } } } ] } ] }
    },
    "layer4": {
      "servers": {
        "main": {
          "listen": ["${CADDY_LISTEN}"],
          "routes": [
            { "match": [{ "proxy_protocol": {} }],
              "handle": [ { "handler": "proxy_protocol", "allow": ["127.0.0.1/32", "::1/128"] },
                          { "handler": "subroute", "routes": ${SNI_ROUTES} } ] },
            { "handle": [ { "handler": "subroute", "routes": ${SNI_ROUTES} } ] }
          ]
        }
      }
    }
  }
}
EOF
echo "ok: generated/livekit.yaml, generated/caddy.json"
