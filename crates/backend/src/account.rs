//! Аккаунт: смена пароля, список сеансов, удаление аккаунта.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use proto::{ChangePasswordRequest, DeleteAccountRequest, SessionInfo};

use crate::{
    AppState,
    db,
    error::{AppError, AppResult},
    extract::{ApiJson, AuthUser, ClientIp},
    now,
    routes::{limiter_key, rate_limit},
    security,
};

/// Публичный id сеанса — начало хэша токена (сам токен и полный хэш наружу не идут).
const ID_LEN: usize = 16;

async fn check_password(state: &AppState, user_id: i64, password: String) -> AppResult<()> {
    let hash: Option<String> = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(&state.inner.db)
        .await?;
    let ok = tokio::task::spawn_blocking(move || security::verify_password(&password, hash.as_deref())).await?;
    if ok { Ok(()) } else { Err(AppError::BadCredentials) }
}

/// `GET /auth/sessions` — мои сеансы, свежие сверху.
pub async fn list_sessions(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Vec<SessionInfo>>> {
    let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT token_hash, device, created_at, last_seen FROM sessions
         WHERE user_id = ? AND expires_at > ? ORDER BY MAX(last_seen, created_at) DESC",
    )
    .bind(user.id)
    .bind(now())
    .fetch_all(&state.inner.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(hash, device, created_at, last_seen)| SessionInfo {
                current: hash == user.token_hash,
                id: hash[..ID_LEN].to_string(),
                device,
                created_at,
                last_seen: last_seen.max(created_at),
            })
            .collect(),
    ))
}

/// `DELETE /auth/sessions/{id}` — завершить чужой сеанс (свой — через /auth/logout).
pub async fn revoke_session(State(state): State<AppState>, user: AuthUser, Path(id): Path<String>) -> AppResult<StatusCode> {
    if id.len() != ID_LEN || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AppError::NotFound);
    }
    if user.token_hash.starts_with(&id) {
        return Err(AppError::BadRequest("это текущий сеанс — используйте «Выйти»".into()));
    }
    let n = sqlx::query("DELETE FROM sessions WHERE user_id = ? AND token_hash LIKE ? || '%'")
        .bind(user.id)
        .bind(&id)
        .execute(&state.inner.db)
        .await?
        .rows_affected();
    if n == 0 { Err(AppError::NotFound) } else { Ok(StatusCode::NO_CONTENT) }
}

/// `POST /auth/password` — смена пароля со старым; остальные сеансы закрываются.
pub async fn change_password(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    user: AuthUser,
    ApiJson(req): ApiJson<ChangePasswordRequest>,
) -> AppResult<StatusCode> {
    rate_limit(&state, ip)?;
    check_password(&state, user.id, req.old_password).await?;
    db::validate_password(&req.new_password)?;
    let hash = db::hash_password_async(req.new_password).await?;
    let mut tx = state.inner.db.begin().await?;
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(hash)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE user_id = ? AND token_hash != ?")
        .bind(user.id)
        .bind(&user.token_hash)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    state.inner.auth_limiter.refund(limiter_key(ip));
    tracing::info!(user_id = user.id, "password changed");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /auth/delete` — удалить аккаунт по паролю. Сообщения остаются у «Удалённого пользователя»;
/// личные диалоги исчезают у обоих. Владелец сервера и последний администратор удалить аккаунт не могут.
pub async fn delete_account(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    user: AuthUser,
    ApiJson(req): ApiJson<DeleteAccountRequest>,
) -> AppResult<StatusCode> {
    rate_limit(&state, ip)?;
    check_password(&state, user.id, req.password).await?;
    let owned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM servers WHERE owner_id = ?")
        .bind(user.id)
        .fetch_one(&state.inner.db)
        .await?;
    if owned > 0 {
        return Err(AppError::BadRequest("вы владелец сервера: передайте владение или удалите сервер".into()));
    }
    if user.is_admin {
        let admins: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE is_admin = 1")
            .fetch_one(&state.inner.db)
            .await?;
        if admins <= 1 {
            return Err(AppError::BadRequest("вы единственный администратор инстанса".into()));
        }
    }
    let mut tx = state.inner.db.begin().await?;
    sqlx::query("UPDATE messages SET author_id = 0 WHERE author_id = ?").bind(user.id).execute(&mut *tx).await?;
    sqlx::query("UPDATE attachments SET uploader_id = 0 WHERE uploader_id = ?").bind(user.id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM users WHERE id = ?").bind(user.id).execute(&mut *tx).await?;
    tx.commit().await?;
    state.inner.auth_limiter.refund(limiter_key(ip));
    tracing::info!(user_id = user.id, name = %user.name, "account deleted");
    Ok(StatusCode::NO_CONTENT)
}
