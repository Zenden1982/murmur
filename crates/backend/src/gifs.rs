//! GIF в чате: поиск через Klipy (ключ только у сервера) и прокси картинок: клиент забирает превью и выбранную гифку
//! только у нашего сервера, дальше гифка идёт как обычная загруженная картинка (проверка и перекодирование).
//! Клиенты с Klipy не общаются: ни ключа, ни их адресов у сервиса нет.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::{
    Json,
    body::Bytes,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use proto::{GifItem, GifPage, limits};
use serde::Deserialize;

use crate::{
    AppState,
    error::{AppError, AppResult},
    extract::AuthUser,
    ratelimit::RateLimiter,
};

const API_BASE: &str = "https://api.klipy.com";
/// Единственный хост, с которого берём файлы (защита от подмены адреса клиентом).
const MEDIA_HOST: &str = "static.klipy.com";
const PER_PAGE: u32 = 24;
const MD_GIF_MAX_BYTES: u64 = 2 * 1024 * 1024;
const PAGE_MAX: u32 = 20;
const QUERY_MAX: usize = 60;
const SEARCH_TTL: Duration = Duration::from_secs(600);
const MEDIA_TTL: Duration = Duration::from_secs(1800);
/// Больше этого в кеш не кладём (гифка целиком идёт один раз, превью — часто).
const MEDIA_CACHE_MAX_BYTES: usize = 512 * 1024;
const CACHE_MAX: usize = 300;

pub struct Klipy {
    key: String,
    base: String,
    http: reqwest::Client,
    search_cache: Mutex<HashMap<String, (Instant, GifPage)>>,
    media_cache: Mutex<HashMap<String, (Instant, Bytes, String)>>,
    limiter: RateLimiter<i64>,
}

impl Klipy {
    pub fn new(key: &str) -> Self {
        Self::with_base(key, API_BASE)
    }

    pub fn with_base(key: &str, base: &str) -> Self {
        Self {
            key: key.trim().to_string(),
            base: base.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("murmur-backend/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("reqwest client"),
            search_cache: Mutex::new(HashMap::new()),
            media_cache: Mutex::new(HashMap::new()),
            limiter: RateLimiter::new(40, Duration::from_secs(60)),
        }
    }
}

fn klipy(state: &AppState) -> AppResult<&Klipy> {
    state.inner.klipy.as_ref().ok_or_else(|| AppError::BadRequest("GIF на этом сервере не настроены".into()))
}

fn limit(k: &Klipy, user: &AuthUser) -> AppResult<()> {
    k.limiter.check(user.id).map_err(|retry_after_secs| AppError::RateLimited { retry_after_secs })
}

fn upstream_err(what: &str, e: impl std::fmt::Display) -> AppError {
    tracing::warn!("klipy {what}: {e}");
    AppError::BadRequest("сервис GIF сейчас недоступен, попробуйте позже".into())
}

/// Адрес файла допустим: https, ровно наш хост, без порта и логина.
pub fn media_url(u: &str) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(u).ok()?;
    (url.scheme() == "https" && url.host_str() == Some(MEDIA_HOST) && url.port().is_none() && url.username().is_empty() && url.password().is_none())
        .then_some(url)
}

