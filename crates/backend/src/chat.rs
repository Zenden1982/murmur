//! Чат текстовых каналов: история, отправка, картинки, поток событий (SSE).
//!
//! Новые сообщения доставляются через `GET /events` (Server-Sent Events), а не WebSocket: обычный HTTP-ответ
//! проходит через системный прокси клиента и reverse proxy без особых настроек (`X-Accel-Buffering: no`).

use std::{collections::HashMap, path::Path as FsPath, time::Duration};

use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State, rejection::BytesRejection, rejection::QueryRejection},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use proto::{Attachment, Author, Message, Reaction, SendMessageRequest, ServerEvent, limits};
use serde::Deserialize;

use crate::{
    AppState,
    error::{AppError, AppResult},
    events::{Audience, publish},
    perm,
    extract::{ApiJson, AuthUser},
    now, security,
};

/// Сколько живёт загруженная, но не отправленная картинка.
pub const PENDING_TTL_SECS: i64 = 3600;
/// Неотправленных картинок на пользователя.
const PENDING_MAX: i64 = 20;

/// Литерал (а не const), чтобы склеивать запросы через `concat!` — sqlx принимает только статические строки.
macro_rules! select_messages {
    () => {
        "SELECT m.id, m.channel_id, m.author_id, u.name AS author_name, u.display_name AS author_display, u.avatar_key AS author_avatar, m.content,
        m.created_at, m.edited_at, m.reply_to, m.sticker
     FROM messages m JOIN users u ON u.id = m.author_id
     WHERE m.channel_id = ? AND m.deleted_at IS NULL"
    };
}

#[derive(sqlx::FromRow)]
struct MessageRow {
    id: i64,
    channel_id: i64,
    author_id: i64,
    author_name: String,
    author_display: Option<String>,
    author_avatar: Option<String>,
    content: String,
    created_at: i64,
    edited_at: Option<i64>,
    reply_to: Option<i64>,
    sticker: Option<String>,
}

#[derive(sqlx::FromRow)]
struct AttachmentRow {
    id: i64,
    message_id: Option<i64>,
    mime: String,
    size: i64,
    width: Option<i64>,
    height: Option<i64>,
}

impl From<&AttachmentRow> for Attachment {
    fn from(r: &AttachmentRow) -> Self {
        Attachment {
            id: r.id,
            mime: r.mime.clone(),
            size: r.size.max(0) as u64,
            width: r.width.unwrap_or(0).max(0) as u32,
            height: r.height.unwrap_or(0).max(0) as u32,
        }
    }
}

/// Куда относится чат: текстовый канал сервера или ЛС.
pub(crate) struct ChatTarget {
    /// Кому рассылать новые сообщения.
    pub audience: Audience,
    /// ЛС: (собеседник, кто открыл диалог).
    pub dm: Option<(i64, i64)>,
}

/// Текстовый канал сервера, где ты участник, или твой ЛС. Чужой — NotFound.
pub(crate) async fn chat_target(state: &AppState, id: i64, user_id: i64) -> AppResult<ChatTarget> {
    let db = &state.inner.db;
    let ch: crate::db::ChannelRow = sqlx::query_as(crate::db::CHANNEL_SELECT_BY_ID)
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)?;
    if ch.server_id.is_none() {
        let (other, opened_by) = crate::dm::dm_member(db, id, user_id).await?.ok_or(AppError::NotFound)?;
        return Ok(ChatTarget { audience: Audience::Users(vec![user_id, other]), dm: Some((other, opened_by)) });
    }
    let (m, ch) = perm::channel_member(db, id, user_id).await?;
    if ch.kind != "text" {
        return Err(AppError::BadRequest("это не текстовый канал".into()));
    }
    Ok(ChatTarget { audience: Audience::Server(m.server_id), dm: None })
}

