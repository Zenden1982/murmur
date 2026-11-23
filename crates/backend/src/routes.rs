use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post},
};
use proto::{
    AuthResponse, HealthResponse, LoginRequest, RegisterRequest, User, limits,
};
use tower_http::{services::ServeDir, trace::TraceLayer};

use crate::{
    AppState, SESSION_TTL, account, channels, chat, dm, events, friends, gifs, groupcalls, images, invites, profile, releases, servers, voice,
    db::{self},
    error::{AppError, AppResult},
    extract::{ApiJson, AuthUser, ClientIp},
    now, perm, security,
};

pub fn router(state: AppState) -> Router {
    let downloads = ServeDir::new(&state.inner.releases_dir)
        .precompressed_gzip()
        .fallback(get(releases::download_fallback).with_state(state.clone()));
    Router::new()
        .route("/health", get(health))
        .route("/version", get(releases::version))
        .route("/get", get(releases::get_installer))
        .nest_service("/download", downloads)
        .route("/auth/register", post(register))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/logout-all", post(logout_all))
        .route("/auth/sessions", get(account::list_sessions))
        .route("/auth/sessions/{id}", delete(account::revoke_session))
        .route("/auth/password", post(account::change_password))
        .route("/gifs/search", get(gifs::search))
        .route("/gifs/media", get(gifs::media))        .route("/auth/delete", post(account::delete_account))
        .route("/me", get(profile::me).patch(profile::update))
        .route(
            "/me/avatar",
            axum::routing::put(profile::put_avatar)
                .delete(profile::delete_avatar)
                .layer(DefaultBodyLimit::max(limits::AVATAR_MAX_BYTES + 64 * 1024)),
        )
        .route(
            "/me/banner",
            axum::routing::put(profile::put_banner)
                .delete(profile::delete_banner)
                .layer(DefaultBodyLimit::max(limits::USER_BANNER_MAX_BYTES + 64 * 1024)),
        )
        .route("/channels", get(channels::legacy_list).post(channels::legacy_create))
        .route("/channels/{id}", axum::routing::patch(channels::update).delete(channels::delete))
        .route("/channels/{id}/join", post(channels::join))
        .route("/channels/{id}/summon", post(channels::summon))
        .route("/me/activity", axum::routing::put(profile::put_activity))
        .route("/users/{id}", get(profile::profile))
        .route("/servers/{id}/channels", get(channels::list).post(channels::create))
        .route("/servers/{id}/categories", post(channels::create_category))
        .route("/categories/{id}", axum::routing::patch(channels::rename_category).delete(channels::delete_category))
        .route("/servers/{id}/layout", axum::routing::put(channels::put_layout))
        .route("/invites", get(invites::legacy_list).post(invites::legacy_create))
        .route("/invites/{code}", get(invites::preview).delete(invites::revoke))
        .route("/invites/{code}/join", post(invites::join))
        .route("/servers/{id}/invites", get(invites::list).post(invites::create))
        .route(
            "/channels/{id}/messages",
            get(chat::list_messages).post(chat::send_message),
        )
        .route(
            "/uploads",
            post(chat::upload).layer(DefaultBodyLimit::max(limits::IMAGE_MAX_BYTES + 64 * 1024)),
        )
        .route("/attachments/{id}", get(chat::get_attachment))
        .route("/messages/{id}/reactions/{name}", axum::routing::put(chat::put_reaction).delete(chat::delete_reaction))
        .route("/events", get(events::events))
        .route("/friends", get(friends::get))
        .route("/friends/requests", post(friends::request))
        .route("/friends/{id}/accept", post(friends::accept))
        .route("/friends/{id}", delete(friends::remove))
        .route("/dms", get(dm::list).post(dm::open))
        .route("/dms/{id}/call", post(dm::call).delete(dm::hang_up))
        .route("/calls", get(groupcalls::list).post(groupcalls::create))
        .route("/calls/{id}/join", post(groupcalls::join))
        .route("/calls/{id}/invite", post(groupcalls::invite))
        .route("/calls/{id}", axum::routing::delete(groupcalls::hang_up))
        .route("/unread", get(dm::unread))
        .route("/channels/{id}/read", post(dm::read))
        .route("/servers", get(servers::list).post(servers::create))
        .route("/servers/{id}", axum::routing::patch(servers::update).delete(servers::delete))
        .route(
            "/servers/{id}/icon",
            axum::routing::put(servers::put_icon)
                .delete(servers::delete_icon)
                .layer(DefaultBodyLimit::max(limits::IMAGE_PROFILE_MAX_BYTES + 64 * 1024)),
        )
        .route(
            "/servers/{id}/banner",
            axum::routing::put(servers::put_banner)
                .delete(servers::delete_banner)
                .layer(DefaultBodyLimit::max(limits::IMAGE_BANNER_MAX_BYTES + 64 * 1024)),
        )
        .route("/servers/{id}/transfer", post(servers::transfer))
        .route("/servers/{id}/members", get(servers::members))
        .route("/servers/{id}/members/@me", delete(servers::leave))
        .route("/servers/{id}/members/{uid}", delete(servers::kick))
        .route("/servers/{id}/members/{uid}/role", axum::routing::put(servers::set_role))
        .route("/servers/{id}/members/{uid}/disconnect", post(servers::disconnect))
        .route("/servers/{id}/bans", get(servers::bans).post(servers::ban))
        .route("/servers/{id}/bans/{uid}", delete(servers::unban))
        .route("/images/{key}", get(images::get_image))
        .route("/livekit/webhook", post(voice::webhook))
        .route("/servers/{id}/voice", get(voice::server_voice))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: proto::PROTOCOL_VERSION,
    })
}

