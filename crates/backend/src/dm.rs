//! Личные сообщения (ЛС), непрочитанные и звонки в ЛС.
//!
//! Канал ЛС — строка `channels` без сервера + пара людей в `dm_channels`; история и отправка — общие с чатом
//! (`/channels/{id}/messages`). Писать можно другу, а человеку с общего сервера — если он это разрешил
//! (`users.dm_from_servers`) или сам начал диалог. Звонок — комната LiveKit `dm-{channel_id}`.

use std::{collections::HashMap, sync::Mutex, time::Duration};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use livekit_api::access_token::{AccessToken, VideoGrants};
use proto::{CallInfo, DmChannel, JoinResponse, OpenDmRequest, PublicUser, ReadRequest, ServerEvent, UnreadChannel, limits};

use crate::{
    AppState, LIVEKIT_TOKEN_TTL,
    error::{AppError, AppResult},
    events::{Audience, publish},
    extract::{ApiJson, AuthUser},
    now, voice,
};

/// Имя комнаты звонка.
pub fn dm_room(channel_id: i64) -> String {
    format!("dm-{channel_id}")
}

/// `dm-7` → 7.
pub fn parse_dm_room(name: &str) -> Option<i64> {
    name.strip_prefix("dm-")?.parse().ok()
}

/// Канал ЛС, где ты участник: (собеседник, кто открыл). Не ЛС — None; чужой ЛС — NotFound.
pub(crate) async fn dm_member(db: &sqlx::SqlitePool, channel_id: i64, me: i64) -> AppResult<Option<(i64, i64)>> {
    let row: Option<(i64, i64, i64)> = sqlx::query_as("SELECT user_a, user_b, opened_by FROM dm_channels WHERE channel_id = ?")
        .bind(channel_id)
        .fetch_optional(db)
        .await?;
    match row {
        None => Ok(None),
        Some((a, b, opened)) if a == me => Ok(Some((b, opened))),
        Some((a, b, opened)) if b == me => Ok(Some((a, opened))),
        Some(_) => Err(AppError::NotFound),
    }
}

/// Можно ли мне писать человеку. `opened_by` — кто начал уже существующий диалог.
pub(crate) async fn can_write(db: &sqlx::SqlitePool, me: i64, other: i64, opened_by: Option<i64>) -> AppResult<bool> {
    let friends: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM friendships WHERE status = 'accepted'
         AND ((requester_id = ?1 AND addressee_id = ?2) OR (requester_id = ?2 AND addressee_id = ?1))",
    )
    .bind(me)
    .bind(other)
    .fetch_one(db)
    .await?;
    if friends > 0 {
        return Ok(true);
    }
    let shared: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM server_members a JOIN server_members b ON a.server_id = b.server_id WHERE a.user_id = ? AND b.user_id = ?",
    )
    .bind(me)
    .bind(other)
    .fetch_one(db)
    .await?;
    if shared == 0 {
        return Ok(false);
    }
    let open: Option<bool> = sqlx::query_scalar("SELECT dm_from_servers FROM users WHERE id = ?").bind(other).fetch_optional(db).await?;
    Ok(open.unwrap_or(false) || opened_by == Some(other))
}

pub(crate) fn not_allowed() -> AppError {
    AppError::Validation("писать можно только друзьям (или если человек разрешил ЛС от людей с общих серверов)".into())
}

