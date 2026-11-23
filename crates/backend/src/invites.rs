//! Приглашения на сервер: создать, список, отозвать, превью без входа, вступить по коду.
//! Любой код годится и для регистрации нового аккаунта (решение владельца, фаза 9.1).

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use proto::{CreateInviteRequest, Invite, InvitePreview, Server, perm};

use crate::{
    AppState,
    db::{self, InviteRow},
    error::{AppError, AppResult},
    extract::{ApiJson, AuthUser, ClientIp},
    now, perm as p, security, servers,
};

/// `POST /servers/{id}/invites`
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    ApiJson(req): ApiJson<CreateInviteRequest>,
) -> AppResult<(StatusCode, Json<Invite>)> {
    p::member(&state.inner.db, id, user.id).await?.require(perm::INVITE)?;
    let inv = db::create_invite(&state.inner.db, id, user.id, req.uses.unwrap_or(1), req.expires_in_hours.unwrap_or(168)).await?;
    Ok((StatusCode::CREATED, Json(inv)))
}

/// `GET /servers/{id}/invites` — свои; с правом управлять сервером — все.
pub async fn list(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> AppResult<Json<Vec<Invite>>> {
    let me = p::member(&state.inner.db, id, user.id).await?;
    let all = me.require(perm::MANAGE_SERVER).is_ok();
    let rows: Vec<InviteRow> = sqlx::query_as(
        "SELECT code, uses_left, expires_at, revoked, created_by, server_id FROM invites
         WHERE server_id = ? AND (? OR created_by = ?) ORDER BY created_at DESC, code",
    )
    .bind(id)
    .bind(all)
    .bind(user.id)
    .fetch_all(&state.inner.db)
    .await?;
    Ok(Json(rows.into_iter().map(Invite::from).collect()))
}

/// `DELETE /invites/{code}` — создатель или управляющий сервером.
pub async fn revoke(State(state): State<AppState>, user: AuthUser, Path(code): Path<String>) -> AppResult<StatusCode> {
    let code = security::normalize_invite_code(&code);
    let row: Option<(i64, Option<i64>)> = sqlx::query_as("SELECT created_by, server_id FROM invites WHERE code = ?")
        .bind(&code)
        .fetch_optional(&state.inner.db)
        .await?;
    let (created_by, server_id) = row.ok_or(AppError::NotFound)?;
    let server_id = server_id.ok_or(AppError::NotFound)?;
    if created_by != user.id {
        p::member(&state.inner.db, server_id, user.id).await?.require(perm::MANAGE_SERVER)?;
    }
    sqlx::query("UPDATE invites SET revoked = 1 WHERE code = ?").bind(&code).execute(&state.inner.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Действующий инвайт → id сервера.
async fn valid_server(state: &AppState, code: &str) -> AppResult<Option<i64>> {
    Ok(sqlx::query_scalar::<_, Option<i64>>(
        "SELECT server_id FROM invites
         WHERE code = ? AND revoked = 0 AND uses_left > 0 AND (expires_at IS NULL OR expires_at > ?)",
    )
    .bind(code)
    .bind(now())
    .fetch_optional(&state.inner.db)
    .await?
    .flatten())
}

/// `GET /invites/{code}` без входа — куда зовут (лимит попыток как у входа).
pub async fn preview(State(state): State<AppState>, ClientIp(ip): ClientIp, Path(code): Path<String>) -> AppResult<Json<InvitePreview>> {
    state.inner.preview_limiter.check(crate::routes::limiter_key(ip)).map_err(|retry_after_secs| AppError::RateLimited { retry_after_secs })?;
    let sid = valid_server(&state, &security::normalize_invite_code(&code)).await?.ok_or(AppError::NotFound)?;
    let (server_name, server_icon, description, color): (String, Option<String>, Option<String>, Option<String>) =
        sqlx::query_as("SELECT name, icon_key, description, color FROM servers WHERE id = ?").bind(sid).fetch_one(&state.inner.db).await?;
    let members = servers::member_ids(&state.inner.db, sid).await?;
    let online_count = members.iter().filter(|&&u| state.inner.online.is_online(u)).count() as u32;
    Ok(Json(InvitePreview { server_name, server_icon, description, color, member_count: members.len() as u32, online_count }))
}

/// `POST /invites/{code}/join` — вступить на сервер (уже участник — без списания).
pub async fn join(State(state): State<AppState>, user: AuthUser, Path(code): Path<String>) -> AppResult<Json<Server>> {
    let code = security::normalize_invite_code(&code);
    let sid = valid_server(&state, &code).await?.ok_or(AppError::InvalidInvite)?;
    if p::member(&state.inner.db, sid, user.id).await.is_ok() {
        return Ok(Json(servers::server_for(&state.inner.db, sid, user.id).await?));
    }
    if p::is_banned(&state.inner.db, sid, user.id).await? {
        return Err(AppError::Banned);
    }
    let mut tx = state.inner.db.begin().await?;
    let used = sqlx::query(
        "UPDATE invites SET uses_left = uses_left - 1
         WHERE code = ? AND revoked = 0 AND uses_left > 0 AND (expires_at IS NULL OR expires_at > ?)",
    )
    .bind(&code)
    .bind(now())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if used != 1 {
        return Err(AppError::InvalidInvite);
    }
    let role = p::default_role(&mut *tx, sid).await?;
    sqlx::query("INSERT INTO server_members (server_id, user_id, role_id, joined_at) VALUES (?, ?, ?, ?)")
        .bind(sid)
        .bind(user.id)
        .bind(role)
        .bind(now())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(server_id = sid, user_id = user.id, "joined server by invite");
    announce_join(&state, sid, user.id).await?;
    Ok(Json(servers::server_for(&state.inner.db, sid, user.id).await?))
}

/// Новичку — сервер (сначала, чтобы его поток начал получать события сервера), остальным — участник.
pub(crate) async fn announce_join(state: &AppState, server_id: i64, user_id: i64) -> AppResult<()> {
    let server = servers::server_for(&state.inner.db, server_id, user_id).await?;
    crate::events::publish(state, crate::events::Audience::User(user_id), proto::ServerEvent::ServerUpserted { server });
    servers::publish_member(state, server_id, user_id).await
}

// ---------------------------------------------------------------- совместимость 0.2.x (сервер №1)

/// `GET /invites` (0.2.x)
pub async fn legacy_list(state: State<AppState>, user: AuthUser) -> AppResult<Json<Vec<Invite>>> {
    list(state, user, Path(1)).await
}

/// `POST /invites` (0.2.x)
pub async fn legacy_create(
    state: State<AppState>,
    user: AuthUser,
    req: ApiJson<CreateInviteRequest>,
) -> AppResult<(StatusCode, Json<Invite>)> {
    create(state, user, Path(1), req).await
}
