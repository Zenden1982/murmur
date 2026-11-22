//! HTTP-клиент бэкенда Amadeus (типы — из `proto`).

use std::time::Duration;

use proto::{
    Attachment, AuthResponse, Ban, Category, Channel, ChannelLayout, CreateInviteRequest, ErrorBody, Invite,
    InvitePreview, JoinResponse, LoginRequest, Member, Message, RegisterRequest, SendMessageRequest, Server, User,
    VoiceRoster,
};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct ApiError {
    /// Машинный код (`bad_credentials`, `name_taken`, …) или `network`.
    pub code: String,
    pub message: String,
    /// Исходный текст сервера (для `validation` — что именно не так).
    pub detail: String,
}

impl ApiError {
    fn network(e: reqwest::Error) -> Self {
        let message = if e.is_timeout() {
            "Сервер не ответил вовремя. Проверьте интернет.".to_string()
        } else if e.is_connect() {
            "Не удалось подключиться к серверу.".to_string()
        } else {
            format!("Ошибка сети: {e}")
        };
        Self { code: "network".into(), message, detail: String::new() }
    }

    pub fn is_unauthorized(&self) -> bool {
        self.code == "unauthorized"
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

pub type ApiResult<T> = Result<T, ApiError>;

/// Переводит ответ сервера с ошибкой в понятный текст (сервер шлёт английские сообщения).
fn humanize(status: StatusCode, body: Option<ErrorBody>) -> ApiError {
    let (code, server_msg) = match body {
        Some(b) => (b.error, b.message),
        None => (format!("http_{}", status.as_u16()), String::new()),
    };
    let message = match code.as_str() {
        "bad_credentials" => "Неверный ник или пароль.".to_string(),
        "invalid_invite" => "Приглашение недействительно: неверный код, истёк срок или уже использовано.".to_string(),
        "name_taken" => "Этот ник уже занят. Приглашение не потрачено — выберите другой.".to_string(),
        "rate_limited" => "Слишком много попыток. Подождите несколько минут.".to_string(),
        "unauthorized" => "Сессия истекла — войдите заново.".to_string(),
        "forbidden" => "Недостаточно прав.".to_string(),
        "not_found" => "Не найдено.".to_string(),
        "validation" => format!("Проверьте поля: {server_msg}"),
        _ if server_msg.is_empty() => format!("Ошибка сервера ({status})."),
        _ => format!("Ошибка сервера: {server_msg}"),
    };
    ApiError { code, message, detail: server_msg }
}

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    /// Для долгих ответов (поток событий): без общего таймаута, но с таймаутом чтения.
    stream_http: reqwest::Client,
    base: String,
    token: Option<String>,
}

/// Поток событий SSE: сервер шлёт пинг каждые 15 с — тишина дольше значит обрыв.
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(40);
/// Загрузка картинки (до 8 МБ на медленном канале).
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(180);

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api").field("base", &self.base).field("authorized", &self.token.is_some()).finish()
    }
}

impl Api {
    pub fn new(base: &str, token: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("Amadeus/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client");
        let stream_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(8))
            .read_timeout(STREAM_READ_TIMEOUT)
            .user_agent(concat!("Amadeus/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client");
        Self { http, stream_http, base: base.trim_end_matches('/').to_string(), token }
    }

    pub fn with_token(&self, token: Option<String>) -> Self {
        Self { http: self.http.clone(), stream_http: self.stream_http.clone(), base: self.base.clone(), token }
    }

