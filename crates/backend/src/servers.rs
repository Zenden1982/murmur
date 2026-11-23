//! Серверы-сообщества: создание, настройки, участники, передача владения, удаление.

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
};
use proto::{
    Ban, BanRequest, CreateServerRequest, Member, PublicUser, Role, Server, ServerEvent, SetRoleRequest, TransferRequest,
    UpdateServerRequest, limits, perm,
};

use crate::{
    AppState,
    error::{AppError, AppResult},
    events::{Audience, publish},
    extract::{ApiJson, AuthUser},
    images, now,
    perm::{self as p, MemberCtx},
    voice,
};

pub(crate) fn validate_name(raw: &str, max: usize, what: &str) -> AppResult<String> {
    let name = raw.trim().to_string();
    if name.is_empty() || name.chars().count() > max {
        return Err(AppError::Validation(format!("{what}: от 1 до {max} символов")));
    }
    Ok(name)
}

pub(crate) async fn public_user(db: &sqlx::SqlitePool, id: i64) -> AppResult<PublicUser> {
    let row: Option<(i64, String, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT id, name, display_name, avatar_key FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(db)
            .await?;
    let (id, name, display_name, avatar) = row.ok_or(AppError::NotFound)?;
    Ok(PublicUser { id, name, display_name, avatar })
}

async fn roles_of(db: &sqlx::SqlitePool, server_id: i64) -> AppResult<Vec<Role>> {
    let rows: Vec<(i64, String, i32, i64)> =
        sqlx::query_as("SELECT id, name, position, permissions FROM server_roles WHERE server_id = ? ORDER BY position DESC")
            .bind(server_id)
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().map(|(id, name, position, perms)| Role { id, name, position, permissions: perms as u32 }).collect())
}

/// Сервер глазами участника (с его ролью).
pub(crate) async fn server_for(db: &sqlx::SqlitePool, server_id: i64, user_id: i64) -> AppResult<Server> {
    let m = p::member(db, server_id, user_id).await?;
    type Row = (String, Option<String>, i64, Option<String>, Option<String>, Option<String>, Option<String>);
    let (name, icon, owner_id, description, banner, banner_image, color): Row =
        sqlx::query_as("SELECT name, icon_key, owner_id, description, banner, banner_key, color FROM servers WHERE id = ?")
            .bind(server_id)
            .fetch_one(db)
            .await?;
    Ok(Server {
        id: server_id,
        name,
        icon,
        owner_id,
        roles: roles_of(db, server_id).await?,
        my_role_id: m.role_id,
        description,
        banner,
        banner_image,
        color,
    })
}

pub(crate) async fn member_ids(db: &sqlx::SqlitePool, server_id: i64) -> AppResult<Vec<i64>> {
    Ok(sqlx::query_scalar("SELECT user_id FROM server_members WHERE server_id = ? ORDER BY user_id")
        .bind(server_id)
        .fetch_all(db)
        .await?)
}

pub(crate) async fn member_json(state: &AppState, server_id: i64, user_id: i64) -> AppResult<Member> {
    let db = &state.inner.db;
    let m = p::member(db, server_id, user_id).await?;
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM users WHERE id = ?").bind(user_id).fetch_one(db).await?;
    let online = &state.inner.online;
    Ok(Member {
        user: public_user(db, user_id).await?,
        role_id: m.role_id,
        online: online.is_online(user_id),
        activity: online.activity(user_id, status),
    })
}

/// Разослать каждому участнику сервер с его собственной ролью.
pub(crate) async fn publish_server(state: &AppState, server_id: i64) -> AppResult<()> {
    for uid in member_ids(&state.inner.db, server_id).await? {
        let server = server_for(&state.inner.db, server_id, uid).await?;
        publish(state, Audience::User(uid), ServerEvent::ServerUpserted { server });
    }
    Ok(())
}

pub(crate) async fn publish_member(state: &AppState, server_id: i64, user_id: i64) -> AppResult<()> {
    let member = member_json(state, server_id, user_id).await?;
    publish(state, Audience::Server(server_id), ServerEvent::MemberUpserted { server_id, member });
    Ok(())
}

