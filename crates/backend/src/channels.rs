//! Каналы и категории сервера: раскладка, создание, правка, порядок; вход в голос.

use std::collections::HashSet;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use livekit_api::access_token::{AccessToken, VideoGrants};
use proto::{
    Category, CategoryRequest, Channel, ChannelKind, ChannelLayout, CreateChannelRequest, JoinResponse, LayoutRequest,
    ServerEvent, UpdateChannelRequest, limits, perm,
};

use crate::{
    AppState, LIVEKIT_TOKEN_TTL,
    db::ChannelRow,
    error::{AppError, AppResult},
    events::{Audience, publish},
    extract::{ApiJson, AuthUser},
    now,
    perm::{self as p},
    servers::{self, validate_name},
    voice,
};

pub(crate) async fn layout_of(db: &sqlx::SqlitePool, server_id: i64) -> AppResult<ChannelLayout> {
    let rows: Vec<ChannelRow> = sqlx::query_as(
        "SELECT id, name, kind, server_id, category_id, position, user_limit FROM channels WHERE server_id = ? ORDER BY position, id",
    )
    .bind(server_id)
    .fetch_all(db)
    .await?;
    let cats: Vec<(i64, String, i32)> =
        sqlx::query_as("SELECT id, name, position FROM channel_categories WHERE server_id = ? ORDER BY position, id")
            .bind(server_id)
            .fetch_all(db)
            .await?;
    Ok(ChannelLayout {
        categories: cats.into_iter().map(|(id, name, position)| Category { id, name, position }).collect(),
        channels: rows.into_iter().map(Channel::try_from).collect::<AppResult<_>>()?,
    })
}

async fn publish_layout(state: &AppState, server_id: i64) -> AppResult<ChannelLayout> {
    let layout = layout_of(&state.inner.db, server_id).await?;
    publish(state, Audience::Server(server_id), ServerEvent::LayoutChanged { server_id, layout: layout.clone() });
    Ok(layout)
}

fn check_limit(kind: ChannelKind, limit: Option<u32>) -> AppResult<()> {
    match limit {
        None => Ok(()),
        Some(_) if kind != ChannelKind::Voice => Err(AppError::Validation("лимит участников — только у голосовых каналов".into())),
        Some(n) if !(1..=limits::USER_LIMIT_MAX).contains(&n) => {
            Err(AppError::Validation(format!("лимит: от 1 до {}", limits::USER_LIMIT_MAX)))
        }
        Some(_) => Ok(()),
    }
}

async fn check_category(db: &sqlx::SqlitePool, server_id: i64, cat: Option<i64>) -> AppResult<()> {
    if let Some(c) = cat {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channel_categories WHERE id = ? AND server_id = ?")
            .bind(c)
            .bind(server_id)
            .fetch_one(db)
            .await?;
        if n == 0 {
            return Err(AppError::BadRequest("нет такой категории на сервере".into()));
        }
    }
    Ok(())
}

/// `GET /servers/{id}/channels`
pub async fn list(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> AppResult<Json<ChannelLayout>> {
    p::member(&state.inner.db, id, user.id).await?;
    Ok(Json(layout_of(&state.inner.db, id).await?))
}

async fn insert_channel(state: &AppState, server_id: i64, req: CreateChannelRequest) -> AppResult<Channel> {
    let name = validate_name(&req.name, limits::CHANNEL_NAME_MAX, "название")?;
    check_limit(req.kind, req.user_limit)?;
    check_category(&state.inner.db, server_id, req.category_id).await?;
    let id = sqlx::query(
        "INSERT INTO channels (name, kind, created_at, server_id, category_id, user_limit, position)
         VALUES (?, ?, ?, ?, ?, ?, (SELECT COALESCE(MAX(position) + 1, 0) FROM channels WHERE server_id = ?))",
    )
    .bind(&name)
    .bind(req.kind.as_str())
    .bind(now())
    .bind(server_id)
    .bind(req.category_id)
    .bind(req.user_limit.map(i64::from))
    .bind(server_id)
    .execute(&state.inner.db)
    .await?
    .last_insert_rowid();
    publish_layout(state, server_id).await?;
    channel(state, id).await
}

async fn channel(state: &AppState, id: i64) -> AppResult<Channel> {
    let row: ChannelRow = sqlx::query_as(crate::db::CHANNEL_SELECT_BY_ID).bind(id).fetch_one(&state.inner.db).await?;
    Channel::try_from(row)
}

/// `POST /servers/{id}/channels`
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    ApiJson(req): ApiJson<CreateChannelRequest>,
) -> AppResult<(StatusCode, Json<Channel>)> {
    p::member(&state.inner.db, id, user.id).await?.require(perm::MANAGE_CHANNELS)?;
    Ok((StatusCode::CREATED, Json(insert_channel(&state, id, req).await?)))
}

