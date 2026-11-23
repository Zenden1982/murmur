#!/bin/sh
# Murmur — свой сервер одной командой (Ubuntu 22.04/24.04, Debian 12; x86-64).
#
#   curl -fsSL https://raw.githubusercontent.com/Zenden1982/murmur/main/deploy/install.sh | sudo sh
#
# Повторный запуск (или «murmur update») — обновление: настройки, ключи и база сохраняются.
# Без вопросов: ... | sudo sh -s -- --yes --admin <ник> --admin-password-file <файл> [--domain <домен>] [--email <почта>]
# Команда после установки: murmur update | invite | backup | restore | logs | status
#
# Что делает: ставит Docker (если нет), спрашивает домен (Enter — бесплатные имена по IP через sslip.io),
# создаёт ключи, открывает порты в ufw, поднимает LiveKit + Caddy + бэкенд из готовых образов (ghcr.io),
# создаёт администратора и инвайт для друзей. Ставится в /opt/murmur.
set -eu

MURMUR_REPO="${MURMUR_REPO:-Zenden1982/murmur}"
MURMUR_DIR="${MURMUR_DIR:-/opt/murmur}"
MURMUR_TTY="${MURMUR_TTY:-/dev/tty}"
REGISTRY="ghcr.io/zenden1982"
DEFAULT_UPSTREAM="https://api.murmurchat.ru"

# ───────────────────────────── вывод
say()  { printf '%s\n' "$*"; }
step() { printf '\n== %s\n' "$*"; }
warn() { printf '!! %s\n' "$*" >&2; }
die()  { printf '\nОшибка: %s\n' "$*" >&2; exit 1; }

# ───────────────────────────── проверки значений
valid_ipv4() {
  case "${1:-}" in ''|*[!0-9.]*) return 1 ;; esac
  _ifs=$IFS; IFS=.
  # shellcheck disable=SC2086 # разбиение по точкам — намеренно
  set -- $1
  IFS=$_ifs
  [ $# -eq 4 ] || return 1
  for _o in "$@"; do
    case "$_o" in ''|*[!0-9]*) return 1 ;; esac
    [ ${#_o} -le 3 ] && [ "$_o" -le 255 ] || return 1
  done
}

valid_domain() {
  case "${1:-}" in
    ''|*[!a-z0-9.-]*|.*|-*|*.|*..*|*.-*|*-.*) return 1 ;;
    *.*) [ ${#1} -le 200 ] ;;
    *) return 1 ;;
  esac
}

valid_email() {
  case "${1:-}" in ''|*[!A-Za-z0-9._%+@-]*|@*|*@|*@*@*) return 1 ;; *@*) ;; *) return 1 ;; esac
  valid_domain "$(printf '%s' "${1#*@}" | tr '[:upper:]' '[:lower:]')"
}

valid_version() { printf '%s' "${1:-}" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+(-[a-z0-9]+)?'; }

# установка уже есть (а не обрывок .env после прерванного первого запуска)
is_installed() { [ -n "$(env_get "$1/.env" LIVEKIT_API_SECRET)" ] && [ -n "$(env_get "$1/.env" API_DOMAIN)" ]; }

sslip_base() { printf '%s.sslip.io' "$(printf '%s' "$1" | tr . -)"; }

# ───────────────────────────── .env
env_get() { [ -f "$1" ] && sed -n "s/^$2=//p" "$1" | tail -n 1; return 0; }

# env_set FILE KEY VALUE — заменить или дописать (значение — как есть, без интерпретации); права файла сохраняются
env_set() {
  _t=$(mktemp)
  K="$2" V="$3" awk 'BEGIN { k = ENVIRON["K"]; v = ENVIRON["V"] }
    index($0, k "=") == 1 { if (!done) print k "=" v; done = 1; next }
    { print }
    END { if (!done) print k "=" v }' "$1" > "$_t"
  cat "$_t" > "$1"
  rm -f "$_t"
}

alias_add() {
  _cur=$(env_get "$1" "$2")
  case ",$_cur," in *",$3,"*) return 0 ;; esac
  env_set "$1" "$2" "${_cur:+$_cur,}$3"
}