/// Разбор ответа Klipy (поиск/популярное). Лишнее и странное пропускаем.
pub fn parse_page(v: &serde_json::Value) -> GifPage {
    let data = &v["data"];
    let url = |o: &serde_json::Value| o["url"].as_str().filter(|u| media_url(u).is_some()).map(str::to_string);
    let items = data["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|i| i["type"].as_str().is_none_or(|t| t == "gif"))
                .filter_map(|i| {
                    let id = i["id"].as_i64().map(|n| n.to_string()).or_else(|| i["id"].as_str().map(str::to_string))?;
                    let f = &i["file"];
                    let thumb = url(&f["sm"]["jpg"])?;
                    let preview = url(&f["sm"]["gif"]).unwrap_or_else(|| thumb.clone());
                    // среднего размера, но тяжёлые (больше 2 МБ) заменяем маленькими — трафик и память клиентов
                    let md = (f["md"]["gif"]["size"].as_u64().unwrap_or(0) <= MD_GIF_MAX_BYTES).then(|| url(&f["md"]["gif"])).flatten();
                    let gif = md.or_else(|| url(&f["sm"]["gif"])).or_else(|| url(&f["md"]["gif"]))?;
                    Some(GifItem {
                        id,
                        title: i["title"].as_str().unwrap_or("").chars().take(120).collect(),
                        width: f["sm"]["jpg"]["width"].as_u64().unwrap_or(0) as u32,
                        height: f["sm"]["jpg"]["height"].as_u64().unwrap_or(0) as u32,
                        thumb,
                        preview,
                        gif,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    GifPage { items, has_next: data["has_next"].as_bool().unwrap_or(false) }
}

#[derive(Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    page: u32,
}

/// `GET /gifs/search?q=&page=` — поиск; пустой запрос — популярное. Ответы кешируются на 10 минут.
pub async fn search(State(state): State<AppState>, user: AuthUser, Query(q): Query<SearchQuery>) -> AppResult<Json<GifPage>> {
    let k = klipy(&state)?;
    limit(k, &user)?;
    let text: String = q.q.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > QUERY_MAX {
        return Err(AppError::Validation(format!("запрос: до {QUERY_MAX} символов")));
    }
    let page = q.page.clamp(1, PAGE_MAX);
    let cache_key = format!("{}|{page}", text.to_lowercase());
    {
        let c = k.search_cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, p)) = c.get(&cache_key)
            && at.elapsed() < SEARCH_TTL
        {
            return Ok(Json(p.clone()));
        }
    }
    let url = format!("{}/api/v1/{}/gifs/{}", k.base, k.key, if text.is_empty() { "trending" } else { "search" });
    let mut req = k.http.get(url).query(&[("per_page", PER_PAGE.to_string()), ("page", page.to_string())]);
    if !text.is_empty() {
        req = req.query(&[("q", text.as_str())]);
    }
    let resp = req.send().await.map_err(|e| upstream_err("поиск", e.without_url()))?;
    if !resp.status().is_success() {
        return Err(upstream_err("поиск", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| upstream_err("ответ", e.without_url()))?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(upstream_err("ответ", "слишком большой"));
    }
    let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| upstream_err("ответ", e))?;
    let out = parse_page(&v);
    let mut c = k.search_cache.lock().unwrap_or_else(|e| e.into_inner());
    if c.len() >= CACHE_MAX {
        c.retain(|_, (at, _)| at.elapsed() < SEARCH_TTL);
        if c.len() >= CACHE_MAX {
            c.clear();
        }
    }
    c.insert(cache_key, (Instant::now(), out.clone()));
    Ok(Json(out))
}

/// Скачать файл с Klipy (с проверкой адреса, типа и размера).
async fn fetch_media(k: &Klipy, u: &str, max: usize) -> AppResult<(Bytes, String)> {
    let url = media_url(u).ok_or_else(|| AppError::Validation("адрес файла не принят".into()))?;
    let resp = k.http.get(url).send().await.map_err(|e| upstream_err("файл", e.without_url()))?;
    if !resp.status().is_success() {
        return Err(upstream_err("файл", resp.status()));
    }
    let ctype = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if !ctype.starts_with("image/") {
        return Err(AppError::Validation("это не картинка".into()));
    }
    if resp.content_length().is_some_and(|n| n as usize > max) {
        return Err(AppError::Validation("файл слишком большой".into()));
    }
    let mut body = Vec::new();
    let mut resp = resp;
    while let Some(chunk) = resp.chunk().await.map_err(|e| upstream_err("файл", e.without_url()))? {
        if body.len() + chunk.len() > max {
            return Err(AppError::Validation("файл слишком большой".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((Bytes::from(body), ctype))
}

#[derive(Deserialize)]
pub struct MediaQuery {
    u: String,
}

/// `GET /gifs/media?u=` — файл с Klipy (превью, выбранная гифка) через наш сервер; небольшие файлы кешируются на 30 минут.
pub async fn media(State(state): State<AppState>, user: AuthUser, Query(q): Query<MediaQuery>) -> AppResult<Response> {
    let k = klipy(&state)?;
    media_url(&q.u).ok_or_else(|| AppError::Validation("адрес файла не принят".into()))?;
    {
        let c = k.media_cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, b, ct)) = c.get(&q.u)
            && at.elapsed() < MEDIA_TTL
        {
            return Ok(media_response(b.clone(), ct));
        }
    }
    // лимит — только на промахи кеша: сетка из 24 превью не должна упираться в него
    limit(k, &user)?;
    let (b, ct) = fetch_media(k, &q.u, limits::IMAGE_MAX_BYTES).await?;
    if b.len() <= MEDIA_CACHE_MAX_BYTES {
        let mut c = k.media_cache.lock().unwrap_or_else(|e| e.into_inner());
        if c.len() >= CACHE_MAX {
            c.clear();
        }
        c.insert(q.u, (Instant::now(), b.clone(), ct.clone()));
    }
    Ok(media_response(b, &ct))
}

fn media_response(b: Bytes, ct: &str) -> Response {
    (StatusCode::OK, [(header::CONTENT_TYPE, ct.to_string()), (header::CACHE_CONTROL, "private, max-age=3600".to_string())], b).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_klipy_static_urls() {
        assert!(media_url("https://static.klipy.com/ii/x/a.gif").is_some());
        for bad in [
            "http://static.klipy.com/a.gif",
            "https://static.klipy.com.evil.test/a.gif",
            "https://evil.test/static.klipy.com/a.gif",
            "https://user@static.klipy.com/a.gif",
            "https://static.klipy.com:444/a.gif",
            "https://127.0.0.1/a.gif",
            "ftp://static.klipy.com/a.gif",
            "не адрес",
        ] {
            assert!(media_url(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn parses_klipy_page() {
        let v = serde_json::json!({"result": true, "data": {"has_next": true, "data": [
            {"id": 42, "title": "Кот", "type": "gif", "file": {
                "sm": {"jpg": {"url": "https://static.klipy.com/a.jpg", "width": 220, "height": 200},
                       "gif": {"url": "https://static.klipy.com/a-sm.gif"}},
                "md": {"gif": {"url": "https://static.klipy.com/a-md.gif"}}}},
            {"id": 43, "type": "ad", "file": {}},
            {"id": 44, "type": "gif", "file": {"sm": {"jpg": {"url": "https://evil.test/a.jpg"}}}}
        ]}});
        let p = parse_page(&v);
        assert!(p.has_next);
        assert_eq!(p.items.len(), 1);
        let i = &p.items[0];
        assert_eq!((i.id.as_str(), i.width, i.height), ("42", 220, 200));
        assert_eq!(i.gif, "https://static.klipy.com/a-md.gif");
    }
}
