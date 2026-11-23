//! Раздача клиента: `GET /version` (последний выпуск), `GET /download/{file}` (файлы из `RELEASES_DIR`),
//! `GET /get` → установщик. Без авторизации: ссылку на установщик можно просто отправить другу.
//!
//! Выпуск — это файлы в `RELEASES_DIR` и `latest.json` (`proto::ReleaseInfo`) рядом; кладёт их `scripts/release.sh`.
//! Для `amadeus-x.y.z.exe` можно положить `amadeus-x.y.z.exe.gz` — отдаётся клиентам, которые принимают gzip.
//!
//! Свой сервер без своих выпусков (self-host) берёт `latest.json` у официального (`Upstream`, UPDATES_UPSTREAM)
//! и отправляет за файлами туда же (302). Выпуск с `min_server` новее версии этого сервера клиентам не показывается.

use std::time::{Duration, Instant};

use axum::{
    Json,
    extract::State,
    http::{HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Redirect, Response},
};
use proto::ReleaseInfo;

use crate::{
    AppState,
    error::{AppError, AppResult},
};

pub const LATEST: &str = "latest.json";

async fn latest(state: &AppState) -> AppResult<ReleaseInfo> {
    let path = state.inner.releases_dir.join(LATEST);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return match &state.inner.upstream {
                Some(up) => up.latest().await.ok_or(AppError::NotFound),
                None => Err(AppError::NotFound),
            };
        }
        Err(e) => return Err(AppError::Internal(anyhow::anyhow!("чтение {LATEST}: {e}"))),
    };
    let info: ReleaseInfo = serde_json::from_slice(&bytes)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("{LATEST} повреждён: {e}")))?;
    if !info.is_valid() {
        return Err(AppError::Internal(anyhow::anyhow!("{LATEST}: недопустимые поля")));
    }
    Ok(info)
}

/// `GET /version`
pub async fn version(State(state): State<AppState>) -> AppResult<Response> {
    let mut resp = Json(latest(&state).await?).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Ok(resp)
}

/// `GET /get` — на установщик последней версии (относительная ссылка: работает за любым прокси).
pub async fn get_installer(State(state): State<AppState>) -> AppResult<Redirect> {
    let name = latest(&state).await?.installer.ok_or(AppError::NotFound)?;
    Ok(Redirect::temporary(&format!("download/{name}")))
}

/// `/download/{file}`, которого нет в RELEASES_DIR: при включённом upstream — 302 на тот же файл у него.
pub async fn download_fallback(State(state): State<AppState>, uri: Uri) -> Response {
    let name = uri.path().trim_start_matches('/');
    match &state.inner.upstream {
        Some(up) if proto::safe_file_name(name) => {
            (StatusCode::FOUND, [(header::LOCATION, format!("{}/download/{name}", up.base))]).into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Самое большое тело `/version` у upstream, которое читаем (настоящий latest.json — меньше 1 КБ).
const UPSTREAM_MAX_BODY: usize = 64 * 1024;

/// Официальный сервер — источник обновлений клиента для сервера без своих выпусков.
pub struct Upstream {
    base: String,
    server_version: String,
    http: reqwest::Client,
    fresh: Duration,
    stale_max: Duration,
    /// После неудачи upstream не дёргаем столько времени (отдаём старый кеш или «обновлений нет»).
    retry_after: Duration,
    state: std::sync::Mutex<CacheState>,
    /// Один запрос к upstream за раз; остальные в это время не ждут, а берут кеш.
    fetching: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct CacheState {
    ok: Option<(Instant, ReleaseInfo)>,
    failed_at: Option<Instant>,
}

impl Upstream {
    /// Кеш свежий 5 мин, при недоступности upstream старый отдаётся до 24 ч; запрос — не дольше 5 с.
    pub fn new(base: &str, server_version: &str) -> Self {
        Self {
            base: base.trim().trim_end_matches('/').to_string(),
            server_version: server_version.to_string(),
            http: client(Duration::from_secs(5)),
            fresh: Duration::from_secs(5 * 60),
            stale_max: Duration::from_secs(24 * 3600),
            retry_after: Duration::from_secs(60),
            state: std::sync::Mutex::new(CacheState::default()),
            fetching: tokio::sync::Mutex::new(()),
        }
    }

    pub fn with_timings(mut self, fresh: Duration, stale_max: Duration, timeout: Duration) -> Self {
        self.fresh = fresh;
        self.stale_max = stale_max;
        self.http = client(timeout);
        self
    }

    /// Из настройки UPDATES_UPSTREAM: пустая строка — выключено.
    pub fn from_config(url: &str, server_version: &str) -> Option<Self> {
        (!url.trim().is_empty()).then(|| Self::new(url, server_version))
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Последний выпуск, который можно отдать клиентам этого сервера. Никогда не ждёт чужой запрос к upstream.
    async fn latest(&self) -> Option<ReleaseInfo> {
        let stale = {
            let s = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((at, info)) = &s.ok
                && at.elapsed() < self.fresh
            {
                return Some(info.clone());
            }
            let stale = s.ok.as_ref().filter(|(at, _)| at.elapsed() < self.stale_max).map(|(_, i)| i.clone());
            if s.failed_at.is_some_and(|t| t.elapsed() < self.retry_after) {
                return stale;
            }
            stale
        };
        let Ok(_guard) = self.fetching.try_lock() else {
            return stale;
        };
        let res = self.fetch().await;
        let mut s = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match res {
            Ok(Some(info)) => {
                s.ok = Some((Instant::now(), info.clone()));
                s.failed_at = None;
                Some(info)
            }
            Ok(None) => {
                s.ok = None;
                s.failed_at = None;
                None
            }
            Err(e) => {
                tracing::warn!(upstream = %self.base, "обновления с официального сервера: {e}");
                s.failed_at = Some(Instant::now());
                stale
            }
        }
    }

    /// Ok(None) — у upstream выпуска нет или он не для этого сервера; Err — upstream недоступен или ответил мусором.
    async fn fetch(&self) -> Result<Option<ReleaseInfo>, String> {
        let mut resp = self
            .http
            .get(format!("{}/version", self.base))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!("ответ {}", resp.status()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            body.extend_from_slice(&chunk);
            if body.len() > UPSTREAM_MAX_BODY {
                return Err("слишком большой ответ".into());
            }
        }
        let info: ReleaseInfo = serde_json::from_slice(&body).map_err(|e| format!("не latest.json: {e}"))?;
        if !info.is_valid() {
            return Err("latest.json: недопустимые поля".into());
        }
        Ok(info.server_ok(&self.server_version).then_some(info))
    }
}

fn client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(timeout)
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::limited(3))
        .user_agent(concat!("murmur-backend/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("reqwest client")
}

#[cfg(test)]
mod tests {
    use super::Upstream;

    #[test]
    fn upstream_from_config() {
        assert!(Upstream::from_config("", "0.3.13").is_none());
        assert!(Upstream::from_config("  ", "0.3.13").is_none());
        let up = Upstream::from_config("https://api.murmurchat.ru/", "0.3.13").unwrap();
        assert_eq!(up.base(), "https://api.murmurchat.ru");
    }
}