rand_str() { LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom | head -c "$1"; }

# ───────────────────────────── сеть
detect_ip() {
  for _u in https://api.ipify.org https://ifconfig.me/ip https://ipv4.icanhazip.com; do
    _ip=$(curl -4 -fsS --max-time 6 "$_u" 2>/dev/null | tr -d ' \r\n') || continue
    if valid_ipv4 "$_ip"; then printf '%s\n' "$_ip"; return 0; fi
  done
  return 1
}

dns_points_to() { getent ahostsv4 "$1" 2>/dev/null | awk '{ print $1 }' | grep -qx "$2"; }

# port_busy_by_other PROTO PORT — порт слушает кто-то кроме нашей установки
port_busy_by_other() {
  if [ "$1" = udp ]; then _l=$(ss -lunpH "sport = :$2" 2>/dev/null || true)
  else _l=$(ss -ltnpH "sport = :$2" 2>/dev/null || true); fi
  [ -n "$_l" ] || return 1
  # наша же установка (повторный запуск): caddy / livekit-server / проброс порта бэкенда
  if is_installed "$MURMUR_DIR"; then
    case "$_l" in *'"caddy"'*|*'"livekit-server"'*|*'"docker-proxy"'*) return 1 ;; esac
  fi
  return 0
}

# ───────────────────────────── вопросы (из терминала, даже при curl | sh)
have_tty() { [ -r "$MURMUR_TTY" ] && [ -w "$MURMUR_TTY" ] && { : <"$MURMUR_TTY"; } 2>/dev/null; }

ask() { # ask "Вопрос" [по умолчанию] → ответ
  if [ -n "${2:-}" ]; then printf '%s [%s]: ' "$1" "$2" >"$MURMUR_TTY"; else printf '%s: ' "$1" >"$MURMUR_TTY"; fi
  IFS= read -r _a <"$MURMUR_TTY" || _a=""
  printf '%s' "${_a:-${2:-}}"
}

ask_secret() {
  printf '%s: ' "$1" >"$MURMUR_TTY"
  stty -echo <"$MURMUR_TTY" 2>/dev/null || true
  IFS= read -r _a <"$MURMUR_TTY" || _a=""
  stty echo <"$MURMUR_TTY" 2>/dev/null || true
  printf '\n' >"$MURMUR_TTY"
  printf '%s' "$_a"
}

# ───────────────────────────── docker
dc() { docker compose --project-directory "$MURMUR_DIR" -f "$MURMUR_DIR/docker-compose.yml" "$@"; }

install_docker() {
  if command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then return 0; fi
  step "Ставлю Docker"
  # shellcheck disable=SC1091
  _id=$(. /etc/os-release 2>/dev/null && printf '%s' "${ID:-}")
  case "$_id" in
    ubuntu|debian)
      export DEBIAN_FRONTEND=noninteractive
      apt-get update -qq
      # пакеты дистрибутива (зеркала доступны и из РФ); в Debian 12 compose v2 нет — тогда официальный скрипт Docker
      apt-get install -y -qq docker.io docker-compose-v2 >/dev/null 2>&1 \
        || apt-get install -y -qq docker.io docker-compose-plugin >/dev/null 2>&1 \
        || { apt-get remove -y -qq docker.io >/dev/null 2>&1 || true
             curl -fsSL https://get.docker.com | sh; } ;;
    *)
      die "автоматически Docker ставится только на Ubuntu и Debian. Поставьте Docker с compose сами
(https://docs.docker.com/engine/install/) и запустите эту команду снова." ;;
  esac
  systemctl enable --now docker >/dev/null 2>&1 || service docker start >/dev/null 2>&1 || true
  docker compose version >/dev/null 2>&1 || die "Docker поставился без «docker compose» — поставьте docker-compose-plugin и повторите."
}

# health бэкенда напрямую (минуя Caddy): бэкенд ждёт заголовок PROXY v2 — шлём LOCAL-заголовок вручную
backend_health() {
  bash -c 'exec 3<>/dev/tcp/127.0.0.1/8080 || exit 1
    printf "\r\n\r\n\0\r\nQUIT\n\x20\x00\x00\x00GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n" >&3
    timeout 5 cat <&3' 2>/dev/null | grep -q '"version"'
}

