#!/usr/bin/env bash
# Выпуск клиента Murmur (бывш. Amadeus): установщик + файлы автообновления → папка выпусков бэкенда.
#
#   scripts/release.sh "Что нового (одна-две строки)"            — собрать и выложить на домашний сервер
#   NO_UPLOAD=1 scripts/release.sh "…"                             — только собрать в dist/<версия>/
#   NO_GITHUB=1 scripts/release.sh "…"                             — без релиза на GitHub (scripts/gh-release.sh)
#   MIN_SERVER=0.3.13 scripts/release.sh "…"                       — клиенту нужен сервер не старее 0.3.13: чужие серверы
#                                                                    (обновления с официального) отдадут выпуск, только если
#                                                                    сами не старее. Без MIN_SERVER — любым серверам.
#
# Версия — из [workspace.package] в Cargo.toml (поднять перед выпуском: клиенты обновляются, только если она больше).
# На сервере: файлы → $REMOTE_DIR (том бэкенда, /data/releases в контейнере), latest.json кладётся последним —
# пока его нет, клиенты новую версию не видят. Запускать из Git Bash (ssh home, sha256sum, gzip).
set -euo pipefail
cd "$(dirname "$0")/.."

NOTES="${1:?Укажите, что нового: scripts/release.sh \"…\"}"
REMOTE_HOST="${REMOTE_HOST:-home}"
REMOTE_DIR="${REMOTE_DIR:-/srv/data/amadeus/backend/releases}"

VERSION=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "не понял версию: '$VERSION'"; exit 1; }
MIN_SERVER="${MIN_SERVER:-}"
[[ -z "$MIN_SERVER" || "$MIN_SERVER" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "MIN_SERVER должен быть x.y.z: '$MIN_SERVER'"; exit 1; }
echo "== Murmur $VERSION${MIN_SERVER:+ (сервер не старее $MIN_SERVER)}"

# старые установщики этой же версии (в т.ч. amadeus_…) убрать, чтобы ниже взять ровно свежесобранный
rm -f target/release/*_"${VERSION}"_x64-setup.exe
cargo packager -p client --release

OUT="dist/$VERSION"
mkdir -p "$OUT"
EXE="murmur-$VERSION.exe"
SETUP="murmur-setup-$VERSION.exe"
cp target/release/murmur.exe "$OUT/$EXE"
# cargo-packager называет установщик по productName (раньше amadeus_…, теперь murmur_…/Murmur_…)
mapfile -t BUILT_ALL < <(compgen -G "target/release/*_${VERSION}_x64-setup.exe" || true)
[[ ${#BUILT_ALL[@]} -eq 1 ]] || { echo "ожидался ровно один установщик ${VERSION}, найдено: ${BUILT_ALL[*]:-ничего}"; exit 1; }
BUILT="${BUILT_ALL[0]}"
cp "$BUILT" "$OUT/$SETUP"
gzip -kf9 "$OUT/$EXE"   # сервер отдаёт .gz клиентам с Accept-Encoding: gzip (~в 3 раза меньше)

SHA=$(sha256sum "$OUT/$EXE" | cut -d' ' -f1)
SIZE=$(stat -c %s "$OUT/$EXE")
MIN_SERVER="$MIN_SERVER" NOTES="$NOTES" VERSION="$VERSION" EXE="$EXE" SETUP="$SETUP" SHA="$SHA" SIZE="$SIZE" python -X utf8 -I -c '
import json, os
e = os.environ
info = {"version": e["VERSION"], "notes": e["NOTES"], "file": e["EXE"], "sha256": e["SHA"],
        "size": int(e["SIZE"]), "installer": e["SETUP"]}
if e["MIN_SERVER"]:
    info["min_server"] = e["MIN_SERVER"]
print(json.dumps(info, ensure_ascii=False, indent=2))
' > "$OUT/latest.json"
cat "$OUT/latest.json"
ls -la "$OUT"

if [[ -n "${NO_UPLOAD:-}" ]]; then
    echo "== NO_UPLOAD: файлы в $OUT, на сервер не выкладываю"
    exit 0
fi

TMP="/tmp/murmur-release-$VERSION"
echo "== выкладываю на $REMOTE_HOST:$REMOTE_DIR"
ssh "$REMOTE_HOST" "rm -rf $TMP && mkdir -p $TMP"
scp "$OUT/$EXE" "$OUT/$EXE.gz" "$OUT/$SETUP" "$OUT/latest.json" "$REMOTE_HOST:$TMP/"
# файлы — владельцу тома (uid 10001), latest.json — последним и атомарно (mv)
ssh "$REMOTE_HOST" "set -e
  sudo install -d -o 10001 -g 10001 -m 755 $REMOTE_DIR
  sudo install -o 10001 -g 10001 -m 644 $TMP/$EXE $TMP/$EXE.gz $TMP/$SETUP $REMOTE_DIR/
  sudo install -o 10001 -g 10001 -m 644 $TMP/latest.json $REMOTE_DIR/latest.json.tmp
  sudo mv $REMOTE_DIR/latest.json.tmp $REMOTE_DIR/latest.json
  rm -rf $TMP
  ls -la $REMOTE_DIR"
# Установщик — ещё и в релизы GitHub (коммит должен быть запушен). NO_GITHUB=1 — пропустить.
if [[ -z "${NO_GITHUB:-}" ]]; then
    bash scripts/gh-release.sh "$VERSION" "$NOTES" || echo "!! GitHub-релиз не сделан — повторить: scripts/gh-release.sh $VERSION \"…\""
fi
echo "== готово: клиенты увидят $VERSION при следующей проверке (до 6 ч) или по «Проверить обновления»"