async fn dm_view(state: &AppState, channel_id: i64, me: i64) -> AppResult<DmChannel> {
    type Row = (i64, String, Option<String>, Option<String>, Option<i64>, i64);
    let (uid, name, display_name, avatar, last_id, last_at): Row = sqlx::query_as(
        "SELECT u.id, u.name, u.display_name, u.avatar_key,
                (SELECT MAX(id) FROM messages WHERE channel_id = d.channel_id AND deleted_at IS NULL),
                COALESCE((SELECT MAX(created_at) FROM messages WHERE channel_id = d.channel_id), d.created_at)
         FROM dm_channels d JOIN users u ON u.id = CASE WHEN d.user_a = ?2 THEN d.user_b ELSE d.user_a END
         WHERE d.channel_id = ?1 AND (d.user_a = ?2 OR d.user_b = ?2)",
    )
    .bind(channel_id)
    .bind(me)
    .fetch_optional(&state.inner.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(DmChannel {
        id: channel_id,
        user: PublicUser { id: uid, name, display_name, avatar },
        last_message_id: last_id,
        last_at,
        call: call_info(state, channel_id),
    })
}

/// `GET /dms` — мои диалоги: с сообщениями, открытые мной или со звонком; свежие сверху.
pub async fn list(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Vec<DmChannel>>> {
    let rows: Vec<(i64, bool)> = sqlx::query_as(
        "SELECT d.channel_id, d.opened_by = ?1 OR EXISTS (SELECT 1 FROM messages m WHERE m.channel_id = d.channel_id)
         FROM dm_channels d WHERE d.user_a = ?1 OR d.user_b = ?1",
    )
    .bind(user.id)
    .fetch_all(&state.inner.db)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (id, visible) in rows {
        // пустой чужой диалог — только если в нём звонят
        if visible || call_info(&state, id).is_some() {
            out.push(dm_view(&state, id, user.id).await?);
        }
    }
    out.sort_by(|a, b| b.last_at.cmp(&a.last_at).then(b.id.cmp(&a.id)));
    Ok(Json(out))
}

/// `POST /dms` — открыть диалог с человеком (или вернуть существующий).
pub async fn open(State(state): State<AppState>, user: AuthUser, ApiJson(req): ApiJson<OpenDmRequest>) -> AppResult<(StatusCode, Json<DmChannel>)> {
    let other = req.user_id;
    if other == user.id {
        return Err(AppError::Validation("нельзя написать самому себе".into()));
    }
    let db = &state.inner.db;
    let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE id = ?").bind(other).fetch_optional(db).await?;
    if exists.is_none() {
        return Err(AppError::NotFound);
    }
    let (a, b) = (user.id.min(other), user.id.max(other));
    let found: Option<i64> = sqlx::query_scalar("SELECT channel_id FROM dm_channels WHERE user_a = ? AND user_b = ?")
        .bind(a)
        .bind(b)
        .fetch_optional(db)
        .await?;
    if let Some(id) = found {
        // уже есть: открыть могу, даже если писать нельзя (история остаётся)
        return Ok((StatusCode::OK, Json(dm_view(&state, id, user.id).await?)));
    }
    if !can_write(db, user.id, other, None).await? {
        return Err(not_allowed());
    }
    let mut tx = db.begin().await?;
    let id = sqlx::query("INSERT INTO channels (name, kind, created_at) VALUES ('', 'text', ?)")
        .bind(now())
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
    let inserted = sqlx::query("INSERT INTO dm_channels (channel_id, user_a, user_b, opened_by, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(id)
        .bind(a)
        .bind(b)
        .bind(user.id)
        .bind(now())
        .execute(&mut *tx)
        .await;
    match inserted {
        Ok(_) => tx.commit().await?,
        // Одновременный запрос уже создал этот диалог — отдаём его.
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            drop(tx);
            let existing: i64 = sqlx::query_scalar("SELECT channel_id FROM dm_channels WHERE user_a = ? AND user_b = ?")
                .bind(a)
                .bind(b)
                .fetch_one(db)
                .await?;
            return Ok((StatusCode::OK, Json(dm_view(&state, existing, user.id).await?)));
        }
        Err(e) => return Err(e.into()),
    }
    tracing::info!(channel_id = id, from = user.id, to = other, "dm opened");
    Ok((StatusCode::CREATED, Json(dm_view(&state, id, user.id).await?)))
}

// ---------------------------------------------------------------- непрочитанные

/// `GET /unread` — каналы (текстовые моих серверов и ЛС), где есть непрочитанные чужие сообщения.
pub async fn unread(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Vec<UnreadChannel>>> {
    type Row = (i64, Option<i64>, i64, i64);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT c.id, c.server_id, COUNT(m.id), MAX(m.id)
         FROM channels c
         JOIN server_members sm ON sm.server_id = c.server_id AND sm.user_id = ?1
         JOIN messages m ON m.channel_id = c.id AND m.deleted_at IS NULL AND m.author_id <> ?1
         LEFT JOIN read_states rs ON rs.user_id = ?1 AND rs.channel_id = c.id
         WHERE c.kind = 'text' AND m.id > COALESCE(rs.last_read_id, 0)
           AND (rs.last_read_id IS NOT NULL OR m.created_at >= sm.joined_at)
         GROUP BY c.id
         UNION ALL
         SELECT d.channel_id, NULL, COUNT(m.id), MAX(m.id)
         FROM dm_channels d
         JOIN messages m ON m.channel_id = d.channel_id AND m.deleted_at IS NULL AND m.author_id <> ?1
         LEFT JOIN read_states rs ON rs.user_id = ?1 AND rs.channel_id = d.channel_id
         WHERE (d.user_a = ?1 OR d.user_b = ?1) AND m.id > COALESCE(rs.last_read_id, 0)
         GROUP BY d.channel_id",
    )
    .bind(user.id)
    .fetch_all(&state.inner.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(channel_id, server_id, count, last)| UnreadChannel {
                channel_id,
                server_id,
                count: count.clamp(0, u32::MAX as i64) as u32,
                last_message_id: last,
            })
            .collect(),
    ))
}

