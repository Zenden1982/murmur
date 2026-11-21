//! Профиль: отображаемое имя (отдельно от логина) и аватар.

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
};
use proto::{PublicUser, ServerEvent, SetActivityRequest, UpdateMeRequest, User, UserProfile, limits};

use crate::{
    AppState,
    error::{AppError, AppResult},
    events::{Audience, publish},
    extract::{ApiJson, AuthUser},
    images,
};

pub(crate) async fn load_user(db: &sqlx::SqlitePool, id: i64) -> AppResult<User> {
    type Row = (i64, String, bool, Option<String>, Option<String>, bool, Option<String>, Option<String>);
    let (id, name, is_admin, display_name, avatar, dm_from_servers, about, status): Row = sqlx::query_as(
        "SELECT id, name, is_admin, display_name, avatar_key, dm_from_servers, about, status FROM users WHERE id = ?",
    )
    .bind(id)
    .fetch_one(db)
    .await?;
    Ok(User { id, name, is_admin, display_name, avatar, dm_from_servers, about, status })
}

/// Все, кто делит с человеком хоть один сервер (и он сам).
pub(crate) async fn neighbours(db: &sqlx::SqlitePool, id: i64) -> AppResult<Vec<i64>> {
    let mut v: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT b.user_id FROM server_members a JOIN server_members b ON a.server_id = b.server_id WHERE a.user_id = ?",
    )
    .bind(id)
    .fetch_all(db)
    .await?;
    // и друзья — они видят имя и аватар в списке друзей; и собеседники в ЛС
    let dms: Vec<i64> = sqlx::query_scalar(
        "SELECT CASE WHEN user_a = ?1 THEN user_b ELSE user_a END FROM dm_channels WHERE user_a = ?1 OR user_b = ?1",
    )
    .bind(id)
    .fetch_all(db)
    .await?;
    for f in crate::friends::friend_ids(db, id).await?.into_iter().chain(dms) {
        if !v.contains(&f) {
            v.push(f);
        }
    }
    if !v.contains(&id) {
        v.push(id);
    }
    Ok(v)
}

async fn changed(state: &AppState, id: i64) -> AppResult<Json<User>> {
    let u = load_user(&state.inner.db, id).await?;
    let public = PublicUser { id: u.id, name: u.name.clone(), display_name: u.display_name.clone(), avatar: u.avatar.clone() };
    publish(state, Audience::Users(neighbours(&state.inner.db, id).await?), ServerEvent::UserUpdated { user: public });
    Ok(Json(u))
}

/// `GET /me`
pub async fn me(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<User>> {
    Ok(Json(load_user(&state.inner.db, user.id).await?))
}

/// Текст профиля: обрезать, без управляющих символов, до `max` символов; пустой — None.
fn clean_text(raw: &str, max: usize, what: &str, multiline: bool) -> AppResult<Option<String>> {
    let t = raw.trim();
    if t.chars().count() > max || t.chars().any(|c| c.is_control() && !(multiline && c == '\n')) {
        return Err(AppError::Validation(format!("{what}: до {max} символов")));
    }
    Ok((!t.is_empty()).then(|| t.to_string()))
}

/// Активность человека изменилась — друзьям и соседям.
pub(crate) async fn announce_activity(state: &AppState, id: i64) -> AppResult<()> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM users WHERE id = ?").bind(id).fetch_one(&state.inner.db).await?;
    let activity = state.inner.online.activity(id, status);
    let mut ids = neighbours(&state.inner.db, id).await?;
    ids.retain(|&u| u != id);
    publish(state, Audience::Users(ids), ServerEvent::Activity { user_id: id, activity });
    Ok(())
}