backend_health_plain() { curl -fsS --max-time 5 http://127.0.0.1:8080/health 2>/dev/null | grep -q '"version"'; }

wait_backend() {
  _i=0
  while [ $_i -lt 60 ]; do
    if [ "$(env_get "$MURMUR_DIR/.env" CADDY_SERVE_API)" = 1 ]; then backend_health && return 0
    else backend_health_plain && return 0; fi
    _i=$((_i + 1)); sleep 2
  done
  return 1
}

# admin_state → yes | no | error (ошибка проверки ≠ «админа нет»: иначе при сбое создали бы второго)
admin_state() {
  if dc exec -T backend backend admin-exists >/dev/null 2>&1; then echo yes
  else
    case $? in 2) echo no ;; *) echo error ;; esac
  fi
}

# ───────────────────────────── файлы
fetch_files() { # fetch_files VERSION
  mkdir -p "$MURMUR_DIR"
  for _f in docker-compose.yml render.sh install.sh; do
    if [ -n "${MURMUR_FILES_FROM:-}" ]; then cp "$MURMUR_FILES_FROM/$_f" "$MURMUR_DIR/$_f.new"
    else curl -fsSL --max-time 30 "https://raw.githubusercontent.com/$MURMUR_REPO/v$1/deploy/$_f" -o "$MURMUR_DIR/$_f.new" \
      || die "не скачал deploy/$_f версии $1 с GitHub"; fi
    mv "$MURMUR_DIR/$_f.new" "$MURMUR_DIR/$_f"
  done
  mkdir -p "$MURMUR_DIR/caddy"   # контекст сборки из compose-файла (на сервере не собираем, но путь должен быть)
  chmod 755 "$MURMUR_DIR/render.sh" "$MURMUR_DIR/install.sh"
}

latest_version() {
  curl -fsSL --max-time 15 "https://api.github.com/repos/$MURMUR_REPO/releases/latest" 2>/dev/null \
    | sed -n 's/.*"tag_name": *"v\{0,1\}\([0-9][0-9.]*\)".*/\1/p' | head -n 1
}

livekit_tag() { grep -o 'livekit/livekit-server:v[0-9.]*' "$MURMUR_DIR/docker-compose.yml" | head -n 1 | cut -d: -f2; }

install_command() {
  cat > /usr/local/bin/murmur <<EOF
#!/bin/sh
# Команда сервера Murmur: murmur update | invite | backup | restore | logs | status
MURMUR_DIR="$MURMUR_DIR" exec sh "$MURMUR_DIR/install.sh" "\$@"
EOF
  chmod 755 /usr/local/bin/murmur
}

# ───────────────────────────── установка / обновление
usage() {
  cat <<'EOF'
Murmur — свой сервер.
  murmur update [--version X.Y.Z] [--domain ДОМЕН]   обновить (или сменить домен; старый адрес продолжит работать)
  murmur invite [--uses N] [--hours H]               новый инвайт для друзей (по умолчанию 10 раз, 7 дней)
  murmur backup                                      копия базы и картинок (архив, хранится последних 7)
  murmur restore ФАЙЛ.tar.gz                         восстановить из такого архива
  murmur logs                                        журналы (Ctrl+C — выход)
  murmur status                                      состояние и проверка обновлений
EOF
}