/// Новый сервер: стандартные роли, владелец; `default_channels` — «general» (текст) и «General» (голос).
pub(crate) async fn insert_server(
    conn: &mut sqlx::SqliteConnection,
    name: &str,
    owner: i64,
    default_channels: bool,
) -> AppResult<i64> {
    let sid = sqlx::query("INSERT INTO servers (name, owner_id, created_at) VALUES (?, ?, ?)")
        .bind(name)
        .bind(owner)
        .bind(now())
        .execute(&mut *conn)
        .await?
        .last_insert_rowid();
    let roles = p::create_default_roles(&mut *conn, sid).await?;
    sqlx::query("INSERT INTO server_members (server_id, user_id, role_id, joined_at) VALUES (?, ?, ?, ?)")
        .bind(sid)
        .bind(owner)
        .bind(roles.owner)
        .bind(now())
        .execute(&mut *conn)
        .await?;
    if default_channels {
        for (pos, (cname, kind)) in [("general", "text"), ("General", "voice")].into_iter().enumerate() {
            sqlx::query("INSERT INTO channels (name, kind, created_at, server_id, position) VALUES (?, ?, ?, ?, ?)")
                .bind(cname)
                .bind(kind)
                .bind(now())
                .bind(sid)
                .bind(pos as i64)
                .execute(&mut *conn)
                .await?;
        }
    }
    Ok(sid)
}

/// `GET /servers` — мои серверы.
pub async fn list(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Vec<Server>>> {
    let ids: Vec<i64> = sqlx::query_scalar("SELECT server_id FROM server_members WHERE user_id = ? ORDER BY joined_at, server_id")
        .bind(user.id)
        .fetch_all(&state.inner.db)
        .await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(server_for(&state.inner.db, id, user.id).await?);
    }
    Ok(Json(out))
}

/// `POST /servers`
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    ApiJson(req): ApiJson<CreateServerRequest>,
) -> AppResult<(StatusCode, Json<Server>)> {
    let name = validate_name(&req.name, limits::SERVER_NAME_MAX, "название сервера")?;
    let owned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM servers WHERE owner_id = ?")
        .bind(user.id)
        .fetch_one(&state.inner.db)
        .await?;
    if owned as usize >= limits::SERVERS_OWNED_MAX {
        return Err(AppError::Validation(format!("можно создать не больше {} серверов", limits::SERVERS_OWNED_MAX)));
    }
    let mut tx = state.inner.db.begin().await?;
    let sid = insert_server(&mut tx, &name, user.id, true).await?;
    tx.commit().await?;
    tracing::info!(server_id = sid, user_id = user.id, "server created");
    let server = server_for(&state.inner.db, sid, user.id).await?;
    publish(&state, Audience::User(user.id), ServerEvent::ServerUpserted { server: server.clone() });
    Ok((StatusCode::CREATED, Json(server)))
}

async fn me_on(state: &AppState, server_id: i64, user: &AuthUser) -> AppResult<MemberCtx> {
    p::member(&state.inner.db, server_id, user.id).await
}

