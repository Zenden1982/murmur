//! Картинки профиля: аватары и иконки серверов. Файлы — в `UPLOADS_DIR` со случайным ключом.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};

use crate::{
    AppState,
    chat::{sniff_image, storage_path, write_atomic},
    error::{AppError, AppResult},
    extract::AuthUser,
    security,
};

/// Иконка сервера: квадрат до 512 px (PNG/JPEG/WebP, до 1 МБ); перекодируется без метаданных. Возвращает ключ.
pub async fn store_profile_image(state: &AppState, body: &[u8]) -> AppResult<String> {
    store_clean(state, crate::media::Kind::ServerIcon, body).await
}

/// Баннер сервера: PNG/JPEG/WebP ровно 960×480, до 2 МБ; перекодируется без метаданных. Возвращает ключ.
pub async fn store_banner_image(state: &AppState, body: &[u8]) -> AppResult<String> {
    store_clean(state, crate::media::Kind::ServerBanner, body).await
}

async fn store_clean(state: &AppState, kind: crate::media::Kind, body: &[u8]) -> AppResult<String> {
    let body = body.to_vec();
    let clean = tokio::task::spawn_blocking(move || crate::media::sanitize(kind, &body))
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("проверка картинки: {e}")))?
        .map_err(AppError::Validation)?;
    let key = security::new_session_token();
    write_atomic(&storage_path(state, &key), &clean)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("запись файла: {e}")))?;
    Ok(key)
}

/// Картинка профиля пользователя (аватар/баннер): строгая проверка и очистка (media::sanitize), ключ = sha256 итога.
pub async fn store_profile_media(state: &AppState, kind: crate::media::Kind, body: Bytes) -> AppResult<String> {
    let clean = tokio::task::spawn_blocking(move || crate::media::sanitize(kind, &body))
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("проверка картинки: {e}")))?
        .map_err(AppError::Validation)?;
    use sha2::Digest;
    let key: String = sha2::Sha256::digest(&clean).iter().map(|b| format!("{b:02x}")).collect();
    let path = storage_path(state, &key);
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        write_atomic(&path, &clean).await.map_err(|e| AppError::Internal(anyhow::anyhow!("запись файла: {e}")))?;
    }
    Ok(key)
}

/// Удалить файл картинки, если на него больше никто не ссылается (ключи-хеши бывают общими).
pub async fn release(state: &AppState, key: &str) {
    let used: Result<i64, _> = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM users WHERE avatar_key = ?1 OR banner_key = ?1) + (SELECT COUNT(*) FROM servers WHERE icon_key = ?1 OR banner_key = ?1)",
    )
    .bind(key)
    .fetch_one(&state.inner.db)
    .await;
    match used {
        Ok(0) => remove(state, key).await,
        Ok(_) => {}
        Err(e) => tracing::warn!(key, error = %e, "не удалось проверить ссылки на картинку"),
    }
}

pub async fn remove(state: &AppState, key: &str) {
    crate::chat::remove_files(state, &[key.to_string()]).await;
}

/// Ключ без мусора (64 hex) — защита от выхода из папки.
fn valid_key(k: &str) -> bool {
    !k.is_empty() && k.len() <= 128 && k.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `GET /images/{key}` — аватар или иконка сервера (только то, на что ссылается профиль или сервер).
pub async fn get_image(State(state): State<AppState>, _user: AuthUser, Path(key): Path<String>) -> AppResult<Response> {
    if !valid_key(&key) {
        return Err(AppError::NotFound);
    }
    let known: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM users WHERE avatar_key = ?1 OR banner_key = ?1) + (SELECT COUNT(*) FROM servers WHERE icon_key = ?1 OR banner_key = ?1)",
    )
    .bind(&key)
    .fetch_one(&state.inner.db)
    .await?;
    if known == 0 {
        return Err(AppError::NotFound);
    }
    let bytes = match tokio::fs::read(storage_path(&state, &key)).await {
        Ok(b) => Bytes::from(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(AppError::NotFound),
        Err(e) => return Err(AppError::Internal(anyhow::anyhow!("чтение файла: {e}"))),
    };
    let mime = sniff_image(&bytes).unwrap_or("application/octet-stream");
    let mut resp = bytes.into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=31536000, immutable"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    Ok(resp)
}

#[cfg(test)]
mod tests {
    #[test]
    fn keys() {
        assert!(super::valid_key("abc123"));
        assert!(!super::valid_key("../x"));
        assert!(!super::valid_key(""));
    }
}
