#!/usr/bin/env bash
# Тесты функций deploy/install.sh (без Docker и root): MURMUR_LIB=1 — скрипт только определяет функции.
#   bash deploy/tests/install_test.sh
set -u
cd "$(dirname "$0")/.."
INSTALL="$(pwd)/install.sh"
PASS=0; FAILS=0
ok()   { PASS=$((PASS+1)); echo "ok: $1"; }
bad()  { FAILS=$((FAILS+1)); echo "FAIL: $1" >&2; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

# каждый тест — в своём подшелле, с чистыми заглушками
lib() { MURMUR_LIB=1 . "$INSTALL"; }

# ── sslip / IP / домены
( lib
  check "sslip_base" '[ "$(sslip_base 85.12.34.56)" = 85-12-34-56.sslip.io ]'
  check "valid_ipv4 принимает" 'valid_ipv4 85.12.34.56 && valid_ipv4 1.1.1.1'
  for b in 256.1.1.1 "<html>" ::1 "" 1.2.3 1..2.3 1.2.3.4.5 " 1.2.3.4" 2001:db8::1; do
    check "valid_ipv4 отвергает '$b'" '! valid_ipv4 "$b"'
  done
  check "valid_domain" 'valid_domain chat.example.com && ! valid_domain "Example" && ! valid_domain localhost && ! valid_domain "a..b" && ! valid_domain "-a.b" && ! valid_domain "a.b." && ! valid_domain "a b.c"'
  exit $FAILS ) || FAILS=$((FAILS+1))

# ── detect_ip_skips_garbage
( lib
  curl() { case "$*" in
      *ipify*) echo "<html>blocked</html>";;
      *ifconfig.me*) echo "2001:db8::1";;
      *icanhazip*) printf '85.12.34.56\n';;
      *) return 7;; esac; }
  check "detect_ip_skips_garbage" '[ "$(detect_ip)" = 85.12.34.56 ]'
  curl() { echo "nope"; }
  check "detect_ip: все мимо — код ≠ 0" '! detect_ip >/dev/null'
  exit $FAILS ) || FAILS=$((FAILS+1))

# ── env_set / env_get / alias_add
( lib
  T=$(mktemp -d); F="$T/.env"
  printf 'A=1\nLIVEKIT_API_SECRET=old\nB=2\n' > "$F"
  env_set "$F" LIVEKIT_API_SECRET 'n/e=w&\\x'
  env_set "$F" NEW 'v a l'
  WANT='n/e=w&\\x'
  check "env_set заменяет" '[ "$(env_get "$F" LIVEKIT_API_SECRET)" = "$WANT" ]'
  check "env_set не трогает соседей" '[ "$(env_get "$F" A)" = 1 ] && [ "$(env_get "$F" B)" = 2 ]'
  check "env_set дописывает" '[ "$(env_get "$F" NEW)" = "v a l" ]'
  check "env_set без дублей" '[ "$(grep -c "^LIVEKIT_API_SECRET=" "$F")" = 1 ]'
  alias_add "$F" API_DOMAIN_ALIASES api.1-2-3-4.sslip.io
  alias_add "$F" API_DOMAIN_ALIASES api.old.example.com
  alias_add "$F" API_DOMAIN_ALIASES api.1-2-3-4.sslip.io
  check "alias_add без дублей" '[ "$(env_get "$F" API_DOMAIN_ALIASES)" = "api.1-2-3-4.sslip.io,api.old.example.com" ]'
  rm -rf "$T"
  exit $FAILS ) || FAILS=$((FAILS+1))

# ── dns_points_to
( lib
  getent() { echo "93.184.216.34   STREAM api.example.com"; }
  check "dns_points_to: другой IP — нет" '! dns_points_to api.example.com 85.12.34.56'
  check "dns_points_to: тот же — да" 'dns_points_to api.example.com 93.184.216.34'
  exit $FAILS ) || FAILS=$((FAILS+1))

# ── busy_443_stops_before_changes
( lib
  T=$(mktemp -d)
  id() { echo 0; }
  uname() { echo x86_64; }
  ss() { case "$*" in *443*) echo 'LISTEN 0 511 0.0.0.0:443 0.0.0.0:* users:(("nginx",pid=812,fd=6))';; esac; }
  apt-get() { touch "$T/apt-called"; }
  out=$(MURMUR_DIR="$T/murmur" MURMUR_TTY=/nonexistent main --yes --admin tester --admin-password-file /dev/null 2>&1); code=$?
  check "busy_443: код ≠ 0" '[ $code -ne 0 ]'
  check "busy_443: сообщение про 443" 'printf "%s" "$out" | grep -q 443'
  check "busy_443: ничего не изменено" '[ ! -e "$T/murmur" ] && [ ! -e "$T/apt-called" ]'
  rm -rf "$T"
  exit $FAILS ) || FAILS=$((FAILS+1))

# ── no_tty_without_yes_fails
( lib
  T=$(mktemp -d)
  out=$(MURMUR_DIR="$T/m" MURMUR_TTY=/nonexistent timeout 10 sh -c 'MURMUR_LIB=1 . "$1"; main' sh "$INSTALL" </dev/null 2>&1); code=$?
  check "no_tty: код ≠ 0 и не таймаут" '[ $code -ne 0 ] && [ $code -ne 124 ]'
  check "no_tty: подсказка про --yes" 'printf "%s" "$out" | grep -q -- --yes'
  check "no_tty: ничего не создано" '[ ! -e "$T/m" ]'
  rm -rf "$T"
  exit $FAILS ) || FAILS=$((FAILS+1))

echo "итого: ошибок $FAILS"
[ "$FAILS" -eq 0 ] && echo PASS