cmd_install() {
  YES=""; DOMAIN=""; EMAIL=""; ADMIN=""; PWFILE=""; IP=""; VERSION="${MURMUR_VERSION:-}"
  while [ $# -gt 0 ]; do
    case "$1" in
      --yes|-y) YES=1 ;;
      --domain) DOMAIN="${2:?--domain: нужно значение}"; shift ;;
      --email) EMAIL="${2:?}"; shift ;;
      --admin) ADMIN="${2:?}"; shift ;;
      --admin-password-file) PWFILE="${2:?}"; shift ;;
      --ip) IP="${2:?}"; shift ;;
      --version) VERSION="${2:?}"; shift ;;
      -h|--help) usage; exit 0 ;;
      *) die "непонятный параметр: $1 (murmur --help)" ;;
    esac
    shift
  done
  ENV="$MURMUR_DIR/.env"
  UPDATE=""; is_installed "$MURMUR_DIR" && UPDATE=1

  # 1. проверки — до любых изменений системы
  # вопросы задаются только при первой установке (при обновлении — лишь если админа нет)
  if [ -z "$YES" ] && [ -z "$UPDATE" ] && ! have_tty; then
    die "нет терминала для вопросов. Запустите без вопросов:
  ... | sudo sh -s -- --yes --admin <ник> --admin-password-file <файл с паролем> [--domain <домен>]"
  fi
  [ "$(id -u)" = 0 ] || die "нужны права root. Запустите так: curl -fsSL https://raw.githubusercontent.com/$MURMUR_REPO/main/deploy/install.sh | sudo sh"
  [ "$(uname -m)" = x86_64 ] || die "поддерживается только x86-64 (у вас $(uname -m))"
  if [ -n "$VERSION" ]; then valid_version "$VERSION" || die "версия должна быть вида 0.3.13: '$VERSION'"; fi
  if [ -n "$EMAIL" ]; then
    EMAIL=$(printf '%s' "$EMAIL" | tr -d ' \t\r')
    valid_email "$EMAIL" || die "не похоже на почту: '$EMAIL'"
  fi
  command -v ss >/dev/null 2>&1 || warn "нет команды ss (iproute2) — занятость портов не проверяю"
  # наружу: 443, 7881, 7882/udp, 3478/udp; на этом сервере: 8080 (бэкенд), 7880 и 5349 (LiveKit)
  for _p in tcp:443 tcp:7881 udp:7882 udp:3478 tcp:8080 tcp:7880 tcp:5349; do
    if port_busy_by_other "${_p%%:*}" "${_p#*:}"; then
      die "порт ${_p#*:}/${_p%%:*} уже занят другой программой:
$(ss -lntupH "sport = :${_p#*:}" 2>/dev/null | head -n 3)
Murmur нужны свободные порты 443, 7881, 7882/udp, 3478/udp и локальные 8080, 7880, 5349. Если 443 занят вашим nginx/HAProxy —
установите вручную в режиме «за SNI-прокси»: https://github.com/$MURMUR_REPO/blob/main/deploy/README.md"
    fi
  done
  if [ -z "$UPDATE" ] && [ -n "$YES" ] && { [ -z "$ADMIN" ] || [ -z "$PWFILE" ]; }; then
    die "с --yes нужны --admin <ник> и --admin-password-file <файл>"
  fi
  _mem=$(awk '/^MemTotal:/ { print int($2 / 1024) }' /proc/meminfo 2>/dev/null || echo 0)
  [ "$_mem" -ge 900 ] || warn "памяти ${_mem} МБ — рекомендуется от 1 ГБ"

  # 2. Docker
  install_docker
  command -v curl >/dev/null 2>&1 || die "нужен curl"

  # 3. версия и файлы
  [ -n "$VERSION" ] || VERSION=$(latest_version)
  valid_version "$VERSION" || die "не узнал последнюю версию с GitHub — укажите явно: --version X.Y.Z"
  step "Murmur $VERSION → $MURMUR_DIR"
  fetch_files "$VERSION"

  # 4. адрес и имена
  OLD_LK=""; OLD_TURN=""; OLD_API=""
  if [ -n "$UPDATE" ]; then
    OLD_LK=$(env_get "$ENV" LK_DOMAIN); OLD_TURN=$(env_get "$ENV" TURN_DOMAIN); OLD_API=$(env_get "$ENV" API_DOMAIN)
    [ -n "$IP" ] || IP=$(env_get "$ENV" PUBLIC_IP)
  fi
  if [ -z "$IP" ]; then
    IP=$(detect_ip || true)
    if [ -z "$YES" ]; then IP=$(ask "Публичный IPv4 этого сервера" "$IP"); fi
  fi
  valid_ipv4 "$IP" || die "не понял публичный IPv4 сервера: '$IP' (укажите --ip)"

  if [ -z "$UPDATE" ] && [ -z "$DOMAIN" ] && [ -z "$YES" ]; then
    say ""
    say "Свой домен (например chat.example.com) — будут нужны 3 записи DNS."
    say "Нет домена — нажмите Enter: имена будут вида api.$(sslip_base "$IP") (бесплатно, без настройки)."
    DOMAIN=$(ask "Домен")
  fi
  if [ -n "$DOMAIN" ]; then
    DOMAIN=$(printf '%s' "$DOMAIN" | tr '[:upper:]' '[:lower:]')
    valid_domain "$DOMAIN" || die "не похоже на домен: '$DOMAIN'"
    BASE="$DOMAIN"
  elif [ -n "$UPDATE" ]; then
    BASE="${OLD_API#api.}"
  else
    BASE=$(sslip_base "$IP")
  fi
  LK="lk.$BASE"; TURN="turn.$BASE"; API="api.$BASE"

  if [ -n "$DOMAIN" ] && [ -z "${MURMUR_TEST:-}" ]; then
    step "DNS"
    say "Создайте у регистратора домена три A-записи (без прокси CDN):"
    say "  $LK    →  $IP"
    say "  $TURN  →  $IP"
    say "  $API   →  $IP"
    _w=0
    until dns_points_to "$LK" "$IP" && dns_points_to "$TURN" "$IP" && dns_points_to "$API" "$IP"; do
      [ $_w -lt 120 ] || die "за 30 минут записи DNS так и не указали на $IP. Проверьте их и запустите команду снова."
      [ $_w -eq 0 ] && say "Жду, пока записи начнут указывать на $IP (проверка каждые 15 с, Ctrl+C — прервать)..."
      _w=$((_w + 1)); sleep 15
    done
    say "DNS в порядке."
  fi

  if [ -z "$UPDATE" ] && [ -z "$EMAIL" ] && [ -z "$YES" ]; then
    while :; do
      EMAIL=$(ask "Почта для Let's Encrypt (необязательно, Enter — пропустить)" | tr -d ' \t\r')
      [ -z "$EMAIL" ] || valid_email "$EMAIL" && break
      say "Не похоже на почту — ещё раз (или Enter, чтобы пропустить)."
    done
  fi

  # 5. .env — собирается во временном файле и подменяется целиком (обрыв посередине не оставит «полуустановку»)
  LK_TAG=$(livekit_tag); [ -n "$LK_TAG" ] || die "в docker-compose.yml нет версии LiveKit"
  umask 077
  REAL_ENV="$ENV"; ENV="$REAL_ENV.new"
  if [ -n "$UPDATE" ]; then cp "$REAL_ENV" "$ENV"; else
    : > "$ENV"
    env_set "$ENV" COMPOSE_PROFILES media,caddy,backend
    env_set "$ENV" CADDY_SERVE_API 1
    env_set "$ENV" CADDY_LISTEN :443
    env_set "$ENV" BACKEND_BIND 127.0.0.1:8080
    env_set "$ENV" TRUSTED_PROXY_HOPS 0
    env_set "$ENV" LK_TCP_PORT 7881
    env_set "$ENV" LK_UDP_PORT 7882
    env_set "$ENV" TURN_UDP_PORT 3478
    env_set "$ENV" LK_MAX_PARTICIPANTS 20
    # бэкенд → API LiveKit напрямую (не через сертификаты Caddy)
    env_set "$ENV" LIVEKIT_API_URL http://host.docker.internal:7880
    env_set "$ENV" UPDATES_UPSTREAM "$DEFAULT_UPSTREAM"
    env_set "$ENV" ACME_EMAIL "$EMAIL"
    env_set "$ENV" LIVEKIT_API_KEY "API$(rand_str 12)"
    env_set "$ENV" LIVEKIT_API_SECRET "$(rand_str 48)"
    [ -z "${MURMUR_TEST:-}" ] || env_set "$ENV" ACME_CA https://acme-staging-v02.api.letsencrypt.org/directory
  fi
  chmod 600 "$ENV"
  env_set "$ENV" MURMUR_VERSION "$VERSION"
  env_set "$ENV" BACKEND_IMAGE "$REGISTRY/murmur-backend:$VERSION"
  env_set "$ENV" CADDY_IMAGE "$REGISTRY/murmur-caddy:$VERSION"
  env_set "$ENV" LIVEKIT_IMAGE "$REGISTRY/murmur-livekit:$LK_TAG"
  env_set "$ENV" PUBLIC_IP "$IP"
  if [ -n "$OLD_API" ] && [ "$OLD_API" != "$API" ]; then
    alias_add "$ENV" LK_DOMAIN_ALIASES "$OLD_LK"
    alias_add "$ENV" TURN_DOMAIN_ALIASES "$OLD_TURN"
    alias_add "$ENV" API_DOMAIN_ALIASES "$OLD_API"
    say "Прежние имена ($OLD_API и др.) продолжат работать — у друзей ничего не сломается."
  fi
  env_set "$ENV" LK_DOMAIN "$LK"
  env_set "$ENV" TURN_DOMAIN "$TURN"
  env_set "$ENV" API_DOMAIN "$API"
  env_set "$ENV" LIVEKIT_PUBLIC_URL "wss://$LK"
  # вебхук «вошёл/вышел» — через Caddy: бэкенд в этом режиме принимает только соединения с заголовком PROXY
  env_set "$ENV" LK_WEBHOOK_URL "https://$API/livekit/webhook"
  chmod 600 "$ENV"
  mv "$ENV" "$REAL_ENV"
  ENV="$REAL_ENV"

  # 6. firewall
  if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q "Status: active"; then
    step "Открываю порты в ufw"
    for _p in 443/tcp 7881/tcp 7882/udp 3478/udp; do ufw allow "$_p" >/dev/null; done
  fi

  # 7. запуск
  if [ -n "$UPDATE" ] && dc ps --status running --services 2>/dev/null | grep -qx backend; then
    step "Копия базы перед обновлением"
    _b="/data/backup/app-pre-$VERSION-$(date +%Y%m%d-%H%M%S).db"
    dc exec -T backend backend backup "$_b" >/dev/null || die "не удалось сделать копию базы — обновление остановлено"
    say "Копия: $_b (в томе данных бэкенда)"
  fi
  step "Запускаю"
  if [ -z "${MURMUR_NO_PULL:-}" ]; then
    _pull=$(dc pull -q 2>&1) || {
      printf '%s\n' "$_pull" >&2
      case "$_pull" in
        *"not found"*|*"manifest unknown"*) die "образов версии $VERSION на ghcr.io ещё нет — их собирает GitHub Actions после выпуска (6–10 минут). Повторите позже." ;;
        *) die "не скачал образы с ghcr.io (нет доступа к интернету?)" ;;
      esac
    }
  fi
  sh "$MURMUR_DIR/render.sh" >/dev/null
  dc up -d --no-build --remove-orphans >/dev/null 2>&1 || { dc up -d --no-build --remove-orphans; die "docker compose up не удался"; }
  wait_backend || die "бэкенд не ответил за 2 минуты. Журнал: murmur logs"

  # 8. администратор и инвайт
  INVITE=""
  _adm=$(admin_state)
  [ "$_adm" != error ] || die "не смог проверить, есть ли администратор (бэкенд не отвечает?). Журнал: murmur logs backend"
  if [ "$_adm" = no ]; then
    step "Администратор"
    if [ -n "$YES" ] && { [ -z "$ADMIN" ] || [ -z "$PWFILE" ]; }; then
      die "администратора ещё нет — с --yes нужны --admin <ник> и --admin-password-file <файл>"
    fi
    if [ -z "$YES" ] && ! have_tty; then
      die "администратора ещё нет, а спросить ник и пароль негде (нет терминала). Запустите с --admin <ник> --admin-password-file <файл>"
    fi
    if [ -z "$ADMIN" ]; then ADMIN=$(ask "Ник администратора (2–32 символа)"); fi
    if [ -n "$PWFILE" ]; then ADMIN_PASSWORD=$(cat "$PWFILE")
    else
      while :; do
        ADMIN_PASSWORD=$(ask_secret "Пароль (от 8 символов, не показывается)")
        _again=$(ask_secret "Ещё раз")
        [ "$ADMIN_PASSWORD" = "$_again" ] && [ ${#ADMIN_PASSWORD} -ge 8 ] && break
        say "Пароли не совпали или короче 8 символов — ещё раз."
      done
    fi
    export ADMIN_PASSWORD
    dc exec -T -e ADMIN_PASSWORD backend backend create-admin --name "$ADMIN" >/dev/null \
      || die "не создал администратора (ник: 2–32 символа, буквы/цифры; пароль: от 8 символов)"
    unset ADMIN_PASSWORD
    INVITE=$(dc exec -T backend backend create-invite --uses 10 --hours 168 | tr -d '\r')
  fi

  install_command

  # 9. проверка снаружи
  HTTPS_OK=""
  if [ -z "${MURMUR_TEST:-}" ]; then
    step "Жду сертификаты (до 3 минут)"
    _i=0
    while [ $_i -lt 36 ]; do
      if curl -fsS --max-time 5 "https://$API/health" >/dev/null 2>&1; then HTTPS_OK=1; break; fi
      _i=$((_i + 1)); sleep 5
    done
  fi

  # 10. итог
  say ""
  say "════════════════════════════════════════════════════════"
  if [ -n "$UPDATE" ]; then say " Murmur обновлён до $VERSION"; else say " Murmur $VERSION установлен"; fi
  say "════════════════════════════════════════════════════════"
  say " Адрес сервера для клиента:  https://$API"
  say "   (в Murmur на экране входа → «Сервер» → вставить этот адрес)"
  [ -z "$ADMIN" ] || say " Администратор:  $ADMIN"
  if [ -n "$INVITE" ]; then
    say " Инвайт для друзей (10 человек, 7 дней):  $INVITE"
  fi
  say " Новый инвайт:  murmur invite      Обновить:  murmur update      Состояние:  murmur status"
  say " Клиент для Windows: https://github.com/$MURMUR_REPO/releases/latest"
  if [ -z "${MURMUR_TEST:-}" ] && [ -z "$HTTPS_OK" ]; then
    warn "https://$API пока не отвечает. Частые причины: закрыты порты в панели хостера
   (нужны 443/tcp, 7881/tcp, 7882/udp, 3478/udp) или DNS ещё не обновился. Журнал: murmur logs caddy"
  fi
}

cmd_invite() {
  _uses=10; _hours=168
  while [ $# -gt 0 ]; do
    case "$1" in
      --uses) _uses="${2:?}"; shift ;;
      --hours) _hours="${2:?}"; shift ;;
      *) die "непонятный параметр: $1" ;;
    esac
    shift
  done
  _code=$(dc exec -T backend backend create-invite --uses "$_uses" --hours "$_hours" | tr -d '\r') || die "не создал инвайт"
  say "Инвайт ($_uses раз, $_hours ч): $_code"
  say "Адрес сервера: https://$(env_get "$MURMUR_DIR/.env" API_DOMAIN)"
}