    /// Адрес сервера без «/» на конце.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Последняя выпущенная версия клиента; `None` — сервер ещё ничего не выпускал (404).
    pub async fn latest_release(&self) -> ApiResult<Option<proto::ReleaseInfo>> {
        let resp = self.request(Method::GET, "/version").send().await.map_err(ApiError::network)?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Self::decode(resp).await.map(Some)
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let req = self.http.request(method, format!("{}{}", self.base, path));
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    async fn call<B: Serialize, T: DeserializeOwned>(&self, method: Method, path: &str, body: Option<&B>) -> ApiResult<T> {
        let mut req = self.request(method, path);
        if let Some(b) = body {
            req = req.json(b);
        }
        Self::decode(req.send().await.map_err(ApiError::network)?).await
    }

    async fn decode<T: DeserializeOwned>(resp: reqwest::Response) -> ApiResult<T> {
        let status = resp.status();
        let bad = |e: &dyn std::fmt::Display| ApiError {
            code: "decode".into(),
            message: format!("Непонятный ответ сервера: {e}"),
            detail: String::new(),
        };
        if status == StatusCode::NO_CONTENT {
            serde_json::from_value(serde_json::Value::Null).map_err(|e| bad(&e))
        } else if status.is_success() {
            resp.json::<T>().await.map_err(|e| bad(&e))
        } else {
            let body = resp.json::<ErrorBody>().await.ok();
            Err(humanize(status, body))
        }
    }

    /// Жив ли сервер (экран входа).
    pub async fn health(&self) -> ApiResult<proto::HealthResponse> {
        self.call(Method::GET, "/health", None::<&()>).await
    }

    pub async fn login(&self, name: &str, password: &str) -> ApiResult<AuthResponse> {
        let body = LoginRequest { name: name.to_string(), password: password.to_string() };
        self.call(Method::POST, "/auth/login", Some(&body)).await
    }

    pub async fn register(&self, invite_code: &str, name: &str, password: &str) -> ApiResult<AuthResponse> {
        let body = RegisterRequest { invite_code: invite_code.to_string(), name: name.to_string(), password: password.to_string() };
        self.call(Method::POST, "/auth/register", Some(&body)).await
    }

    pub async fn logout(&self) -> ApiResult<()> {
        self.call::<(), _>(Method::POST, "/auth/logout", None).await
    }

    pub async fn me(&self) -> ApiResult<User> {
        self.call::<(), _>(Method::GET, "/me", None).await
    }

    pub async fn join(&self, channel_id: i64) -> ApiResult<JoinResponse> {
        self.call::<(), _>(Method::POST, &format!("/channels/{channel_id}/join"), None).await
    }

    pub async fn messages(&self, channel_id: i64, before: Option<i64>, after: Option<i64>) -> ApiResult<Vec<Message>> {
        let mut path = format!("/channels/{channel_id}/messages?limit={}", proto::limits::HISTORY_PAGE);
        if let Some(b) = before {
            path.push_str(&format!("&before={b}"));
        }
        if let Some(a) = after {
            path.push_str(&format!("&after={a}"));
        }
        self.call::<(), _>(Method::GET, &path, None).await
    }

    pub async fn send_message(&self, channel_id: i64, req: &SendMessageRequest) -> ApiResult<Message> {
        self.call(Method::POST, &format!("/channels/{channel_id}/messages"), Some(req)).await
    }

    /// Загрузить картинку (тело — файл как есть). Id вложения указывается при отправке сообщения.
    pub async fn upload(&self, bytes: Vec<u8>) -> ApiResult<Attachment> {
        let req = self
            .request(Method::POST, "/uploads")
            .timeout(UPLOAD_TIMEOUT)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes);
        Self::decode(req.send().await.map_err(ApiError::network)?).await
    }