/// `PATCH /servers/{id}`
pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    ApiJson(req): ApiJson<UpdateServerRequest>,
) -> AppResult<Json<Server>> {
    me_on(&state, id, &user).await?.require(perm::MANAGE_SERVER)?;
    let db = &state.inner.db;
    // Сначала проверяем все поля, потом пишем — чтобы при ошибке не применялась половина правки.
    let name = req.name.map(|n| validate_name(&n, limits::SERVER_NAME_MAX, "название сервера")).transpose()?;
    let description = match req.description {
        Some(d) => {
            let d = d.trim().to_string();
            if d.chars().count() > limits::SERVER_DESCRIPTION_MAX || d.chars().any(|c| c.is_control() && c != '\n') {
                return Err(AppError::Validation(format!("описание: до {} символов", limits::SERVER_DESCRIPTION_MAX)));
            }
            Some(d)
        }
        None => None,
    };
    if let Some(b) = &req.banner
        && !b.is_empty()
        && !proto::banner::ALL.contains(&b.as_str())
    {
        return Err(AppError::Validation("нет такого узора баннера".into()));
    }
    let color = req.color.map(|c| c.to_ascii_uppercase());
    if let Some(c) = &color
        && !c.is_empty()
        && !proto::SERVER_COLORS.contains(&c.as_str())
    {
        return Err(AppError::Validation("нет такого цвета".into()));
    }
    let mut tx = db.begin().await?;
    if let Some(name) = name {
        sqlx::query("UPDATE servers SET name = ? WHERE id = ?").bind(name).bind(id).execute(&mut *tx).await?;
    }
    if let Some(d) = description {
        sqlx::query("UPDATE servers SET description = ? WHERE id = ?").bind((!d.is_empty()).then_some(d)).bind(id).execute(&mut *tx).await?;
    }
    if let Some(b) = req.banner {
        sqlx::query("UPDATE servers SET banner = ? WHERE id = ?").bind((!b.is_empty()).then_some(b)).bind(id).execute(&mut *tx).await?;
    }
    if let Some(c) = color {
        sqlx::query("UPDATE servers SET color = ? WHERE id = ?").bind((!c.is_empty()).then_some(c)).bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    publish_server(&state, id).await?;
    Ok(Json(server_for(&state.inner.db, id, user.id).await?))
}

/// `PUT /servers/{id}/icon` — тело: квадратная картинка.
pub async fn put_icon(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>, body: Bytes) -> AppResult<Json<Server>> {
    me_on(&state, id, &user).await?.require(perm::MANAGE_SERVER)?;
    state.inner.media_limiter.check(user.id).map_err(|retry_after_secs| AppError::RateLimited { retry_after_secs })?;
    let _images = state.inner.image_lock.lock().await; // сериализует запись файла, смену ключа и очистку (S12)
    let key = images::store_profile_image(&state, &body).await?;
    set_icon(&state, id, Some(key)).await?;
    Ok(Json(server_for(&state.inner.db, id, user.id).await?))
}

/// `DELETE /servers/{id}/icon`
pub async fn delete_icon(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> AppResult<Json<Server>> {
    me_on(&state, id, &user).await?.require(perm::MANAGE_SERVER)?;
    let _images = state.inner.image_lock.lock().await; // сериализует запись файла, смену ключа и очистку (S12)
    set_icon(&state, id, None).await?;
    Ok(Json(server_for(&state.inner.db, id, user.id).await?))
}

async fn set_icon(state: &AppState, id: i64, key: Option<String>) -> AppResult<()> {
    let old: Option<String> = sqlx::query_scalar("SELECT icon_key FROM servers WHERE id = ?").bind(id).fetch_one(&state.inner.db).await?;
    sqlx::query("UPDATE servers SET icon_key = ? WHERE id = ?").bind(&key).bind(id).execute(&state.inner.db).await?;
    if let Some(old) = old {
        images::release(state, &old).await;
    }
    publish_server(state, id).await
}

/// `PUT /servers/{id}/banner` — своя картинка баннера 960×480.
pub async fn put_banner(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>, body: Bytes) -> AppResult<Json<Server>> {
    me_on(&state, id, &user).await?.require(perm::MANAGE_SERVER)?;
    state.inner.media_limiter.check(user.id).map_err(|retry_after_secs| AppError::RateLimited { retry_after_secs })?;
    let _images = state.inner.image_lock.lock().await; // сериализует запись файла, смену ключа и очистку (S12)
    let key = images::store_banner_image(&state, &body).await?;
    set_banner_image(&state, id, Some(key)).await?;
    Ok(Json(server_for(&state.inner.db, id, user.id).await?))
}

/// `DELETE /servers/{id}/banner` — убрать свою картинку (останется узор).
pub async fn delete_banner(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> AppResult<Json<Server>> {
    me_on(&state, id, &user).await?.require(perm::MANAGE_SERVER)?;
    let _images = state.inner.image_lock.lock().await; // сериализует запись файла, смену ключа и очистку (S12)
    set_banner_image(&state, id, None).await?;
    Ok(Json(server_for(&state.inner.db, id, user.id).await?))
}

async fn set_banner_image(state: &AppState, id: i64, key: Option<String>) -> AppResult<()> {
    let old: Option<String> = sqlx::query_scalar("SELECT banner_key FROM servers WHERE id = ?").bind(id).fetch_one(&state.inner.db).await?;
    sqlx::query("UPDATE servers SET banner_key = ? WHERE id = ?").bind(&key).bind(id).execute(&state.inner.db).await?;
    if let Some(old) = old {
        images::release(state, &old).await;
    }
    publish_server(state, id).await
}

/// `DELETE /servers/{id}` — только владелец.
pub async fn delete(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> AppResult<StatusCode> {
    me_on(&state, id, &user).await?.require_owner()?;
    let db = &state.inner.db;
    let members = member_ids(db, id).await?;
    let mut keys: Vec<String> = sqlx::query_scalar(
        "SELECT a.storage_key FROM attachments a JOIN messages m ON m.id = a.message_id
         JOIN channels c ON c.id = m.channel_id WHERE c.server_id = ?",
    )
    .bind(id)
    .fetch_all(db)
    .await?;
    let (icon, banner): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT icon_key, banner_key FROM servers WHERE id = ?").bind(id).fetch_one(db).await?;
    keys.extend(icon);
    keys.extend(banner);
    for roster in state.inner.presence.of_server(id) {
        voice::clear_room(&state, &voice::room_name(id, roster.channel_id)).await;
    }
    sqlx::query("DELETE FROM servers WHERE id = ?").bind(id).execute(db).await?;
    crate::chat::remove_files(&state, &keys).await;
    tracing::info!(server_id = id, user_id = user.id, "server deleted");
    publish(&state, Audience::Users(members), ServerEvent::ServerRemoved { server_id: id });
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /servers/{id}/transfer` — владелец отдаёт сервер участнику; сам становится админом.
pub async fn transfer(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    ApiJson(req): ApiJson<TransferRequest>,
) -> AppResult<Json<Server>> {
    let me = me_on(&state, id, &user).await?;
    me.require_owner()?;
    if req.user_id == user.id {
        return Err(AppError::BadRequest("вы уже владелец".into()));
    }
    let heir = p::member(&state.inner.db, id, req.user_id).await?;
    let role = |pos: i32| {
        sqlx::query_scalar::<_, i64>("SELECT id FROM server_roles WHERE server_id = ? AND position = ?").bind(id).bind(pos)
    };
    let owner_role = role(p::OWNER_POSITION).fetch_one(&state.inner.db).await?;
    let admin_role = role(2).fetch_one(&state.inner.db).await?;
    let mut tx = state.inner.db.begin().await?;
    sqlx::query("UPDATE servers SET owner_id = ? WHERE id = ?").bind(heir.user_id).bind(id).execute(&mut *tx).await?;
    for (uid, rid) in [(heir.user_id, owner_role), (user.id, admin_role)] {
        sqlx::query("UPDATE server_members SET role_id = ? WHERE server_id = ? AND user_id = ?")
            .bind(rid)
            .bind(id)
            .bind(uid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    tracing::info!(server_id = id, from = user.id, to = heir.user_id, "ownership transferred");
    publish_server(&state, id).await?;
    publish_member(&state, id, heir.user_id).await?;
    publish_member(&state, id, user.id).await?;
    Ok(Json(server_for(&state.inner.db, id, user.id).await?))
}

/// `GET /servers/{id}/members`
pub async fn members(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> AppResult<Json<Vec<Member>>> {
    me_on(&state, id, &user).await?;
    type Row = (i64, String, Option<String>, Option<String>, i64, Option<String>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT u.id, u.name, u.display_name, u.avatar_key, m.role_id, u.status
         FROM server_members m JOIN users u ON u.id = m.user_id WHERE m.server_id = ? ORDER BY m.joined_at, u.id",
    )
    .bind(id)
    .fetch_all(&state.inner.db)
    .await?;
    let online = &state.inner.online;
    Ok(Json(
        rows.into_iter()
            .map(|(uid, name, display_name, avatar, role_id, status)| Member {
                user: PublicUser { id: uid, name, display_name, avatar },
                role_id,
                online: online.is_online(uid),
                activity: online.activity(uid, status),
            })
            .collect(),
    ))
}

/// Убрать человека с сервера (выход, кик, бан): из голоса, из участников, события.
pub(crate) async fn drop_member(state: &AppState, server_id: i64, user_id: i64) -> AppResult<()> {
    voice::kick_from_server_voice(state, server_id, user_id).await;
    sqlx::query("DELETE FROM server_members WHERE server_id = ? AND user_id = ?")
        .bind(server_id)
        .bind(user_id)
        .execute(&state.inner.db)
        .await?;
    // Приглашения ушедшего (кик, бан, выход) больше не действуют.
    sqlx::query("UPDATE invites SET revoked = 1 WHERE server_id = ? AND created_by = ?")
        .bind(server_id)
        .bind(user_id)
        .execute(&state.inner.db)
        .await?;
    publish(state, Audience::User(user_id), ServerEvent::ServerRemoved { server_id });
    publish(state, Audience::Server(server_id), ServerEvent::MemberRemoved { server_id, user_id });
    Ok(())
}

/// `DELETE /servers/{id}/members/@me` — выйти с сервера.
pub async fn leave(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> AppResult<StatusCode> {
    let me = me_on(&state, id, &user).await?;
    if me.is_owner {
        return Err(AppError::BadRequest("сначала передайте владение или удалите сервер".into()));
    }
    drop_member(&state, id, user.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- роли и модерация

/// Я и цель на сервере; действовать можно только над ролью строго ниже своей.
async fn me_and_target(state: &AppState, server_id: i64, user: &AuthUser, target: i64, need: u32) -> AppResult<(MemberCtx, MemberCtx)> {
    let me = me_on(state, server_id, user).await?;
    me.require(need)?;
    let t = p::member(&state.inner.db, server_id, target).await?;
    if !me.can_act_on(&t) {
        return Err(AppError::Forbidden);
    }
    Ok((me, t))
}

/// `PUT /servers/{id}/members/{uid}/role`
pub async fn set_role(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, uid)): Path<(i64, i64)>,
    ApiJson(req): ApiJson<SetRoleRequest>,
) -> AppResult<Json<Member>> {
    let (me, _) = me_and_target(&state, id, &user, uid, perm::MANAGE_ROLES).await?;
    let pos: Option<i32> = sqlx::query_scalar("SELECT position FROM server_roles WHERE id = ? AND server_id = ?")
        .bind(req.role_id)
        .bind(id)
        .fetch_optional(&state.inner.db)
        .await?;
    let pos = pos.ok_or_else(|| AppError::BadRequest("нет такой роли на сервере".into()))?;
    if !me.can_assign(pos) {
        return Err(AppError::Forbidden);
    }
    sqlx::query("UPDATE server_members SET role_id = ? WHERE server_id = ? AND user_id = ?")
        .bind(req.role_id)
        .bind(id)
        .bind(uid)
        .execute(&state.inner.db)
        .await?;
    tracing::info!(server_id = id, by = user.id, user_id = uid, role_id = req.role_id, "role changed");
    publish_member(&state, id, uid).await?;
    // у самого человека меняется my_role_id в сервере
    let server = server_for(&state.inner.db, id, uid).await?;
    publish(&state, Audience::User(uid), ServerEvent::ServerUpserted { server });
    Ok(Json(member_json(&state, id, uid).await?))
}

/// `DELETE /servers/{id}/members/{uid}` — кик.
pub async fn kick(State(state): State<AppState>, user: AuthUser, Path((id, uid)): Path<(i64, i64)>) -> AppResult<StatusCode> {
    me_and_target(&state, id, &user, uid, perm::KICK).await?;
    drop_member(&state, id, uid).await?;
    tracing::info!(server_id = id, by = user.id, user_id = uid, "kicked");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /servers/{id}/members/{uid}/disconnect` — выкинуть из голоса, оставив на сервере.
pub async fn disconnect(State(state): State<AppState>, user: AuthUser, Path((id, uid)): Path<(i64, i64)>) -> AppResult<StatusCode> {
    me_and_target(&state, id, &user, uid, perm::DISCONNECT).await?;
    voice::kick_from_server_voice(&state, id, uid).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /servers/{id}/bans`
pub async fn bans(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> AppResult<Json<Vec<Ban>>> {
    me_on(&state, id, &user).await?.require(perm::BAN)?;
    type Row = (i64, String, Option<String>, Option<String>, i64, Option<String>, i64);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT u.id, u.name, u.display_name, u.avatar_key, b.by_user, b.reason, b.created_at
         FROM server_bans b JOIN users u ON u.id = b.user_id WHERE b.server_id = ? ORDER BY b.created_at DESC",
    )
    .bind(id)
    .fetch_all(&state.inner.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(uid, name, display_name, avatar, by, reason, created_at)| Ban {
                user: PublicUser { id: uid, name, display_name, avatar },
                by,
                reason,
                created_at,
            })
            .collect(),
    ))
}

/// `POST /servers/{id}/bans` — бан: вылет с сервера и из голоса, вернуться нельзя.
pub async fn ban(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>, ApiJson(req): ApiJson<BanRequest>) -> AppResult<StatusCode> {
    me_and_target(&state, id, &user, req.user_id, perm::BAN).await?;
    let reason = req.reason.map(|r| r.trim().chars().take(200).collect::<String>()).filter(|r| !r.is_empty());
    sqlx::query("INSERT OR REPLACE INTO server_bans (server_id, user_id, by_user, reason, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(id)
        .bind(req.user_id)
        .bind(user.id)
        .bind(reason)
        .bind(now())
        .execute(&state.inner.db)
        .await?;
    drop_member(&state, id, req.user_id).await?;
    tracing::info!(server_id = id, by = user.id, user_id = req.user_id, "banned");
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /servers/{id}/bans/{uid}`
pub async fn unban(State(state): State<AppState>, user: AuthUser, Path((id, uid)): Path<(i64, i64)>) -> AppResult<StatusCode> {
    me_on(&state, id, &user).await?.require(perm::BAN)?;
    let n = sqlx::query("DELETE FROM server_bans WHERE server_id = ? AND user_id = ?")
        .bind(id)
        .bind(uid)
        .execute(&state.inner.db)
        .await?
        .rows_affected();
    if n == 0 { Err(AppError::NotFound) } else { Ok(StatusCode::NO_CONTENT) }
}
