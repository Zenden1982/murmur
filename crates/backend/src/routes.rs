use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use proto::{
    AuthResponse, HealthResponse, LoginRequest, RegisterRequest, User, limits,
};
use tower_http::{services::ServeDir, trace::TraceLayer};

use crate::{
    AppState, SESSION_TTL, channels, chat, dm, events, friends, images, invites, profile, releases, servers, voice,
    db::{self},
    error::{AppError, AppResult},
    extract::{ApiJson, AuthUser, ClientIp},
    now, perm, security,
};

pub fn router(state: AppState) -> Router {
    let downloads = ServeDir::new(&state.inner.releases_dir).precompressed_gzip();
    Router::new()
        .route("/health", get(health))
        .route("/version", get(releases::version))
        .route("/get", get(releases::get_installer))
        .nest_service("/download", downloads)
        .route("/auth/register", post(register))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/me", get(profile::me).patch(profile::update))
        .route(
            "/me/avatar",
            axum::routing::put(profile::put_avatar)
                .delete(profile::delete_avatar)
                .layer(DefaultBodyLimit::max(limits::IMAGE_PROFILE_MAX_BYTES + 64 * 1024)),
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

async fn new_session(state: &AppState, user: User) -> AppResult<AuthResponse> {
    let token = security::new_session_token();
    let expires_at = now() + SESSION_TTL.as_secs() as i64;
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, created_at, expires_at) VALUES (?, ?, ?, ?)",
    )
    .bind(security::token_hash(&token))
    .bind(user.id)
    .bind(now())
    .bind(expires_at)
    .execute(&state.inner.db)
    .await?;
    Ok(AuthResponse {
        token,
        expires_at,
        user,
    })
}

fn rate_limit(state: &AppState, ip: std::net::IpAddr) -> AppResult<()> {
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
    ApiJson(req): ApiJson<RegisterRequest>,
) -> AppResult<(StatusCode, Json<AuthResponse>)> {
    rate_limit(&state, ip)?;
    let name = db::validate_name(&req.name)?;
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
    invites::announce_join(&state, server_id, id).await?;

    let resp = new_session(
        &state,
        profile::load_user(&state.inner.db, id).await?,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(resp)))
}

async fn login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
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
    sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(now())
        .execute(&state.inner.db)
        .await?;
    Ok(Json(
        new_session(&state, profile::load_user(&state.inner.db, id).await?).await?,
    ))
}

async fn logout(State(state): State<AppState>, user: AuthUser) -> AppResult<StatusCode> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(&user.token_hash)
        .execute(&state.inner.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}