/// `PATCH /channels/{id}`
pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    ApiJson(req): ApiJson<UpdateChannelRequest>,
) -> AppResult<Json<Channel>> {
    let (m, row) = p::channel_member(&state.inner.db, id, user.id).await?;
    m.require(perm::MANAGE_CHANNELS)?;
    let kind = ChannelKind::parse(&row.kind).unwrap_or(ChannelKind::Text);
    // Сначала проверки, потом запись — без частичного применения.
    let name = req.name.map(|n| validate_name(&n, limits::CHANNEL_NAME_MAX, "название")).transpose()?;
    if let Some(limit) = req.user_limit {
        check_limit(kind, limit)?;
    }
    if let Some(name) = name {
        sqlx::query("UPDATE channels SET name = ? WHERE id = ?").bind(name).bind(id).execute(&state.inner.db).await?;
    }
    if let Some(limit) = req.user_limit {
        sqlx::query("UPDATE channels SET user_limit = ? WHERE id = ?")
            .bind(limit.map(i64::from))
            .bind(id)
            .execute(&state.inner.db)
            .await?;
    }
    publish_layout(&state, m.server_id).await?;
    Ok(Json(channel(&state, id).await?))
}

/// `DELETE /channels/{id}` — файлы вложений удаляются, голосовая комната очищается.
pub async fn delete(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> AppResult<StatusCode> {
    let (m, row) = p::channel_member(&state.inner.db, id, user.id).await?;
    m.require(perm::MANAGE_CHANNELS)?;
    remove_channel(&state, m.server_id, &row).await?;
    publish_layout(&state, m.server_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_channel(state: &AppState, server_id: i64, row: &ChannelRow) -> AppResult<()> {
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT a.storage_key FROM attachments a JOIN messages m ON m.id = a.message_id WHERE m.channel_id = ?",
    )
    .bind(row.id)
    .fetch_all(&state.inner.db)
    .await?;
    if row.kind == "voice" {
        voice::clear_room(state, &voice::room_name(server_id, row.id)).await;
    }
    sqlx::query("DELETE FROM channels WHERE id = ?").bind(row.id).execute(&state.inner.db).await?;
    crate::chat::remove_files(state, &keys).await;
    Ok(())
}

/// `POST /servers/{id}/categories`
pub async fn create_category(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    ApiJson(req): ApiJson<CategoryRequest>,
) -> AppResult<(StatusCode, Json<Category>)> {
    p::member(&state.inner.db, id, user.id).await?.require(perm::MANAGE_CHANNELS)?;
    let name = validate_name(&req.name, limits::CATEGORY_NAME_MAX, "название категории")?;
    let (cid, position): (i64, i32) = sqlx::query_as(
        "INSERT INTO channel_categories (server_id, name, position)
         VALUES (?, ?, (SELECT COALESCE(MAX(position) + 1, 0) FROM channel_categories WHERE server_id = ?))
         RETURNING id, position",
    )
    .bind(id)
    .bind(&name)
    .bind(id)
    .fetch_one(&state.inner.db)
    .await?;
    publish_layout(&state, id).await?;
    Ok((StatusCode::CREATED, Json(Category { id: cid, name, position })))
}

async fn category_server(state: &AppState, cat: i64, user: &AuthUser) -> AppResult<i64> {
    let sid: Option<i64> = sqlx::query_scalar("SELECT server_id FROM channel_categories WHERE id = ?")
        .bind(cat)
        .fetch_optional(&state.inner.db)
        .await?;
    let sid = sid.ok_or(AppError::NotFound)?;
    p::member(&state.inner.db, sid, user.id).await?.require(perm::MANAGE_CHANNELS)?;
    Ok(sid)
}

/// `PATCH /categories/{id}`
pub async fn rename_category(
    State(state): State<AppState>,
    user: AuthUser,
    Path(cat): Path<i64>,
    ApiJson(req): ApiJson<CategoryRequest>,
) -> AppResult<Json<ChannelLayout>> {
    let sid = category_server(&state, cat, &user).await?;
    let name = validate_name(&req.name, limits::CATEGORY_NAME_MAX, "название категории")?;
    sqlx::query("UPDATE channel_categories SET name = ? WHERE id = ?").bind(name).bind(cat).execute(&state.inner.db).await?;
    Ok(Json(publish_layout(&state, sid).await?))
}

/// `DELETE /categories/{id}` — каналы остаются, без категории.
pub async fn delete_category(State(state): State<AppState>, user: AuthUser, Path(cat): Path<i64>) -> AppResult<StatusCode> {
    let sid = category_server(&state, cat, &user).await?;
    sqlx::query("DELETE FROM channel_categories WHERE id = ?").bind(cat).execute(&state.inner.db).await?;
    publish_layout(&state, sid).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /servers/{id}/layout` — новый порядок: все категории и все каналы сервера, ровно по разу.
pub async fn put_layout(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    ApiJson(req): ApiJson<LayoutRequest>,
) -> AppResult<Json<ChannelLayout>> {
    p::member(&state.inner.db, id, user.id).await?.require(perm::MANAGE_CHANNELS)?;
    let current = layout_of(&state.inner.db, id).await?;
    let cats: HashSet<i64> = current.categories.iter().map(|c| c.id).collect();
    let chans: HashSet<i64> = current.channels.iter().map(|c| c.id).collect();
    let req_cats: HashSet<i64> = req.categories.iter().copied().collect();
    let req_chans: HashSet<i64> = req.channels.iter().map(|c| c.id).collect();
    let bad = req_cats != cats
        || req_cats.len() != req.categories.len()
        || req_chans != chans
        || req_chans.len() != req.channels.len()
        || req.channels.iter().any(|c| c.category_id.is_some_and(|k| !cats.contains(&k)));
    if bad {
        return Err(AppError::BadRequest("раскладка устарела или чужая — обновите список каналов".into()));
    }
    let mut tx = state.inner.db.begin().await?;
    for (pos, cid) in req.categories.iter().enumerate() {
        sqlx::query("UPDATE channel_categories SET position = ? WHERE id = ?").bind(pos as i64).bind(cid).execute(&mut *tx).await?;
    }
    for (pos, c) in req.channels.iter().enumerate() {
        sqlx::query("UPDATE channels SET position = ?, category_id = ? WHERE id = ?")
            .bind(pos as i64)
            .bind(c.category_id)
            .bind(c.id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(Json(publish_layout(&state, id).await?))
}

/// `POST /channels/{id}/join` — токен LiveKit для голосового канала.
pub async fn join(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> AppResult<Json<JoinResponse>> {
    let (m, row) = p::channel_member(&state.inner.db, id, user.id).await?;
    if row.kind != "voice" {
        return Err(AppError::BadRequest("в текстовый канал нельзя зайти голосом".into()));
    }
    let room = voice::room_name(m.server_id, row.id);
    if let Some(limit) = row.user_limit {
        let roster = state.inner.presence.roster(&room);
        if !roster.contains(&user.id) && roster.len() as i64 >= limit {
            return Err(AppError::ChannelFull);
        }
    }
    let me = servers::public_user(&state.inner.db, user.id).await?;
    let lk = &state.inner.livekit;
    let grants = VideoGrants {
        room_join: true,
        room: room.clone(),
        can_publish: Some(true),
        can_subscribe: Some(true),
        can_publish_data: Some(true),
        can_publish_sources: ["microphone", "camera", "screen_share", "screen_share_audio"].map(String::from).to_vec(),
        // имя задаёт только сервер (из токена) — участник не может переименоваться
        can_update_own_metadata: Some(false),
        ..Default::default()
    };
    let token = AccessToken::with_api_key(&lk.api_key, &lk.api_secret)
        .with_identity(&user.id.to_string())
        .with_name(me.shown_name())
        .with_grants(grants)
        .with_ttl(LIVEKIT_TOKEN_TTL)
        .to_jwt()?;
    tracing::info!(user_id = user.id, %room, "livekit token issued");
    Ok(Json(JoinResponse { livekit_url: lk.public_url.clone(), livekit_token: token, room }))
}

/// `POST /channels/{id}/summon` — позвать участника сервера в голосовой канал (ему приходит `summon`).
pub async fn summon(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    ApiJson(req): ApiJson<proto::SummonRequest>,
) -> AppResult<StatusCode> {
    state.inner.chat_limiter.check(user.id).map_err(|retry_after_secs| AppError::RateLimited { retry_after_secs })?;
    let (m, row) = p::channel_member(&state.inner.db, id, user.id).await?;
    if row.kind != "voice" {
        return Err(AppError::BadRequest("позвать можно только в голосовой канал".into()));
    }
    if req.user_id == user.id {
        return Err(AppError::BadRequest("нельзя позвать самого себя".into()));
    }
    // цель — тоже участник сервера (иначе 404, не раскрываем)
    p::member(&state.inner.db, m.server_id, req.user_id).await?;
    let from = servers::public_user(&state.inner.db, user.id).await?;
    tracing::info!(from = user.id, to = req.user_id, channel_id = id, "summon");
    publish(&state, crate::events::Audience::User(req.user_id), ServerEvent::Summon { from, server_id: m.server_id, channel_id: id });
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- совместимость 0.2.x (сервер №1)

/// `GET /channels` (0.2.x): каналы сервера №1, если ты его участник.
pub async fn legacy_list(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Vec<Channel>>> {
    if p::member(&state.inner.db, 1, user.id).await.is_err() {
        return Ok(Json(Vec::new()));
    }
    Ok(Json(layout_of(&state.inner.db, 1).await?.channels))
}

/// `POST /channels` (0.2.x): канал на сервере №1.
pub async fn legacy_create(
    State(state): State<AppState>,
    user: AuthUser,
    ApiJson(req): ApiJson<CreateChannelRequest>,
) -> AppResult<(StatusCode, Json<Channel>)> {
    p::member(&state.inner.db, 1, user.id).await.map_err(|_| AppError::Forbidden)?.require(perm::MANAGE_CHANNELS)?;
    Ok((StatusCode::CREATED, Json(insert_channel(&state, 1, req).await?)))
}