    /// Файл вложения.
    pub async fn attachment(&self, id: i64) -> ApiResult<Vec<u8>> {
        let resp = self.request(Method::GET, &format!("/attachments/{id}")).send().await.map_err(ApiError::network)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(humanize(status, resp.json::<ErrorBody>().await.ok()));
        }
        Ok(resp.bytes().await.map_err(ApiError::network)?.to_vec())
    }

    pub async fn servers(&self) -> ApiResult<Vec<proto::Server>> {
        self.call::<(), _>(Method::GET, "/servers", None).await
    }

    pub async fn members(&self, server_id: i64) -> ApiResult<Vec<proto::Member>> {
        self.call::<(), _>(Method::GET, &format!("/servers/{server_id}/members"), None).await
    }

    /// Отображаемое имя (пустая строка — сбросить на логин).
    pub async fn update_me(&self, display_name: &str) -> ApiResult<User> {
        let body = proto::UpdateMeRequest { display_name: Some(display_name.to_string()), dm_from_servers: None, ..Default::default() };
        self.call(Method::PATCH, "/me", Some(&body)).await
    }

    /// Кто может писать мне в ЛС: true — и люди с общих серверов.
    pub async fn set_dm_from_servers(&self, open: bool) -> ApiResult<User> {
        let body = proto::UpdateMeRequest { display_name: None, dm_from_servers: Some(open), ..Default::default() };
        self.call(Method::PATCH, "/me", Some(&body)).await
    }

    /// «О себе» и свой статус (None — не менять, пустая строка — убрать).
    pub async fn update_profile(&self, about: Option<String>, status: Option<String>) -> ApiResult<User> {
        let body = proto::UpdateMeRequest { about, status, ..Default::default() };
        self.call(Method::PATCH, "/me", Some(&body)).await
    }

    /// Во что играю (None — ни во что).
    pub async fn set_activity(&self, game: Option<String>, since: Option<i64>) -> ApiResult<()> {
        self.call(Method::PUT, "/me/activity", Some(&proto::SetActivityRequest { game, since })).await
    }

    /// Карточка профиля.
    pub async fn user_profile(&self, id: i64) -> ApiResult<proto::UserProfile> {
        self.call::<(), _>(Method::GET, &format!("/users/{id}"), None).await
    }

    /// Позвать человека в голосовой канал.
    pub async fn summon(&self, channel_id: i64, user_id: i64) -> ApiResult<()> {
        self.call(Method::POST, &format!("/channels/{channel_id}/summon"), Some(&proto::SummonRequest { user_id })).await
    }

    /// Мои диалоги (ЛС), свежие сверху.
    pub async fn dms(&self) -> ApiResult<Vec<proto::DmChannel>> {
        self.call::<(), _>(Method::GET, "/dms", None).await
    }

    /// Открыть диалог с человеком (или найти существующий).
    pub async fn open_dm(&self, user_id: i64) -> ApiResult<proto::DmChannel> {
        self.call(Method::POST, "/dms", Some(&proto::OpenDmRequest { user_id })).await
    }

    /// Позвонить в ЛС или ответить на звонок — токен комнаты звонка.
    pub async fn dm_call(&self, channel_id: i64) -> ApiResult<JoinResponse> {
        self.call::<(), _>(Method::POST, &format!("/dms/{channel_id}/call"), None).await
    }

    /// Положить трубку: отменить, отклонить или выйти из звонка.
    pub async fn dm_hang_up(&self, channel_id: i64) -> ApiResult<()> {
        self.call::<(), _>(Method::DELETE, &format!("/dms/{channel_id}/call"), None).await
    }

    /// Каналы с непрочитанными сообщениями.
    pub async fn unread(&self) -> ApiResult<Vec<proto::UnreadChannel>> {
        self.call::<(), _>(Method::GET, "/unread", None).await
    }

    /// Канал прочитан до сообщения включительно.
    pub async fn mark_read(&self, channel_id: i64, message_id: i64) -> ApiResult<()> {
        self.call(Method::POST, &format!("/channels/{channel_id}/read"), Some(&proto::ReadRequest { message_id })).await
    }

    pub async fn put_avatar(&self, bytes: Vec<u8>) -> ApiResult<User> {
        let req = self
            .request(Method::PUT, "/me/avatar")
            .timeout(UPLOAD_TIMEOUT)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes);
        Self::decode(req.send().await.map_err(ApiError::network)?).await
    }

    pub async fn friends(&self) -> ApiResult<proto::FriendsList> {
        self.call::<(), _>(Method::GET, "/friends", None).await
    }

    /// Заявка в друзья по логину (встречная заявка — сразу дружба).
    pub async fn add_friend(&self, name: &str) -> ApiResult<proto::FriendsList> {
        self.call(Method::POST, "/friends/requests", Some(&proto::AddFriendRequest { name: name.to_string() })).await
    }

    pub async fn accept_friend(&self, id: i64) -> ApiResult<proto::FriendsList> {
        self.call::<(), _>(Method::POST, &format!("/friends/{id}/accept"), None).await
    }

    /// Поставить/убрать свою реакцию на сообщение.
    pub async fn react(&self, message_id: i64, name: &str, on: bool) -> ApiResult<()> {
        let m = if on { Method::PUT } else { Method::DELETE };
        self.call::<(), _>(m, &format!("/messages/{message_id}/reactions/{}", path_segment(name)), None).await
    }

    /// Удалить из друзей / отклонить / отменить заявку.
    pub async fn remove_friend(&self, id: i64) -> ApiResult<()> {
        self.call::<(), _>(Method::DELETE, &format!("/friends/{id}"), None).await
    }

    pub async fn delete_avatar(&self) -> ApiResult<User> {
        self.call::<(), _>(Method::DELETE, "/me/avatar", None).await
    }

    /// Свой баннер: 960×384 (PNG или GIF).
    pub async fn put_banner(&self, bytes: Vec<u8>) -> ApiResult<User> {
        let req = self
            .request(Method::PUT, "/me/banner")
            .timeout(UPLOAD_TIMEOUT)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes);
        Self::decode(req.send().await.map_err(ApiError::network)?).await
    }

    pub async fn delete_banner(&self) -> ApiResult<User> {
        self.call::<(), _>(Method::DELETE, "/me/banner", None).await
    }

    /// Картинка профиля/сервера по ключу.
    pub async fn image(&self, key: &str) -> ApiResult<Vec<u8>> {
        let resp = self.request(Method::GET, &format!("/images/{key}")).send().await.map_err(ApiError::network)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(humanize(status, resp.json::<ErrorBody>().await.ok()));
        }
        Ok(resp.bytes().await.map_err(ApiError::network)?.to_vec())
    }

    /// Открыть поток событий `GET /events` (SSE). Данные читать `Response::chunk()`.
    pub async fn events(&self) -> ApiResult<reqwest::Response> {
        let mut req = self
            .stream_http
            .get(format!("{}/events", self.base))
            .header(reqwest::header::ACCEPT, "text/event-stream");
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.map_err(ApiError::network)?;
        let status = resp.status();
        if status.is_success() {
            Ok(resp)
        } else {
            Err(humanize(status, resp.json::<ErrorBody>().await.ok()))
        }
    }
}