# Сколько архивов murmur backup хранить (и в томе, и на хосте).
BACKUP_KEEP=7

cmd_backup() {
  _ts=$(date +%Y%m%d-%H%M%S)
  _name="murmur-$_ts.tar.gz"
  _keep="$BACKUP_KEEP"
  # Архив: app.db (согласованный снимок) + uploads/ (картинки чата, аватары). Делается внутри контейнера бэкенда (там tar, sh).
  dc exec -T backend sh -c "set -e
    mkdir -p /data/backup /data/uploads
    backend backup /data/backup/.snap.db >/dev/null
    tar czf /data/backup/$_name --transform 's|^\\.snap\\.db|app.db|' -C /data/backup .snap.db -C /data uploads
    rm -f /data/backup/.snap.db
    ls -1t /data/backup/murmur-*.tar.gz | tail -n +$((_keep + 1)) | xargs -r rm -f" || die "копия не удалась"
  # Копия на хост — вне тома данных (том может пропасть вместе с контейнером).
  _host="$MURMUR_DIR/backups"
  mkdir -p "$_host"
  dc cp "backend:/data/backup/$_name" "$_host/$_name" >/dev/null || die "не смог скопировать архив из контейнера"
  # shellcheck disable=SC2012 # имена наши, без пробелов
  ls -1t "$_host"/murmur-*.tar.gz 2>/dev/null | tail -n +$((_keep + 1)) | xargs -r rm -f
  say "Копия: $_host/$_name (база + картинки; хранится последних $_keep, копия также в томе: /data/backup/$_name)"
  say "Восстановить: murmur restore $_host/$_name"
  say "Положите копию и за пределы этого сервера (scp/rsync) — иначе при потере диска она пропадёт вместе с ним."
}

