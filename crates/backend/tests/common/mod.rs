//! Общее для интеграционных тестов: in-memory SQLite, без сети, мок LiveKit RoomService.
#![allow(dead_code)]

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use backend::{AppState, config::LiveKitConfig, ratelimit::RateLimiter};
use http_body_util::BodyExt;
use proto::{AuthResponse, ErrorBody, Invite, ServerEvent};
use serde_json::{Value, json};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tower::ServiceExt;

pub const LK_KEY: &str = "APItestkey";
pub const LK_SECRET: &str = "test-secret-test-secret-test-secret-0123";
pub const LK_URL: &str = "wss://lk.example.test";

pub struct T {
    pub app: Router,
    pub pool: sqlx::SqlitePool,
    pub state: AppState,
    pub uploads: std::path::PathBuf,
    pub rooms: Arc<MockRooms>,
}

pub async fn setup_with_limit(limit: u32) -> T {
    setup_inner(limit, None).await
}

/// Сервер с «upstream обновлений» (официальный сервер) — см. releases::Upstream.
pub async fn setup_with_upstream(up: backend::releases::Upstream) -> T {
    setup_inner(1000, Some(up)).await
}

async fn setup_inner(limit: u32, upstream: Option<backend::releases::Upstream>) -> T {
    let opts: SqliteConnectOptions = "sqlite::memory:".parse().unwrap();
    // одно соединение — одна in-memory база на тест
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts.foreign_keys(true))
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let lk = LiveKitConfig {
        public_url: LK_URL.into(),
        api_key: LK_KEY.into(),
        api_secret: LK_SECRET.into(),
    };
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let uploads = std::env::temp_dir().join(format!(
        "murmur-test-uploads-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let state = AppState::with_limiter(
        pool.clone(),
        lk,
        1,
        RateLimiter::new(limit, Duration::from_secs(300)),
        uploads.clone(),
        uploads.join("releases"),
    );
    let rooms = Arc::new(MockRooms::default());
    let mut state = state.with_rooms(rooms.clone());
    if let Some(up) = upstream {
        state = state.with_upstream(up);
    }
    T {
        rooms,
        app: backend::router(state.clone()),
        pool,
        state,
        uploads,
    }
}

impl Drop for T {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.uploads);
    }
}

pub async fn setup() -> T {
    setup_with_limit(1000).await
}

impl T {
    pub async fn call(
        &self,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        self.call_from(method, uri, token, body, "203.0.113.1")
            .await
    }

    pub async fn call_from(
        &self,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
        ip: &str,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-forwarded-for", ip);
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, v)
    }

    /// Сырое тело (загрузка файла). Возвращает статус, JSON (если есть) и заголовки.
    pub async fn call_raw(
        &self,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Vec<u8>,
    ) -> (StatusCode, Value, axum::http::HeaderMap, Vec<u8>) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-forwarded-for", "203.0.113.1");
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = req
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from(body))
            .unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes().to_vec();
        let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v, headers, bytes)
    }

    pub async fn admin(&self) -> String {
        backend::create_user(&self.pool, "admin", "admin-password", true)
            .await
            .unwrap();
        self.login("admin", "admin-password").await
    }

    pub async fn login(&self, name: &str, password: &str) -> String {
        let (s, v) = self
            .call(
                Method::POST,
                "/auth/login",
                None,
                Some(json!({"name": name, "password": password})),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        serde_json::from_value::<AuthResponse>(v).unwrap().token
    }

    pub async fn invite(&self, admin: &str, body: Value) -> Invite {
        let (s, v) = self
            .call(Method::POST, "/invites", Some(admin), Some(body))
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        serde_json::from_value(v).unwrap()
    }

    pub async fn register(&self, code: &str, name: &str) -> (StatusCode, Value) {
        self.call(
            Method::POST,
            "/auth/register",
            None,
            Some(json!({"invite_code": code, "name": name, "password": "password123"})),
        )
        .await
    }
}

pub fn err(v: &Value) -> String {
    serde_json::from_value::<ErrorBody>(v.clone())
        .map(|e| e.error)
        .unwrap_or_default()
}


pub async fn next_event(body: &mut Body, buf: &mut String) -> ServerEvent {
    loop {
        if let Some(end) = buf.find("\n\n") {
            let block: String = buf.drain(..end + 2).collect();
            if let Some(data) = block.lines().find_map(|l| l.strip_prefix("data:")) {
                return serde_json::from_str(data.trim()).unwrap();
            }
            continue;
        }
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("событие не пришло")
            .expect("поток закрыт")
            .unwrap();
        if let Ok(data) = frame.into_data() {
            buf.push_str(std::str::from_utf8(&data).unwrap());
        }
    }
}

/// Мок RoomService: что «сейчас в комнатах» и кого выкинули.
#[derive(Default)]
pub struct MockRooms {
    pub listing: Mutex<Vec<(String, Vec<String>)>>,
    pub removed: Mutex<Vec<(String, String)>>,
}

impl backend::voice::Rooms for MockRooms {
    fn list(&self) -> backend::voice::BoxFut<'_, anyhow::Result<backend::voice::RoomListing>> {
        let v = self.listing.lock().unwrap().clone();
        Box::pin(async move { Ok(v) })
    }
    fn remove<'a>(&'a self, room: &'a str, identity: &'a str) -> backend::voice::BoxFut<'a, anyhow::Result<()>> {
        self.removed.lock().unwrap().push((room.to_string(), identity.to_string()));
        // как настоящий LiveKit: человека больше нет в комнате
        if let Some((_, ids)) = self.listing.lock().unwrap().iter_mut().find(|(r, _)| r == room) {
            ids.retain(|i| i != identity);
        }
        Box::pin(async { Ok(()) })
    }
}

impl T {
    /// Обычный пользователь (не админ инстанса), сразу с сессией.
    pub async fn user(&self, name: &str) -> (String, i64) {
        let u = backend::create_user(&self.pool, name, "password123", false).await.unwrap();
        (self.login(name, "password123").await, u.id)
    }

    pub async fn create_server(&self, tok: &str, name: &str) -> proto::Server {
        let (s, v) = self.call(Method::POST, "/servers", Some(tok), Some(json!({"name": name}))).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        serde_json::from_value(v).unwrap()
    }

    /// Добавить участника напрямую (позиция роли: 0 участник … 2 админ).
    pub async fn add_member(&self, server: i64, user: i64, position: i32) {
        sqlx::query(
            "INSERT INTO server_members (server_id, user_id, role_id, joined_at)
             SELECT ?, ?, id, 0 FROM server_roles WHERE server_id = ? AND position = ?",
        )
        .bind(server).bind(user).bind(server).bind(position)
        .execute(&self.pool).await.unwrap();
    }

    /// Подставить «кто в комнате» и сверить присутствие (как после вебхуков).
    pub async fn set_voice(&self, room: &str, ids: &[i64]) {
        {
            let mut l = self.rooms.listing.lock().unwrap();
            l.retain(|(r, _)| r != room);
            l.push((room.to_string(), ids.iter().map(|i| i.to_string()).collect()));
        }
        backend::voice::reconcile(&self.state).await.unwrap();
    }

    pub fn removed(&self) -> Vec<(String, String)> {
        self.rooms.removed.lock().unwrap().clone()
    }
}