/// `PUT /me/activity` — во что играю (клиент сам смотрит процессы; сюда — только название и время начала).
pub async fn put_activity(State(state): State<AppState>, user: AuthUser, ApiJson(req): ApiJson<SetActivityRequest>) -> AppResult<StatusCode> {
    let game = match req.game.as_deref().map(|g| clean_text(g, limits::GAME_NAME_MAX, "название игры", false)).transpose()?.flatten() {
        Some(g) => Some((g, req.since.unwrap_or_else(crate::now))),
        None => None,
    };
    if state.inner.online.set_game(user.id, game) {
        announce_activity(&state, user.id).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /users/{id}` — карточка профиля. Видна тем, кто видит человека (общий сервер, друг, ЛС), иначе 404.
pub async fn profile(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> AppResult<Json<UserProfile>> {
    let db = &state.inner.db;
    if !neighbours(db, user.id).await?.contains(&id) {
        return Err(AppError::NotFound);
    }
    let u = load_user(db, id).await?;
    let is_friend = crate::friends::friend_ids(db, user.id).await?.contains(&id);
    let online = state.inner.online.is_online(id);
    Ok(Json(UserProfile {
        activity: state.inner.online.activity(id, u.status.clone()),
        user: PublicUser { id: u.id, name: u.name, display_name: u.display_name, avatar: u.avatar },
        about: u.about,
        online,
        is_friend,
    }))
}

/// `PATCH /me` — отображаемое имя (пустая строка — сбросить), кто может писать в ЛС, «о себе», свой статус.
pub async fn update(State(state): State<AppState>, user: AuthUser, ApiJson(req): ApiJson<UpdateMeRequest>) -> AppResult<Json<User>> {
    if let Some(raw) = &req.about {
        let about = clean_text(raw, limits::ABOUT_MAX, "о себе", true)?;
        sqlx::query("UPDATE users SET about = ? WHERE id = ?").bind(about).bind(user.id).execute(&state.inner.db).await?;
    }
    if let Some(raw) = &req.status {
        let status = clean_text(raw, limits::STATUS_MAX, "статус", false)?;
        sqlx::query("UPDATE users SET status = ? WHERE id = ?").bind(status).bind(user.id).execute(&state.inner.db).await?;
        announce_activity(&state, user.id).await?;
    }
    let name_changed = req.display_name.is_some();
    if let Some(raw) = req.display_name {
        let name = raw.trim().to_string();
        if name.chars().count() > limits::DISPLAY_NAME_MAX || name.chars().any(char::is_control) {
            return Err(AppError::Validation(format!("отображаемое имя: до {} символов", limits::DISPLAY_NAME_MAX)));
        }
        sqlx::query("UPDATE users SET display_name = ? WHERE id = ?")
            .bind((!name.is_empty()).then_some(name))
            .bind(user.id)
            .execute(&state.inner.db)
            .await?;
    }
    if let Some(open) = req.dm_from_servers {
        sqlx::query("UPDATE users SET dm_from_servers = ? WHERE id = ?").bind(open).bind(user.id).execute(&state.inner.db).await?;
    }
    if !name_changed {
        // имя и аватар не менялись — UserUpdated соседям не нужен
        return Ok(Json(load_user(&state.inner.db, user.id).await?));
    }
    changed(&state, user.id).await
}

async fn set_avatar(state: &AppState, id: i64, key: Option<String>) -> AppResult<()> {
    let old: Option<String> = sqlx::query_scalar("SELECT avatar_key FROM users WHERE id = ?").bind(id).fetch_one(&state.inner.db).await?;
    sqlx::query("UPDATE users SET avatar_key = ? WHERE id = ?").bind(&key).bind(id).execute(&state.inner.db).await?;
    if let Some(old) = old {
        images::remove(state, &old).await;
    }
    Ok(())
}

/// `PUT /me/avatar` — тело: квадратная картинка.
pub async fn put_avatar(State(state): State<AppState>, user: AuthUser, body: Bytes) -> AppResult<Json<User>> {
    let key = images::store_profile_image(&state, &body).await?;
    set_avatar(&state, user.id, Some(key)).await?;
    changed(&state, user.id).await
}

/// `DELETE /me/avatar`
pub async fn delete_avatar(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<User>> {
    set_avatar(&state, user.id, None).await?;
    changed(&state, user.id).await
}