/// Реакции на сообщения (одним запросом): message_id → реакции в порядке первой постановки.
async fn load_reactions(db: &sqlx::SqlitePool, ids: &[i64]) -> AppResult<HashMap<i64, Vec<Reaction>>> {
    let mut out: HashMap<i64, Vec<Reaction>> = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT message_id, user_id, name FROM message_reactions WHERE message_id IN (");
    let mut sep = qb.separated(", ");
    for id in ids {
        sep.push_bind(*id);
    }
    qb.push(") ORDER BY created_at, rowid");
    let rows: Vec<(i64, i64, String)> = qb.build_query_as().fetch_all(db).await?;
    for (mid, uid, name) in rows {
        let list = out.entry(mid).or_default();
        match list.iter_mut().find(|r| r.name == name) {
            Some(r) => r.users.push(uid),
            None => list.push(Reaction { name, users: vec![uid] }),
        }
    }
    Ok(out)
}

/// Строки сообщений → `Message` с вложениями (одним запросом на всю страницу).
async fn with_attachments(state: &AppState, rows: Vec<MessageRow>) -> AppResult<Vec<Message>> {
    let mut by_msg: HashMap<i64, Vec<Attachment>> = HashMap::new();
    if !rows.is_empty() {
        let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT id, message_id, mime, size, width, height FROM attachments WHERE message_id IN (",
        );
        let mut sep = qb.separated(", ");
        for r in &rows {
            sep.push_bind(r.id);
        }
        qb.push(") ORDER BY id");
        let atts: Vec<AttachmentRow> = qb.build_query_as().fetch_all(&state.inner.db).await?;
        for a in &atts {
            if let Some(mid) = a.message_id {
                by_msg.entry(mid).or_default().push(a.into());
            }
        }
    }
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let mut reactions = load_reactions(&state.inner.db, &ids).await?;
    Ok(rows
        .into_iter()
        .map(|r| Message {
            attachments: by_msg.remove(&r.id).unwrap_or_default(),
            reactions: reactions.remove(&r.id).unwrap_or_default(),
            sticker: r.sticker,
            id: r.id,
            channel_id: r.channel_id,
            author: Author {
                display_name: r.author_display,
                avatar: r.author_avatar,
                id: r.author_id,
                name: r.author_name,
            },
            content: r.content,
            created_at: r.created_at,
            edited_at: r.edited_at,
            reply_to: r.reply_to,
        })
        .collect())
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    /// Сообщения старше этого id (подгрузка при прокрутке вверх).
    before: Option<i64>,
    /// Сообщения новее этого id (дозапрос после переподключения).
    after: Option<i64>,
    limit: Option<u32>,
}

/// `GET /channels/{id}/messages?before=|after=&limit=` — страница истории, всегда по возрастанию id.
/// Без параметров — последние `limit` сообщений.
pub async fn list_messages(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    q: Result<Query<HistoryQuery>, QueryRejection>,
) -> AppResult<Json<Vec<Message>>> {
    let Query(q) = q.map_err(|e| AppError::BadRequest(e.body_text()))?;
    chat_target(&state, id, user.id).await?;
    let limit = q
        .limit
        .unwrap_or(limits::HISTORY_PAGE)
        .clamp(1, limits::HISTORY_PAGE_MAX) as i64;
    let rows: Vec<MessageRow> = match (q.before, q.after) {
        (Some(_), Some(_)) => {
            return Err(AppError::BadRequest(
                "before и after вместе не поддерживаются".into(),
            ));
        }
        (None, Some(after)) => {
            sqlx::query_as(concat!(
                select_messages!(),
                " AND m.id > ? ORDER BY m.id ASC LIMIT ?"
            ))
            .bind(id)
            .bind(after)
            .bind(limit)
            .fetch_all(&state.inner.db)
            .await?
        }
        (before, None) => {
            let mut rows: Vec<MessageRow> = sqlx::query_as(concat!(
                select_messages!(),
                " AND m.id < ? ORDER BY m.id DESC LIMIT ?"
            ))
            .bind(id)
            .bind(before.unwrap_or(i64::MAX))
            .bind(limit)
            .fetch_all(&state.inner.db)
            .await?;
            rows.reverse();
            rows
        }
    };
    Ok(Json(with_attachments(&state, rows).await?))
}

async fn load_message(state: &AppState, channel_id: i64, id: i64) -> AppResult<Message> {
    let rows: Vec<MessageRow> = sqlx::query_as(concat!(select_messages!(), " AND m.id = ?"))
        .bind(channel_id)
        .bind(id)
        .fetch_all(&state.inner.db)
        .await?;
    with_attachments(state, rows)
        .await?
        .pop()
        .ok_or(AppError::NotFound)
}

/// `POST /channels/{id}/messages` — отправить. Событие `message_created` получают все (и сам автор).
pub async fn send_message(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    ApiJson(req): ApiJson<SendMessageRequest>,
) -> AppResult<(StatusCode, Json<Message>)> {
    let target = chat_target(&state, id, user.id).await?;
    if let Some((other, opened_by)) = target.dm
        && !crate::dm::can_write(&state.inner.db, user.id, other, Some(opened_by)).await?
    {
        return Err(crate::dm::not_allowed());
    }
    state
        .inner
        .chat_limiter
        .check(user.id)
        .map_err(|retry_after_secs| AppError::RateLimited { retry_after_secs })?;
    let content = req.content.trim().to_string();
    if content.chars().count() > limits::MESSAGE_MAX {
        return Err(AppError::Validation(format!(
            "сообщение длиннее {} символов",
            limits::MESSAGE_MAX
        )));
    }
    let mut ids = req.attachments;
    ids.sort_unstable();
    ids.dedup();
    if ids.len() > limits::ATTACHMENTS_MAX {
        return Err(AppError::Validation(format!(
            "не больше {} картинок в сообщении",
            limits::ATTACHMENTS_MAX
        )));
    }
    let sticker = req.sticker.filter(|s| !s.is_empty());
    if let Some(s) = &sticker
        && !proto::STICKERS.iter().any(|(id, _)| id == s)
    {
        return Err(AppError::Validation("нет такого стикера".into()));
    }
    if content.is_empty() && ids.is_empty() && sticker.is_none() {
        return Err(AppError::Validation("пустое сообщение".into()));
    }

    let mut tx = state.inner.db.begin().await?;
    let mid = sqlx::query(
        "INSERT INTO messages (channel_id, author_id, content, created_at, sticker) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(user.id)
    .bind(&content)
    .bind(now())
    .bind(&sticker)
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    for aid in &ids {
        let n = sqlx::query(
            "UPDATE attachments SET message_id = ? WHERE id = ? AND uploader_id = ? AND message_id IS NULL",
        )
        .bind(mid)
        .bind(aid)
        .bind(user.id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n != 1 {
            return Err(AppError::BadRequest(
                "вложение не найдено или уже отправлено".into(),
            ));
        }
    }
    tx.commit().await?;
    // своё сообщение — прочитано
    crate::dm::mark_read(&state.inner.db, user.id, id, mid).await?;

    let msg = load_message(&state, id, mid).await?;
    publish(&state, target.audience, ServerEvent::MessageCreated { message: msg.clone() });
    Ok((StatusCode::CREATED, Json(msg)))
}

/// `PUT /messages/{id}/reactions/{name}` — поставить реакцию (повтор — без ошибки).
pub async fn put_reaction(
    State(state): State<AppState>,
    user: AuthUser,
    Path((mid, name)): Path<(i64, String)>,
) -> AppResult<StatusCode> {
    react(&state, user.id, mid, &name, true).await
}

/// `DELETE /messages/{id}/reactions/{name}` — убрать свою реакцию.
pub async fn delete_reaction(
    State(state): State<AppState>,
    user: AuthUser,
    Path((mid, name)): Path<(i64, String)>,
) -> AppResult<StatusCode> {
    react(&state, user.id, mid, &name, false).await
}

async fn react(state: &AppState, user_id: i64, mid: i64, name: &str, on: bool) -> AppResult<StatusCode> {
    if !proto::REACTIONS.iter().any(|(n, _)| *n == name) {
        return Err(AppError::Validation("нет такой реакции".into()));
    }
    let db = &state.inner.db;
    let channel_id: i64 = sqlx::query_scalar("SELECT channel_id FROM messages WHERE id = ? AND deleted_at IS NULL")
        .bind(mid)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)?;
    // доступ к каналу (чужой — NotFound, как и для истории)
    let target = chat_target(state, channel_id, user_id).await?;
    state.inner.chat_limiter.check(user_id).map_err(|retry_after_secs| AppError::RateLimited { retry_after_secs })?;
    let changed = if on {
        sqlx::query("INSERT OR IGNORE INTO message_reactions (message_id, user_id, name, created_at) VALUES (?, ?, ?, ?)")
            .bind(mid)
            .bind(user_id)
            .bind(name)
            .bind(now())
            .execute(db)
            .await?
            .rows_affected()
    } else {
        sqlx::query("DELETE FROM message_reactions WHERE message_id = ? AND user_id = ? AND name = ?")
            .bind(mid)
            .bind(user_id)
            .bind(name)
            .execute(db)
            .await?
            .rows_affected()
    };
    if changed > 0 {
        let reactions = load_reactions(db, &[mid]).await?.remove(&mid).unwrap_or_default();
        publish(state, target.audience, ServerEvent::ReactionsChanged { channel_id, message_id: mid, reactions });
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Тип картинки по сигнатуре (расширению/заголовку клиента не верим).
pub fn sniff_image(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub(crate) fn storage_path(state: &AppState, key: &str) -> std::path::PathBuf {
    state.inner.uploads_dir.join(key)
}

pub(crate) async fn write_atomic(path: &FsPath, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let tmp = path.with_extension("part");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// `POST /uploads` — тело запроса = файл картинки. Ответ — `Attachment` (id указать при отправке сообщения).
pub async fn upload(
    State(state): State<AppState>,
    user: AuthUser,
    body: Result<Bytes, BytesRejection>,
) -> AppResult<(StatusCode, Json<Attachment>)> {
    let too_big = || {
        AppError::Validation(format!(
            "картинка больше {} МБ",
            limits::IMAGE_MAX_BYTES / (1024 * 1024)
        ))
    };
    let body = body.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            too_big()
        } else {
            AppError::BadRequest(e.body_text())
        }
    })?;
    state
        .inner
        .upload_limiter
        .check(user.id)
        .map_err(|retry_after_secs| AppError::RateLimited { retry_after_secs })?;
    if body.len() > limits::IMAGE_MAX_BYTES {
        return Err(too_big());
    }
    let mime = sniff_image(&body).ok_or_else(|| {
        AppError::Validation("поддерживаются только PNG, JPEG, GIF и WebP".into())
    })?;
    let dim = imagesize::blob_size(&body)
        .map_err(|_| AppError::Validation("картинка повреждена".into()))?;
    let (w, h) = (dim.width as u64, dim.height as u64);
    let side = limits::IMAGE_MAX_SIDE as u64;
    if w == 0 || h == 0 || w > side || h > side {
        return Err(AppError::Validation(format!(
            "размер картинки — до {side}×{side}"
        )));
    }
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM attachments WHERE uploader_id = ? AND message_id IS NULL",
    )
    .bind(user.id)
    .fetch_one(&state.inner.db)
    .await?;
    if pending >= PENDING_MAX {
        return Err(AppError::Validation(
            "слишком много неотправленных картинок".into(),
        ));
    }

    let key = security::new_session_token();
    write_atomic(&storage_path(&state, &key), &body)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("запись файла: {e}")))?;
    let id = sqlx::query(
        "INSERT INTO attachments (uploader_id, kind, mime, size, width, height, storage_key, created_at)
         VALUES (?, 'image', ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.id)
    .bind(mime)
    .bind(body.len() as i64)
    .bind(w as i64)
    .bind(h as i64)
    .bind(&key)
    .bind(now())
    .execute(&state.inner.db)
    .await?
    .last_insert_rowid();
    tracing::info!(user_id = user.id, id, mime, size = body.len(), "image uploaded");
    Ok((
        StatusCode::CREATED,
        Json(Attachment {
            id,
            mime: mime.into(),
            size: body.len() as u64,
            width: w as u32,
            height: h as u32,
        }),
    ))
}

/// `GET /attachments/{id}` — файл (только с авторизацией). Неотправленный виден лишь загрузившему.
pub async fn get_attachment(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let row: Option<(String, String, Option<i64>, i64)> = sqlx::query_as(
        "SELECT mime, storage_key, message_id, uploader_id FROM attachments WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.inner.db)
    .await?;
    let (mime, key, message_id, uploader) = row.ok_or(AppError::NotFound)?;
    match message_id {
        // не отправлено — видит только загрузивший
        None if uploader != user.id => return Err(AppError::NotFound),
        None => {}
        // отправлено — только участникам сервера, где канал сообщения (или двоим в ЛС)
        Some(mid) => {
            let channel_id: i64 = sqlx::query_scalar("SELECT channel_id FROM messages WHERE id = ?")
                .bind(mid)
                .fetch_one(&state.inner.db)
                .await?;
            chat_target(&state, channel_id, user.id).await?;
        }
    }
    let bytes = match tokio::fs::read(storage_path(&state, &key)).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(AppError::NotFound),
        Err(e) => return Err(AppError::Internal(anyhow::anyhow!("чтение файла: {e}"))),
    };
    let mut resp = bytes.into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(resp)
}

/// Удалить с диска файлы по ключам (после удаления строк из БД).
pub async fn remove_files(state: &AppState, keys: &[String]) {
    for k in keys {
        if let Err(e) = tokio::fs::remove_file(storage_path(state, k)).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(key = %k, error = %e, "не удалось удалить файл вложения");
        }
    }
}

/// Убрать загруженные, но не отправленные картинки старше `PENDING_TTL_SECS`.
pub async fn cleanup_pending(state: &AppState) -> anyhow::Result<usize> {
    let keys: Vec<String> = sqlx::query_scalar(
        "DELETE FROM attachments WHERE message_id IS NULL AND created_at < ? RETURNING storage_key",
    )
    .bind(now() - PENDING_TTL_SECS)
    .fetch_all(&state.inner.db)
    .await?;
    remove_files(state, &keys).await;
    Ok(keys.len())
}

/// Фоновая уборка раз в 10 минут.
pub fn spawn_janitor(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(600));
        loop {
            tick.tick().await;
            match cleanup_pending(&state).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(n, "removed stale uploads"),
                Err(e) => tracing::warn!(error = ?e, "cleanup of uploads failed"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::sniff_image;

    #[test]
    fn sniff() {
        assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_image(b"GIF89a.."), Some("image/gif"));
        assert_eq!(sniff_image(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_image(b"<svg"), None);
        assert_eq!(sniff_image(b""), None);
    }
}