/// Фаза 9.1: серверы, роли, приглашения, каналы и категории.
impl Api {
    async fn put_image<T: DeserializeOwned>(&self, path: &str, bytes: Vec<u8>) -> ApiResult<T> {
        let req = self
            .request(Method::PUT, path)
            .timeout(UPLOAD_TIMEOUT)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes);
        Self::decode(req.send().await.map_err(ApiError::network)?).await
    }

    pub async fn create_server(&self, name: &str) -> ApiResult<Server> {
        self.call(Method::POST, "/servers", Some(&proto::CreateServerRequest { name: name.to_string() })).await
    }

    /// Профиль сервера: описание, узор баннера, цвет (None — не менять, пустая строка — по умолчанию).
    pub async fn update_server_profile(&self, id: i64, body: proto::UpdateServerRequest) -> ApiResult<Server> {
        self.call(Method::PATCH, &format!("/servers/{id}"), Some(&body)).await
    }

    pub async fn put_server_banner(&self, id: i64, bytes: Vec<u8>) -> ApiResult<Server> {
        self.put_image(&format!("/servers/{id}/banner"), bytes).await
    }

    pub async fn delete_server_banner(&self, id: i64) -> ApiResult<Server> {
        self.call::<(), _>(Method::DELETE, &format!("/servers/{id}/banner"), None).await
    }

    pub async fn put_server_icon(&self, id: i64, bytes: Vec<u8>) -> ApiResult<Server> {
        self.put_image(&format!("/servers/{id}/icon"), bytes).await
    }

    pub async fn delete_server_icon(&self, id: i64) -> ApiResult<Server> {
        self.call::<(), _>(Method::DELETE, &format!("/servers/{id}/icon"), None).await
    }

    pub async fn delete_server(&self, id: i64) -> ApiResult<()> {
        self.call::<(), _>(Method::DELETE, &format!("/servers/{id}"), None).await
    }

    pub async fn transfer(&self, id: i64, user_id: i64) -> ApiResult<Server> {
        self.call(Method::POST, &format!("/servers/{id}/transfer"), Some(&proto::TransferRequest { user_id })).await
    }

    pub async fn leave(&self, id: i64) -> ApiResult<()> {
        self.call::<(), _>(Method::DELETE, &format!("/servers/{id}/members/@me"), None).await
    }

    pub async fn set_role(&self, id: i64, user_id: i64, role_id: i64) -> ApiResult<Member> {
        let body = proto::SetRoleRequest { role_id };
        self.call(Method::PUT, &format!("/servers/{id}/members/{user_id}/role"), Some(&body)).await
    }

    pub async fn kick(&self, id: i64, user_id: i64) -> ApiResult<()> {
        self.call::<(), _>(Method::DELETE, &format!("/servers/{id}/members/{user_id}"), None).await
    }

    pub async fn disconnect(&self, id: i64, user_id: i64) -> ApiResult<()> {
        self.call::<(), _>(Method::POST, &format!("/servers/{id}/members/{user_id}/disconnect"), None).await
    }

    pub async fn ban(&self, id: i64, user_id: i64, reason: Option<String>) -> ApiResult<()> {
        self.call(Method::POST, &format!("/servers/{id}/bans"), Some(&proto::BanRequest { user_id, reason })).await
    }

    pub async fn unban(&self, id: i64, user_id: i64) -> ApiResult<()> {
        self.call::<(), _>(Method::DELETE, &format!("/servers/{id}/bans/{user_id}"), None).await
    }

    pub async fn bans(&self, id: i64) -> ApiResult<Vec<Ban>> {
        self.call::<(), _>(Method::GET, &format!("/servers/{id}/bans"), None).await
    }

    /// Куда зовёт приглашение (без входа).
    pub async fn invite_preview(&self, code: &str) -> ApiResult<InvitePreview> {
        self.call::<(), _>(Method::GET, &format!("/invites/{}", normalize_invite(code)), None).await
    }

    pub async fn join_invite(&self, code: &str) -> ApiResult<Server> {
        self.call::<(), _>(Method::POST, &format!("/invites/{}/join", normalize_invite(code)), None).await
    }

    pub async fn revoke_invite(&self, code: &str) -> ApiResult<()> {
        self.call::<(), _>(Method::DELETE, &format!("/invites/{code}"), None).await
    }

    pub async fn server_invites(&self, id: i64) -> ApiResult<Vec<Invite>> {
        self.call::<(), _>(Method::GET, &format!("/servers/{id}/invites"), None).await
    }

    pub async fn create_server_invite(&self, id: i64, uses: u32, expires_in_hours: u32) -> ApiResult<Invite> {
        let body = CreateInviteRequest { uses: Some(uses), expires_in_hours: Some(expires_in_hours) };
        self.call(Method::POST, &format!("/servers/{id}/invites"), Some(&body)).await
    }

    pub async fn layout(&self, server: i64) -> ApiResult<ChannelLayout> {
        self.call::<(), _>(Method::GET, &format!("/servers/{server}/channels"), None).await
    }

    pub async fn create_channel(&self, server: i64, req: &proto::CreateChannelRequest) -> ApiResult<Channel> {
        self.call(Method::POST, &format!("/servers/{server}/channels"), Some(req)).await
    }

    pub async fn update_channel(&self, id: i64, req: &proto::UpdateChannelRequest) -> ApiResult<Channel> {
        self.call(Method::PATCH, &format!("/channels/{id}"), Some(req)).await
    }

    pub async fn delete_channel(&self, id: i64) -> ApiResult<()> {
        self.call::<(), _>(Method::DELETE, &format!("/channels/{id}"), None).await
    }

    pub async fn create_category(&self, server: i64, name: &str) -> ApiResult<Category> {
        let body = proto::CategoryRequest { name: name.to_string() };
        self.call(Method::POST, &format!("/servers/{server}/categories"), Some(&body)).await
    }

    pub async fn rename_category(&self, id: i64, name: &str) -> ApiResult<ChannelLayout> {
        let body = proto::CategoryRequest { name: name.to_string() };
        self.call(Method::PATCH, &format!("/categories/{id}"), Some(&body)).await
    }

    pub async fn delete_category(&self, id: i64) -> ApiResult<()> {
        self.call::<(), _>(Method::DELETE, &format!("/categories/{id}"), None).await
    }

    pub async fn put_layout(&self, server: i64, req: &proto::LayoutRequest) -> ApiResult<ChannelLayout> {
        self.call(Method::PUT, &format!("/servers/{server}/layout"), Some(req)).await
    }

    /// Кто сейчас в голосовых каналах сервера.
    pub async fn voice(&self, server: i64) -> ApiResult<Vec<VoiceRoster>> {
        self.call::<(), _>(Method::GET, &format!("/servers/{server}/voice"), None).await
    }
}

