# Murmur

**Native Rust voice, chat & screen-sharing messenger. Self-host in one command.**

Murmur is a Discord-style app for friends, gaming squads and small teams — voice channels, text chat, friends and DMs, group calls — built around one thing most chat apps do badly: **sharing your screen or a single window, with that app's sound, in real quality.**

<!-- СКРИНШОТЫ: главное окно, голосовой экран, стрим с управлением, настройки -->

## Why Murmur

- **Native client, no Electron.** Rust + wgpu, a single Windows .exe (installer ≈ 35 MB). In idle it uses ≈180 MB of RAM versus ≈1 GB for Discord (6 processes; measured on Windows 11, 2026-11-23). <!-- ЗАМЕР: в голосе и при просмотре стрима — дописать -->
- **Real screen sharing.** Share a whole screen or one window, up to 1080p/1440p at 60 fps and native resolution, hardware-encoded on your GPU (NVENC / AMF / QuickSync). The sound of *only that app* is shared too (per-process audio capture), not the whole system mix.
- **Remote control of a shared screen — on request.** A viewer can ask the streamer for control; the streamer allows or denies it, sees a "being controlled" indicator, and can take control back at any moment. Pointer-only mode is available too. Discord has nothing like it.
- **Built for bad networks.** TURN over TLS/443 for networks that block UDP, an economy layer for viewers on weak links, fast reconnect without leaving the channel. A built-in F3 overlay shows RTT, bitrate, fps, packet loss and the encoder in use.
- **Clean voice.** Local neural noise suppression (DeepFilterNet), echo cancellation, per-user volume, push-to-talk and global hotkeys that work from inside a game.
- **Everything you expect from a chat.** Text channels, roles and permissions, invites, friends, DMs with unread counters, group calls with camera (up to 8), reactions, emoji, stickers, GIF search (proxied through your server — clients never talk to a third party), Windows notifications, Do Not Disturb that turns on during a stream.
- **Private by design.** No ads, no telemetry. Images are re-encoded on the server (EXIF stripped). Update packages are signed (ed25519).
- **Zero-hassle self-hosting.** One script brings up LiveKit, Caddy (automatic HTTPS) and the backend (Rust + SQLite — no database server to manage). See below.
- **Free to host, free to use.** Your own server has no limits at all. We also run an official instance; paid plans there (higher stream quality and bigger files for large communities) are planned, nothing is paywalled on self-hosted servers. AGPL-3.0.

## Status

Windows client only for now (Linux later). Used daily by a small group of friends; expect rough edges — please report bugs in Issues.

## Установка своего сервера

Сервер Murmur — три сервиса в Docker (голос и видео LiveKit, Caddy с сертификатами, бэкенд с аккаунтами и чатом).
Ставится одной командой, готовые образы скачиваются с ghcr.io, на сервере ничего не собирается.

### Что нужно

- VPS с **Ubuntu 22.04 / 24.04** или **Debian 12**, архитектура x86-64, от 1 ГБ памяти.
- Доступ по SSH с правами root (или sudo).
- Свой домен **не обязателен**: без него установщик использует бесплатные имена по IP вида `api.85-12-34-56.sslip.io`.

### Установка

Зайдите на сервер по SSH и выполните:

```sh
curl -fsSL https://raw.githubusercontent.com/Zenden1982/murmur/main/deploy/install.sh | sudo sh
```

Установщик спросит:

1. **Домен.** Enter — без домена (имена sslip.io по IP). Если вводите свой, заранее создайте три A-записи
   `lk.`, `turn.` и `api.` на IP сервера (например, `lk.chat.example.com`); установщик дождётся, пока они заработают.
2. **Почту** для Let's Encrypt. Можно пропустить.
3. **Ник и пароль администратора.**

Дальше он сам поставит Docker, создаст ключи, откроет порты в ufw (если он включён) и запустит сервер.
Установка идёт несколько минут. В конце на экране будут:

- **адрес сервера** — его вписывают в клиент Murmur на экране входа;
- **инвайт** для друзей (по умолчанию на 10 человек, действует 7 дней).

Всё ставится в `/opt/murmur`.

### Порты

Если у хостера есть свой firewall (панель провайдера), откройте входящие:

| Порт | Зачем |
|---|---|
| 443/tcp | HTTPS, API и подключение к LiveKit |
| 7881/tcp | медиа LiveKit по TCP |
| 7882/udp | медиа LiveKit по UDP |
| 3478/udp | TURN |

### Управление: команда `murmur`

| Команда | Что делает |
|---|---|
| `murmur invite [--uses N] [--hours H]` | новый инвайт (по умолчанию 10 человек, 7 дней) |
| `murmur update` | обновить до последней версии; перед этим делается копия базы, настройки и ключи сохраняются |
| `murmur update --domain chat.example.com` | перейти на свой домен; прежний адрес `…sslip.io` продолжит работать |
| `murmur backup` | копия базы данных |
| `murmur status` | состояние сервера и проверка обновлений |
| `murmur logs` | журналы |

Повторный запуск установочной команды тоже обновляет сервер, ничего не стирая.

### Установка без вопросов

Для автоматизации:

```sh
curl -fsSL https://raw.githubusercontent.com/Zenden1982/murmur/main/deploy/install.sh \
  | sudo sh -s -- --yes --admin <ник> --admin-password-file <файл с паролем> [--domain <домен>] [--email <почта>]
```

### Обновления клиента

Свой сервер не хранит выпуски клиента: номер новой версии он берёт у официального сервера
(`UPDATES_UPSTREAM`, по умолчанию `https://api.murmurchat.ru`), а за файлом отправляет клиентов туда же.
Если новому клиенту нужен более свежий сервер, клиенты не увидят это обновление, пока вы не выполните `murmur update`.
Чтобы отключить, задайте пустое `UPDATES_UPSTREAM=` в `/opt/murmur/.env` и перезапустите сервер (`murmur update`).

### Вручную через docker compose

Для тех, кто не хочет запускать скрипт: те же файлы лежат в [`deploy/`](deploy/) (`docker-compose.yml`, `render.sh`,
`.env.example`). Подробности и другие схемы (за своим reverse proxy, без доступа к Docker Hub) — в [deploy/README.md](deploy/README.md).

## Лицензия

AGPL-3.0, см. [LICENSE](LICENSE).
