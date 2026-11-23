//! Бэкенд Murmur: аккаунты (только по инвайтам), каналы, выдача LiveKit-токенов, чат.

pub mod config;
pub mod error;
pub mod proxy_protocol;
pub mod ratelimit;
pub mod security;

mod account;
pub mod gifs;
mod channels;
pub mod chat;
mod db;
pub mod dm;
pub mod events;
mod friends;
mod images;
mod media;
mod invites;
mod profile;
mod servers;
pub mod voice;
mod extract;
pub(crate) mod perm;
pub mod releases;

pub use extract::ClientAddr;
mod routes;

use std::{path::PathBuf, sync::Arc, time::Duration};

use tokio::sync::{broadcast, watch};

pub use db::{admin_exists, backup, connect, connect_existing, create_invite, create_user};
pub use routes::router;

use config::LiveKitConfig;
use ratelimit::RateLimiter;

/// Срок жизни сессии клиента.
pub const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
/// Срок жизни LiveKit-токена (PLAN.md §3, фаза 2).
pub const LIVEKIT_TOKEN_TTL: Duration = Duration::from_secs(15 * 60);
/// Лимит попыток входа/регистрации с одного IP.
pub const AUTH_ATTEMPTS: u32 = 10;
pub const AUTH_WINDOW: Duration = Duration::from_secs(5 * 60);
/// Лимит сообщений на пользователя.
pub const CHAT_MESSAGES: u32 = 20;
pub const CHAT_WINDOW: Duration = Duration::from_secs(10);
/// Лимит загрузок картинок на пользователя.
pub const UPLOADS: u32 = 30;
pub const UPLOADS_WINDOW: Duration = Duration::from_secs(60);
/// Лимит смен аватара/баннера/иконок на пользователя (тяжёлая обработка картинки).
pub const MEDIA_CHANGES: u32 = 10;
/// Лимит просмотров приглашения без входа (отдельно от входа: страницу приглашения открывают часто).
pub const PREVIEW_ATTEMPTS: u32 = 60;
pub const MEDIA_WINDOW: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<Inner>,
}

pub struct Inner {
    pub db: sqlx::SqlitePool,
    pub livekit: LiveKitConfig,
    pub trusted_proxy_hops: usize,
    pub auth_limiter: RateLimiter,
    pub chat_limiter: RateLimiter<i64>,
    pub upload_limiter: RateLimiter<i64>,
    pub media_limiter: RateLimiter<i64>,
    /// Запись картинки + смена ключа + удаление старого файла — по одному (ключи-хеши общие, S12).
    pub image_lock: tokio::sync::Mutex<()>,
    pub preview_limiter: RateLimiter,
    /// Где лежат файлы вложений.
    pub uploads_dir: PathBuf,
    /// Выпуски клиента (`latest.json` + файлы), см. `releases.rs`.
    pub releases_dir: PathBuf,
    /// События для `GET /events` (каждое — со своей аудиторией).
    pub events: broadcast::Sender<events::Scoped>,
    /// Официальный сервер, с которого берутся обновления клиента, если своих выпусков нет.
    pub upstream: Option<releases::Upstream>,
    /// GIF через Klipy (None — выключено, нет ключа).
    pub klipy: Option<gifs::Klipy>,
    /// Комнаты LiveKit (RoomService); по умолчанию — заглушка.
    pub rooms: Arc<dyn voice::Rooms>,
    /// Кто сейчас в голосовых каналах.
    pub presence: voice::Presence,
    /// Кто в сети (открыт поток событий) — для друзей.
    pub online: friends::Online,
    /// Звонки в ЛС.
    pub calls: dm::Calls,
    /// true — сервис останавливается: потоки SSE закрываются (иначе graceful shutdown ждал бы их вечно).
    pub shutdown: watch::Sender<bool>,
}

impl AppState {
    pub fn new(
        db: sqlx::SqlitePool,
        livekit: LiveKitConfig,
        trusted_proxy_hops: usize,
        uploads_dir: PathBuf,
        releases_dir: PathBuf,
    ) -> Self {
        Self::with_limiter(
            db,
            livekit,
            trusted_proxy_hops,
            RateLimiter::new(AUTH_ATTEMPTS, AUTH_WINDOW),
            uploads_dir,
            releases_dir,
        )
    }

    pub fn with_limiter(
        db: sqlx::SqlitePool,
        livekit: LiveKitConfig,
        trusted_proxy_hops: usize,
        auth_limiter: RateLimiter,
        uploads_dir: PathBuf,
        releases_dir: PathBuf,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                livekit,
                trusted_proxy_hops,
                auth_limiter,
                chat_limiter: RateLimiter::new(CHAT_MESSAGES, CHAT_WINDOW),
                upload_limiter: RateLimiter::new(UPLOADS, UPLOADS_WINDOW),
                media_limiter: RateLimiter::new(MEDIA_CHANGES, MEDIA_WINDOW),
                image_lock: tokio::sync::Mutex::new(()),
                preview_limiter: RateLimiter::new(PREVIEW_ATTEMPTS, AUTH_WINDOW),
                uploads_dir,
                releases_dir,
                events: broadcast::channel(256).0,
                upstream: None,
                klipy: None,
                rooms: Arc::new(voice::NoRooms),
                presence: voice::Presence::default(),
                online: friends::Online::default(),
                calls: dm::Calls::default(),
                shutdown: watch::channel(false).0,
            }),
        }
    }

    /// Подключить upstream обновлений (сразу после создания состояния).
    pub fn with_upstream(mut self, up: releases::Upstream) -> Self {
        Arc::get_mut(&mut self.inner).expect("with_upstream до клонирования состояния").upstream = Some(up);
        self
    }

    /// Включить GIF (ключ Klipy), сразу после создания состояния.
    pub fn with_klipy(mut self, klipy: gifs::Klipy) -> Self {
        Arc::get_mut(&mut self.inner).expect("with_klipy до клонирования состояния").klipy = Some(klipy);
        self
    }

    /// Подключить RoomService (сразу после создания состояния).
    pub fn with_rooms(mut self, rooms: Arc<dyn voice::Rooms>) -> Self {
        Arc::get_mut(&mut self.inner).expect("with_rooms до клонирования состояния").rooms = rooms;
        self
    }

    /// Закрыть потоки событий (перед остановкой сервиса).
    pub fn close_streams(&self) {
        self.inner.shutdown.send_replace(true);
    }
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
