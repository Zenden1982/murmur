#!/usr/bin/env bash
# Прогон deploy/install.sh в чистом контейнере с systemd (Docker Desktop / Linux с Docker).
#   bash deploy/tests/e2e.sh [ubuntu:24.04|debian:12]
# Образы backend/caddy собираются из текущих исходников с тегом 0.0.0-test (как в .github/workflows/server-images.yml).
# Что НЕ проверяется: сертификаты Let's Encrypt, доступ из интернета, звонок — у контейнера нет публичного IP.
set -euo pipefail
export MSYS_NO_PATHCONV=1
BASE="${1:-ubuntu:24.04}"
cd "$(dirname "$0")/../.."
NAME="murmur-e2e-$(echo "$BASE" | tr ':.' '--')"
VOL="$NAME-docker"
VOL2="$NAME-containerd"
TMP=$(mktemp -d)
LK=$(grep -o 'livekit/livekit-server:v[0-9.]*' deploy/docker-compose.yml | head -n 1)
LK_TAG=${LK#*:}
FAILS=0
ok()  { echo "ok: $1"; }
bad() { echo "FAIL: $1" >&2; FAILS=$((FAILS + 1)); }
in_c() { docker exec "$NAME" bash -c "$1"; }
cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; docker volume rm "$VOL" "$VOL2" >/dev/null 2>&1 || true; rm -rf "$TMP"; }
trap cleanup EXIT
cleanup; mkdir -p "$TMP"
TMPD=$(cygpath -w "$TMP" 2>/dev/null || echo "$TMP")   # путь для docker.exe на Windows

echo "== образы"
# собрать из текущих исходников (слои кешируются) — иначе можно проверить старый бэкенд
docker build -q -f crates/backend/Dockerfile -t ghcr.io/zenden1982/murmur-backend:0.0.0-test . >/dev/null
docker build -q -t ghcr.io/zenden1982/murmur-caddy:0.0.0-test deploy/caddy >/dev/null
docker image inspect "$LK" >/dev/null 2>&1 || docker pull -q "$LK"
docker tag "$LK" "ghcr.io/zenden1982/murmur-livekit:$LK_TAG"
docker save -o "$TMPD/images.tar" ghcr.io/zenden1982/murmur-backend:0.0.0-test ghcr.io/zenden1982/murmur-caddy:0.0.0-test "ghcr.io/zenden1982/murmur-livekit:$LK_TAG"

echo "== чистый $BASE"
docker build -q --build-arg "BASE=$BASE" -t "$NAME" deploy/tests/e2e >/dev/null
docker run -d --name "$NAME" --privileged -v "$VOL:/var/lib/docker" -v "$VOL2:/var/lib/containerd" --tmpfs /run --tmpfs /run/lock "$NAME" >/dev/null
sleep 3
docker cp deploy "$NAME:/root/deploy"
docker cp "$TMPD/images.tar" "$NAME:/root/images.tar"
in_c 'printf "tester-password-1" > /root/pw && chmod 600 /root/pw && ufw --force enable >/dev/null'

echo "== Docker ставит сам скрипт (install_docker)"
in_c 'MURMUR_LIB=1 . /root/deploy/install.sh && install_docker' > "$TMP/docker.log" 2>&1 || { tail -20 "$TMP/docker.log"; exit 1; }
if in_c 'docker compose version' >/dev/null; then ok "Docker + compose поставлены"; else bad "Docker не поставился"; fi
in_c 'docker load -q -i /root/images.tar >/dev/null'

ENVS='MURMUR_TEST=1 MURMUR_NO_PULL=1 MURMUR_FILES_FROM=/root/deploy'
ARGS='--yes --ip 203.0.113.10 --admin tester --admin-password-file /root/pw --version 0.0.0-test'

echo "== установка"
in_c "$ENVS sh /root/deploy/install.sh $ARGS" > "$TMP/install1.log" 2>&1 || { cat "$TMP/install1.log"; bad "установка упала"; exit 1; }
cat "$TMP/install1.log"