/// Нормализует код приглашения: только буквы/цифры в верхнем регистре, максимум 12 знаков.
pub fn normalize_invite(raw: &str) -> String {
    raw.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).take(12).collect()
}

/// Код из того, что вставили: сам код или ссылка (берём хвост после последнего «/», без ?query и #).
pub fn invite_from_input(raw: &str) -> String {
    let s = raw.trim();
    let s = s.split(['?', '#']).next().unwrap_or(s).trim_end_matches('/');
    normalize_invite(s.rsplit('/').next().unwrap_or(s))
}

/// `K7PMQ4XXYYZZ` → `K7PM-Q4XX-YYZZ`.
pub fn format_invite(norm: &str) -> String {
    norm.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join("-")
}

#[cfg(test)]
mod tests {
    #[test]
    fn invite_from_link_or_code() {
        use super::invite_from_input as f;
        assert_eq!(f("k7pm-q4xx-yyzz"), "K7PMQ4XXYYZZ");
        assert_eq!(f(" https://murmurchat.ru/i/K7PM-Q4XX-YYZZ/ "), "K7PMQ4XXYYZZ");
        assert_eq!(f("https://murmurchat.ru/invite/K7PMQ4XXYYZZ?from=tg#x"), "K7PMQ4XXYYZZ");
    }

