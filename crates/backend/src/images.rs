//! Картинки профиля: аватары и иконки серверов. Файлы — в `UPLOADS_DIR` со случайным ключом.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use proto::limits;

use crate::{
    AppState,
    chat::{sniff_image, storage_path, write_atomic},
    error::{AppError, AppResult},
    extract::AuthUser,
    security,
};

/// Проверить и сохранить квадратную картинку (PNG/JPEG/WebP, до 512 px и 1 МБ). Возвращает ключ.
pub async fn store_profile_image(state: &AppState, body: &[u8]) -> AppResult<String> {
    if body.len() > limits::IMAGE_PROFILE_MAX_BYTES {
        return Err(AppError::Validation(format!(
            "картинка больше {} КБ",
            limits::IMAGE_PROFILE_MAX_BYTES / 1024
        )));
    }
    match sniff_image(body) {
        Some("image/png" | "image/jpeg" | "image/webp") => {}
        _ => return Err(AppError::Validation("поддерживаются PNG, JPEG и WebP".into())),
    }
    let dim = imagesize::blob_size(body).map_err(|_| AppError::Validation("картинка повреждена".into()))?;
    let side = limits::IMAGE_PROFILE_MAX_SIDE as usize;
    if dim.width == 0 || dim.width != dim.height || dim.width > side {
        return Err(AppError::Validation(format!("нужен квадрат до {side}×{side}")));
    }
    let key = security::new_session_token();
    write_atomic(&storage_path(state, &key), body)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("запись файла: {e}")))?;
    Ok(key)
}

/// Баннер сервера: PNG/JPEG/WebP ровно 960×480, до 2 МБ. Возвращает ключ.
pub async fn store_banner_image(state: &AppState, body: &[u8]) -> AppResult<String> {
    if body.len() > limits::IMAGE_BANNER_MAX_BYTES {
        return Err(AppError::Validation(format!("картинка больше {} МБ", limits::IMAGE_BANNER_MAX_BYTES / 1024 / 1024)));
    }
    match sniff_image(body) {
        Some("image/png" | "image/jpeg" | "image/webp") => {}
        _ => return Err(AppError::Validation("поддерживаются PNG, JPEG и WebP".into())),
    }
    let dim = imagesize::blob_size(body).map_err(|_| AppError::Validation("картинка повреждена".into()))?;
    if dim.width != limits::BANNER_W as usize || dim.height != limits::BANNER_H as usize {
        return Err(AppError::Validation(format!("нужна картинка {}×{}", limits::BANNER_W, limits::BANNER_H)));
    }
    let key = security::new_session_token();
    write_atomic(&storage_path(state, &key), body)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("запись файла: {e}")))?;
    Ok(key)
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
        "SELECT (SELECT COUNT(*) FROM users WHERE avatar_key = ?1) + (SELECT COUNT(*) FROM servers WHERE icon_key = ?1 OR banner_key = ?1)",
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