cmd_restore() {
  _file="${1:-}"; _yes="${2:-}"
  [ -n "$_file" ] || die "укажите архив: murmur restore /путь/murmur-….tar.gz"
  [ -f "$_file" ] || die "нет файла: $_file"
  tar tzf "$_file" 2>/dev/null | grep -qx 'app.db' || die "это не архив murmur backup (нет app.db)"
  _abs=$(cd "$(dirname "$_file")" && pwd)/$(basename "$_file")
  if [ "$_yes" != "--yes" ]; then
    warn "Текущие база и картинки будут заменены содержимым архива (старые останутся: app.db.before-restore, uploads.before-restore)."
    [ "$(ask 'Восстановить? Введите yes' no)" = yes ] || die "отменено"
  fi
  step "Останавливаю бэкенд"
  dc stop backend >/dev/null
  step "Восстанавливаю из $_abs"
  dc run --rm --no-deps -T -v "$_abs:/restore.tar.gz:ro" --entrypoint sh backend -c '
    set -e
    cd /data
    rm -rf restore-tmp && mkdir restore-tmp
    tar xzf /restore.tar.gz -C restore-tmp
    test -f restore-tmp/app.db
    rm -f app.db.before-restore && { [ ! -f app.db ] || mv app.db app.db.before-restore; }
    rm -f app.db-wal app.db-shm
    mv restore-tmp/app.db app.db
    if [ -d restore-tmp/uploads ]; then
      rm -rf uploads.before-restore
      [ ! -d uploads ] || mv uploads uploads.before-restore
      mv restore-tmp/uploads uploads
    fi
    rm -rf restore-tmp' || die "восстановление не удалось (бэкенд остановлен: murmur update — запустит заново)"
  step "Запускаю"
  dc up -d backend >/dev/null 2>&1 || die "не запустил бэкенд"
  wait_backend || die "бэкенд не ответил за 2 минуты. Журнал: murmur logs"
  say "Готово: данные восстановлены."
}