/// Отметить прочитанным до `message_id` (только вперёд).
pub(crate) async fn mark_read(db: &sqlx::SqlitePool, user: i64, channel_id: i64, message_id: i64) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO read_states (user_id, channel_id, last_read_id) VALUES (?, ?, ?)
         ON CONFLICT (user_id, channel_id) DO UPDATE SET last_read_id = MAX(last_read_id, excluded.last_read_id)",
    )
    .bind(user)
    .bind(channel_id)
    .bind(message_id)
    .execute(db)
    .await?;
    Ok(())
}

/// `POST /channels/{id}/read` — прочитано до сообщения включительно. Остальные мои клиенты получают `read_state`.
pub async fn read(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>, ApiJson(req): ApiJson<ReadRequest>) -> AppResult<StatusCode> {
    crate::chat::chat_target(&state, id, user.id).await?;
    let last: Option<i64> = sqlx::query_scalar("SELECT MAX(id) FROM messages WHERE channel_id = ?").bind(id).fetch_one(&state.inner.db).await?;
    let upto = req.message_id.min(last.unwrap_or(0));
    if upto <= 0 {
        return Ok(StatusCode::NO_CONTENT);
    }
    mark_read(&state.inner.db, user.id, id, upto).await?;
    let stored: i64 = sqlx::query_scalar("SELECT last_read_id FROM read_states WHERE user_id = ? AND channel_id = ?")
        .bind(user.id)
        .bind(id)
        .fetch_one(&state.inner.db)
        .await?;
    publish(&state, Audience::User(user.id), ServerEvent::ReadState { channel_id: id, last_read_id: stored });
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- звонки

struct Ring {
    caller: i64,
    started_at: i64,
    /// Второй ответил (зашёл в звонок).
    answered: bool,
}

/// Звонки в ЛС: кто позвонил и ответили ли. Кто в комнате — `voice::Presence`.
#[derive(Default)]
pub struct Calls(Mutex<HashMap<i64, Ring>>);

impl Calls {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Ring>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Текущее состояние звонка в канале ЛС.
pub fn call_info(state: &AppState, channel_id: i64) -> Option<CallInfo> {
    let participants = state.inner.presence.roster(&dm_room(channel_id));
    let calls = state.inner.calls.lock();
    match calls.get(&channel_id) {
        Some(r) => Some(CallInfo { channel_id, caller_id: r.caller, started_at: r.started_at, ringing: !r.answered, participants }),
        None if !participants.is_empty() => {
            Some(CallInfo { channel_id, caller_id: participants[0], started_at: 0, ringing: false, participants })
        }
        None => None,
    }
}

async fn pair_of(db: &sqlx::SqlitePool, channel_id: i64) -> AppResult<Option<(i64, i64)>> {
    Ok(sqlx::query_as("SELECT user_a, user_b FROM dm_channels WHERE channel_id = ?").bind(channel_id).fetch_optional(db).await?)
}

/// Разослать состояние звонка обоим.
pub async fn publish_call(state: &AppState, channel_id: i64) {
    match pair_of(&state.inner.db, channel_id).await {
        Ok(Some((a, b))) => {
            publish(state, Audience::Users(vec![a, b]), ServerEvent::Call { channel_id, call: call_info(state, channel_id) });
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(channel_id, error = ?e, "звонок: не удалось найти пару"),
    }
}

/// Состав комнаты звонка изменился (вебхук, сверка): ответ, конец звонка.
pub fn room_changed(state: &AppState, channel_id: i64) {
    let roster = state.inner.presence.roster(&dm_room(channel_id));
    {
        let mut calls = state.inner.calls.lock();
        if roster.is_empty() {
            calls.remove(&channel_id);
        } else if let Some(r) = calls.get_mut(&channel_id)
            && roster.iter().any(|u| *u != r.caller)
        {
            r.answered = true;
        }
    }
    let state = state.clone();
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        rt.spawn(async move { publish_call(&state, channel_id).await });
    }
}

/// Канал ЛС, где я участник и могу писать (звонить); иначе ошибка.
async fn callable(state: &AppState, channel_id: i64, me: i64) -> AppResult<()> {
    let (other, opened_by) = dm_member(&state.inner.db, channel_id, me).await?.ok_or(AppError::NotFound)?;
    if !can_write(&state.inner.db, me, other, Some(opened_by)).await? {
        return Err(not_allowed());
    }
    Ok(())
}

/// `POST /dms/{id}/call` — позвонить или ответить на звонок: токен LiveKit комнаты звонка.
pub async fn call(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> AppResult<Json<JoinResponse>> {
    callable(&state, id, user.id).await?;
    let room = dm_room(id);
    let roster = state.inner.presence.roster(&room);
    let new_ring = {
        let mut calls = state.inner.calls.lock();
        match calls.get_mut(&id) {
            Some(r) => {
                if r.caller != user.id {
                    r.answered = true;
                }
                None
            }
            None if roster.iter().any(|u| *u != user.id) => {
                // звонок уже идёт без записи (сверка после перезапуска) — просто заходим
                calls.insert(id, Ring { caller: roster[0], started_at: now(), answered: true });
                None
            }
            None => {
                let started_at = now();
                calls.insert(id, Ring { caller: user.id, started_at, answered: false });
                Some(started_at)
            }
        }
    };
    if let Some(started_at) = new_ring {
        tracing::info!(channel_id = id, caller = user.id, "dm call ringing");
        let st = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(limits::CALL_RING_SECS)).await;
            let expired = {
                let mut calls = st.inner.calls.lock();
                let stale = calls.get(&id).is_some_and(|r| r.started_at == started_at && !r.answered);
                if stale {
                    calls.remove(&id);
                }
                stale
            };
            if expired {
                publish_call(&st, id).await;
            }
        });
    }
    publish_call(&state, id).await;

    let me = crate::servers::public_user(&state.inner.db, user.id).await?;
    let lk = &state.inner.livekit;
    let grants = VideoGrants {
        room_join: true,
        room: room.clone(),
        can_publish: Some(true),
        can_subscribe: Some(true),
        can_publish_data: Some(true),
        can_publish_sources: ["microphone", "screen_share", "screen_share_audio"].map(String::from).to_vec(),
        can_update_own_metadata: Some(false),
        ..Default::default()
    };
    let token = AccessToken::with_api_key(&lk.api_key, &lk.api_secret)
        .with_identity(&user.id.to_string())
        .with_name(me.shown_name())
        .with_grants(grants)
        .with_ttl(LIVEKIT_TOKEN_TTL)
        .to_jwt()?;
    Ok(Json(JoinResponse { livekit_url: lk.public_url.clone(), livekit_token: token, room }))
}

/// `DELETE /dms/{id}/call` — положить трубку: отменить свой звонок, отклонить входящий или выйти из идущего.
pub async fn hang_up(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> AppResult<StatusCode> {
    dm_member(&state.inner.db, id, user.id).await?.ok_or(AppError::NotFound)?;
    {
        let mut calls = state.inner.calls.lock();
        if calls.get(&id).is_some_and(|r| !r.answered) {
            calls.remove(&id);
        }
    }
    // клиент уже вышел из комнаты — не ждём вебхука
    voice::left(&state, &dm_room(id), user.id, None);
    publish_call(&state, id).await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rooms() {
        assert_eq!(dm_room(7), "dm-7");
        assert_eq!(parse_dm_room("dm-7"), Some(7));
        assert_eq!(parse_dm_room("srv-1-ch-7"), None);
        assert_eq!(parse_dm_room("dm-"), None);
    }
}