running=$(in_c 'cd /opt/murmur && docker compose ps --status running --services | sort | tr "\n" " "')
if [ "$running" = "backend caddy livekit " ]; then ok "3 контейнера running"; else bad "running: '$running'"; fi
if grep -q 'https://api.203-0-113-10.sslip.io' "$TMP/install1.log"; then ok "адрес sslip.io в итоге"; else bad "нет адреса sslip.io"; fi
INV=$(sed -n 's/.*Инвайт для друзей[^:]*:  *\([A-Z0-9-]*\).*/\1/p' "$TMP/install1.log")
if [ ${#INV} -ge 8 ]; then ok "инвайт: $INV"; else bad "нет инвайта"; fi
SECRET=$(in_c 'sed -n "s/^LIVEKIT_API_SECRET=//p" /opt/murmur/.env')
if [ ${#SECRET} -ge 32 ]; then ok "секрет LiveKit сгенерирован"; else bad "секрета нет"; fi
if grep -qF "$SECRET" "$TMP/install1.log" || grep -qF tester-password-1 "$TMP/install1.log"; then bad "секрет или пароль в выводе"; else ok "секретов в выводе нет"; fi
if [ "$(in_c 'stat -c %a /opt/murmur/.env')" = 600 ]; then ok ".env 600"; else bad ".env не 600"; fi
rules=$(in_c 'ufw status' | grep -cE '^(443/tcp|7881/tcp|7882/udp|3478/udp) ' || true)
if [ "$rules" -ge 4 ]; then ok "ufw: 4 правила"; else bad "ufw: $rules правил"; fi

# запросы к бэкенду напрямую (PROXY v2 LOCAL, как health в install.sh)
docker exec -i "$NAME" sh -c 'cat > /root/req.sh' <<'EOF'
#!/bin/bash
# req.sh METHOD PATH [JSON]
exec 3<>/dev/tcp/127.0.0.1/8080
b="${3:-}"
printf '\r\n\r\n\0\r\nQUIT\n\x20\x00\x00\x00%s %s HTTP/1.0\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: %s\r\n\r\n%s' "$1" "$2" "${#b}" "$b" >&3
timeout 5 cat <&3
EOF
login() { in_c "bash /root/req.sh POST /auth/login '{\"name\":\"tester\",\"password\":\"tester-password-1\"}'" | head -n 1; }
if login | grep -q ' 200'; then ok "вход админа"; else bad "вход админа: $(login)"; fi
INV2=$(in_c 'murmur invite' 2>&1 || true)
if printf '%s' "$INV2" | grep -qE 'Инвайт .*: [A-Z0-9-]{8,}'; then ok "murmur invite"; else bad "murmur invite: $INV2"; fi

echo "== повторный запуск (обновление)"
KEYS1=$(in_c 'grep -E "^LIVEKIT_API_(KEY|SECRET)=" /opt/murmur/.env | sha256sum')
in_c "$ENVS sh /root/deploy/install.sh $ARGS" > "$TMP/install2.log" 2>&1 || { cat "$TMP/install2.log"; bad "повтор упал"; }
KEYS2=$(in_c 'grep -E "^LIVEKIT_API_(KEY|SECRET)=" /opt/murmur/.env | sha256sum')
if [ "$KEYS1" = "$KEYS2" ]; then ok "ключи не изменились"; else bad "ключи изменились"; fi
if in_c 'cd /opt/murmur && docker compose exec -T backend sh -c "ls /data/backup"' | grep -q 'app-pre-0.0.0-test'; then ok "копия базы перед обновлением"; else bad "нет копии базы"; fi
if grep -q 'Инвайт для друзей' "$TMP/install2.log"; then bad "админ/инвайт создан повторно"; else ok "админ повторно не создавался"; fi
if login | grep -q ' 200'; then ok "вход прежним паролем"; else bad "вход после обновления"; fi
running=$(in_c 'cd /opt/murmur && docker compose ps --status running --services | wc -l')
if [ "$running" = 3 ]; then ok "после обновления всё running"; else bad "running: $running"; fi

echo "== смена домена"
in_c "$ENVS sh /root/deploy/install.sh --yes --domain example.test --version 0.0.0-test" > "$TMP/install3.log" 2>&1 || { cat "$TMP/install3.log"; bad "смена домена упала"; }
if [ "$(in_c 'sed -n "s/^API_DOMAIN=//p" /opt/murmur/.env')" = api.example.test ]; then ok "API_DOMAIN=api.example.test"; else bad "API_DOMAIN не сменился"; fi
if in_c 'sed -n "s/^API_DOMAIN_ALIASES=//p" /opt/murmur/.env' | grep -qx 'api.203-0-113-10.sslip.io'; then ok "старое имя в алиасах"; else bad "алиаса нет"; fi
if in_c 'grep -q api.203-0-113-10.sslip.io /opt/murmur/generated/caddy.json && grep -q api.example.test /opt/murmur/generated/caddy.json'; then ok "caddy.json: оба имени"; else bad "caddy.json без обоих имён"; fi

echo
if [ "$FAILS" -eq 0 ]; then echo "PASS ($BASE)"; else echo "FAIL: $FAILS ($BASE)"; exit 1; fi