cmd_status() {
  _env="$MURMUR_DIR/.env"
  _v=$(env_get "$_env" MURMUR_VERSION)
  say "Murmur $_v, адрес: https://$(env_get "$_env" API_DOMAIN)"
  dc ps
  _up=$(env_get "$_env" UPDATES_UPSTREAM)
  if [ -n "$_up" ]; then
    _min=$(curl -fsS --max-time 8 "$_up/version" 2>/dev/null | sed -n 's/.*"min_server": *"\([0-9.]*\)".*/\1/p')
    if [ -n "$_min" ] && [ "$(printf '%s\n%s\n' "$_min" "$_v" | sort -V | head -n 1)" != "$_min" ]; then
      warn "новый клиент Murmur требует сервер $_min или новее — обновите: murmur update"
    fi
  fi
  _latest=$(latest_version || true)
  if [ -n "$_latest" ] && [ "$_latest" != "$_v" ]; then say "Есть новая версия сервера: $_latest — murmur update"; fi
}

main() {
  if [ $# -gt 0 ]; then
    case "$1" in
      update|install) shift; cmd_install "$@"; return ;;
      invite) shift; cmd_invite "$@"; return ;;
      backup) shift; cmd_backup; return ;;
      restore) shift; cmd_restore "$@"; return ;;
      logs) shift; dc logs -f --tail 100 "$@"; return ;;
      status) shift; cmd_status; return ;;
      help|-h|--help) usage; return ;;
    esac
  fi
  cmd_install "$@"
}

[ -n "${MURMUR_LIB:-}" ] || main "$@"