/// Название устройства: заголовок клиента X-Murmur-Device, иначе User-Agent; без управляющих символов, до 60 знаков.
fn device_name(headers: &HeaderMap) -> String {
    let raw = ["x-murmur-device", "user-agent"]
        .iter()
        .find_map(|h| headers.get(*h).and_then(|v| v.to_str().ok()).filter(|s| !s.trim().is_empty()))
        .unwrap_or("");
    raw.chars().filter(|c| !c.is_control()).take(60).collect::<String>().trim().to_string()
}

async fn new_session(state: &AppState, user: User, device: &str) -> AppResult<AuthResponse> {
    let token = security::new_session_token();
    let expires_at = now() + SESSION_TTL.as_secs() as i64;
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, created_at, expires_at, device, last_seen) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(security::token_hash(&token))
    .bind(user.id)
    .bind(now())
    .bind(expires_at)
    .bind(device)
    .bind(now())
    .execute(&state.inner.db)
    .await?;
    Ok(AuthResponse {
        token,
        expires_at,
        user,
    })
}

/// Ключ ограничителя: IPv6 — по сети /64 (у одного клиента их миллиарды), IPv4-в-IPv6 — как IPv4.
pub(crate) fn limiter_key(ip: std::net::IpAddr) -> std::net::IpAddr {
    use std::net::{IpAddr, Ipv6Addr};
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let mut s = v6.segments();
                s[4..].fill(0);
                IpAddr::V6(Ipv6Addr::from(s))
            }
        },
        v4 => v4,
    }
}

pub(crate) fn rate_limit(state: &AppState, ip: std::net::IpAddr) -> AppResult<()> {
    let ip = limiter_key(ip);
    state
        .inner
        .auth_limiter
        .check(ip)
        .map_err(|retry_after_secs| {
            tracing::warn!(%ip, "auth rate limit");
            AppError::RateLimited { retry_after_secs }
        })
}

async fn register(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    ApiJson(req): ApiJson<RegisterRequest>,
) -> AppResult<(StatusCode, Json<AuthResponse>)> {
    rate_limit(&state, ip)?;
    let name = db::validate_name(&req.name)?;
    if db::name_collides(&state.inner.db, &name).await? {
        return Err(AppError::NameTaken);
    }
    db::validate_password(&req.password)?;
    let code = security::normalize_invite_code(&req.invite_code);
    let hash = db::hash_password_async(req.password).await?;

    // Списание инвайта и создание пользователя — одной транзакцией.
    let mut tx = state.inner.db.begin().await?;
    let server_id: Option<Option<i64>> = sqlx::query_scalar(
        "UPDATE invites SET uses_left = uses_left - 1
         WHERE code = ? AND revoked = 0 AND uses_left > 0 AND (expires_at IS NULL OR expires_at > ?)
         RETURNING server_id",
    )
    .bind(&code)
    .bind(now())
    .fetch_optional(&mut *tx)
    .await?;
    let server_id = server_id.flatten().ok_or(AppError::InvalidInvite)?;
    let id = db::insert_user(&mut tx, &name, &hash, false).await?;
    let role = perm::default_role(&mut *tx, server_id).await?;
    sqlx::query("INSERT INTO server_members (server_id, user_id, role_id, joined_at) VALUES (?, ?, ?, ?)")
        .bind(server_id)
        .bind(id)
        .bind(role)
        .bind(now())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(user_id = id, %name, server_id, "registered by invite");
    // Пользователь уже создан: сбой рассылки не должен превращать успех в 500.
    if let Err(e) = invites::announce_join(&state, server_id, id).await {
        tracing::warn!(user_id = id, server_id, "announce_join после регистрации: {e:?}");
    }

    let resp = new_session(
        &state,
        profile::load_user(&state.inner.db, id).await?,
        &device_name(&headers),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(resp)))
}

async fn login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    ApiJson(req): ApiJson<LoginRequest>,
) -> AppResult<Json<AuthResponse>> {
    rate_limit(&state, ip)?;
    let row: Option<(i64, String, String, bool)> =
        sqlx::query_as("SELECT id, name, password_hash, is_admin FROM users WHERE name = ?")
            .bind(req.name.trim())
            .fetch_optional(&state.inner.db)
            .await?;
    let hash = row.as_ref().map(|r| r.2.clone());
    let password = req.password;
    let ok =
        tokio::task::spawn_blocking(move || security::verify_password(&password, hash.as_deref()))
            .await?;
    let Some((id, _, _, _)) = row.filter(|_| ok) else {
        return Err(AppError::BadCredentials);
    };
    // считаются только неудачные попытки: удачный вход (в т.ч. за общим IP) блокировку не приближает
    state.inner.auth_limiter.refund(limiter_key(ip));
    sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(now())
        .execute(&state.inner.db)
        .await?;
    Ok(Json(
        new_session(&state, profile::load_user(&state.inner.db, id).await?, &device_name(&headers)).await?,
    ))
}

/// `POST /auth/logout-all` — «выйти везде»: все сеансы этого человека, включая текущий (S20).
async fn logout_all(State(state): State<AppState>, user: AuthUser) -> AppResult<StatusCode> {
    sqlx::query("DELETE FROM sessions WHERE user_id = ?")
        .bind(user.id)
        .execute(&state.inner.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn logout(State(state): State<AppState>, user: AuthUser) -> AppResult<StatusCode> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(&user.token_hash)
        .execute(&state.inner.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}