    use super::*;

    #[test]
    fn invite_normalization() {
        assert_eq!(normalize_invite(" k7pm-q4xx-yyzz "), "K7PMQ4XXYYZZ");
        assert_eq!(normalize_invite("k7pm q4xx yyzz extra"), "K7PMQ4XXYYZZ");
        assert_eq!(format_invite("K7PMQ4XXYYZZ"), "K7PM-Q4XX-YYZZ");
        assert_eq!(format_invite("K7PMQ"), "K7PM-Q");
    }

    #[test]
    fn humanize_known_codes() {
        let e = humanize(StatusCode::CONFLICT, Some(ErrorBody { error: "name_taken".into(), message: "x".into() }));
        assert!(e.message.contains("не потрачено"));
        let e = humanize(StatusCode::BAD_GATEWAY, None);
        assert_eq!(e.code, "http_502");
    }
}

/// Кусок пути URL: всё, кроме [A-Za-z0-9_.-], — %XX (эмодзи-реакции вроде #️⃣ содержат «#»).
fn path_segment(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"_.-".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

#[cfg(test)]
mod path_tests {
    #[test]
    fn path_segment_escapes() {
        assert_eq!(super::path_segment("fire"), "fire");
        assert_eq!(super::path_segment("+1"), "%2B1");
        assert_eq!(super::path_segment("#\u{FE0F}\u{20E3}"), "%23%EF%B8%8F%E2%83%A3");
    }
}
