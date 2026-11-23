//! Друзья: заявки по логину (отправить, принять, отклонить/отменить), список с онлайн-статусом.
//! Онлайн = у человека открыт поток `GET /events` (клиент держит его, пока запущен и вошёл).
//! Изменения приходят обоим через `friends_changed` (клиент перечитывает `GET /friends`), онлайн — через `presence`.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use proto::{Activity, AddFriendRequest, Friend, FriendRequest, FriendsList, PublicUser, ServerEvent, limits};

use crate::{
    AppState,
    error::{AppError, AppResult},
    events::{Audience, publish},
    extract::{ApiJson, AuthUser},
    now,
};

/// Сколько потоков событий открыто у каждого человека, и во что он играет (только пока в сети).
#[derive(Default)]
pub struct Online {
    streams: std::sync::Mutex<HashMap<i64, usize>>,
    games: std::sync::Mutex<HashMap<i64, (String, i64)>>,
}

impl Online {
    /// Сколько потоков событий открыто сейчас у человека.
    pub fn streams_of(&self, user: i64) -> usize {
        self.streams.lock().unwrap_or_else(|e| e.into_inner()).get(&user).copied().unwrap_or(0)
    }

    pub fn is_online(&self, user: i64) -> bool {
        self.streams.lock().unwrap_or_else(|e| e.into_inner()).get(&user).is_some_and(|n| *n > 0)
    }

    /// Игра человека; None — убрать. true — изменилось.
    pub fn set_game(&self, user: i64, game: Option<(String, i64)>) -> bool {
        let mut m = self.games.lock().unwrap_or_else(|e| e.into_inner());
        let changed = m.get(&user) != game.as_ref();
        match game {
            Some(g) => m.insert(user, g),
            None => m.remove(&user),
        };
        changed
    }

    /// Активность для других: игра (если в сети) + свой статус. Не в сети — None.
    pub fn activity(&self, user: i64, status: Option<String>) -> Option<Activity> {
        if !self.is_online(user) {
            return None;
        }
        let g = self.games.lock().unwrap_or_else(|e| e.into_inner()).get(&user).cloned();
        let a = Activity { since: g.as_ref().map(|g| g.1), game: g.map(|g| g.0), status };
        (!a.is_empty()).then_some(a)
    }

    /// +1 поток; true — человек только что появился в сети.
    fn inc(&self, user: i64) -> bool {
        let mut m = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        let n = m.entry(user).or_insert(0);
        *n += 1;
        *n == 1
    }

    /// −1 поток; true — у человека больше нет потоков (вышел из сети).
    fn dec(&self, user: i64) -> bool {
        let mut m = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        match m.get_mut(&user) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            Some(_) => {
                m.remove(&user);
                drop(m);
                self.set_game(user, None);
                true
            }
            None => false,
        }
    }
}

/// Живёт, пока открыт поток событий человека: при создании и сбросе сообщает друзьям «в сети / не в сети».
pub struct OnlineGuard {
    state: AppState,
    user: i64,
}

impl OnlineGuard {
    pub fn new(state: &AppState, user: i64) -> Self {
        if state.inner.online.inc(user) {
            announce(state.clone(), user, true);
        }
        Self { state: state.clone(), user }
    }
}

impl Drop for OnlineGuard {
    fn drop(&mut self) {
        if self.state.inner.online.dec(self.user) {
            announce(self.state.clone(), self.user, false);
        }
    }
}

/// «В сети / не в сети» — друзьям и соседям по серверам (список участников).
fn announce(state: AppState, user: i64, online: bool) {
    let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
    rt.spawn(async move {
        if let Ok(mut ids) = crate::profile::neighbours(&state.inner.db, user).await {
            ids.retain(|&id| id != user);
            if !ids.is_empty() {
                publish(&state, Audience::Users(ids), ServerEvent::Presence { user_id: user, online });
            }
        }
    });
}

/// Id друзей (заявка принята).
pub(crate) async fn friend_ids(db: &sqlx::SqlitePool, user: i64) -> AppResult<Vec<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT CASE WHEN requester_id = ?1 THEN addressee_id ELSE requester_id END FROM friendships
         WHERE status = 'accepted' AND (requester_id = ?1 OR addressee_id = ?1)",
    )
    .bind(user)
    .fetch_all(db)
    .await?)
}

/// Строка пары в любом направлении: (кто отправил, статус).
async fn pair(db: &sqlx::SqlitePool, a: i64, b: i64) -> AppResult<Option<(i64, String)>> {
    Ok(sqlx::query_as(
        "SELECT requester_id, status FROM friendships
         WHERE (requester_id = ?1 AND addressee_id = ?2) OR (requester_id = ?2 AND addressee_id = ?1)",
    )
    .bind(a)
    .bind(b)
    .fetch_optional(db)
    .await?)
}

fn changed(state: &AppState, a: i64, b: i64) {
    publish(state, Audience::Users(vec![a, b]), ServerEvent::FriendsChanged);
}

async fn list(state: &AppState, me: i64) -> AppResult<FriendsList> {
    type Row = (i64, String, i64, String, Option<String>, Option<String>, i64, Option<String>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT f.requester_id, f.status, u.id, u.name, u.display_name, u.avatar_key, f.created_at, u.status
         FROM friendships f JOIN users u ON u.id = CASE WHEN f.requester_id = ?1 THEN f.addressee_id ELSE f.requester_id END
         WHERE f.requester_id = ?1 OR f.addressee_id = ?1
         ORDER BY u.name COLLATE NOCASE",
    )
    .bind(me)
    .fetch_all(&state.inner.db)
    .await?;
    let mut out = FriendsList::default();
    for (requester, status, id, name, display_name, avatar, created_at, own_status) in rows {
        let user = PublicUser { id, name, display_name, avatar };
        if status == "accepted" {
            let online = &state.inner.online;
            out.friends.push(Friend { online: online.is_online(id), activity: online.activity(id, own_status), user });
        } else if requester == me {
            out.outgoing.push(FriendRequest { user, created_at });
        } else {
            out.incoming.push(FriendRequest { user, created_at });
        }
    }
    Ok(out)
}

/// `GET /friends`
pub async fn get(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<FriendsList>> {
    Ok(Json(list(&state, user.id).await?))
}

async fn count(db: &sqlx::SqlitePool, sql: &'static str, user: i64) -> AppResult<usize> {
    let n: i64 = sqlx::query_scalar(sql).bind(user).fetch_one(db).await?;
    Ok(n as usize)
}

/// `POST /friends/requests` — заявка по логину. Если он уже звал меня — сразу дружба.
pub async fn request(State(state): State<AppState>, user: AuthUser, ApiJson(req): ApiJson<AddFriendRequest>) -> AppResult<(StatusCode, Json<FriendsList>)> {
    state.inner.chat_limiter.check(user.id).map_err(|retry_after_secs| AppError::RateLimited { retry_after_secs })?;
    let name = req.name.trim();
    let db = &state.inner.db;
    // логин точный; если такого нет — без учёта регистра
    let target: Option<i64> = match sqlx::query_scalar("SELECT id FROM users WHERE name = ?").bind(name).fetch_optional(db).await? {
        Some(id) => Some(id),
        None => sqlx::query_scalar("SELECT id FROM users WHERE name = ? COLLATE NOCASE LIMIT 1").bind(name).fetch_optional(db).await?,
    };
    let Some(target) = target else { return Err(AppError::NotFound) };
    if target == user.id {
        return Err(AppError::Validation("нельзя добавить в друзья самого себя".into()));
    }
    match pair(db, user.id, target).await? {
        Some((_, s)) if s == "accepted" => return Err(AppError::BadRequest("уже в друзьях".into())),
        // уже отправлял — ничего не делаем
        Some((from, _)) if from == user.id => return Ok((StatusCode::OK, Json(list(&state, user.id).await?))),
        // он уже звал меня — принимаем
        Some(_) => {
            sqlx::query("UPDATE friendships SET status = 'accepted' WHERE requester_id = ? AND addressee_id = ?")
                .bind(target)
                .bind(user.id)
                .execute(db)
                .await?;
        }
        None => {
            let pending = "SELECT COUNT(*) FROM friendships WHERE requester_id = ? AND status = 'pending'";
            if count(db, pending, user.id).await? >= limits::FRIEND_REQUESTS_PENDING_MAX {
                return Err(AppError::Validation(format!("не больше {} неотвеченных заявок", limits::FRIEND_REQUESTS_PENDING_MAX)));
            }
            let friends = "SELECT COUNT(*) FROM friendships WHERE status = 'accepted' AND (requester_id = ?1 OR addressee_id = ?1)";
            if count(db, friends, user.id).await? >= limits::FRIENDS_MAX {
                return Err(AppError::Validation(format!("не больше {} друзей", limits::FRIENDS_MAX)));
            }
            let inserted = sqlx::query("INSERT INTO friendships (requester_id, addressee_id, status, created_at) VALUES (?, ?, 'pending', ?)")
                .bind(user.id)
                .bind(target)
                .bind(now())
                .execute(db)
                .await;
            match inserted {
                Ok(_) => {}
                // Параллельный запрос уже создал заявку — это не ошибка.
                Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                    return Ok((StatusCode::OK, Json(list(&state, user.id).await?)));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    tracing::info!(from = user.id, to = target, "friend request");
    changed(&state, user.id, target);
    Ok((StatusCode::CREATED, Json(list(&state, user.id).await?)))
}

/// `POST /friends/{id}/accept` — принять входящую заявку.
pub async fn accept(State(state): State<AppState>, user: AuthUser, Path(other): Path<i64>) -> AppResult<Json<FriendsList>> {
    let done = sqlx::query("UPDATE friendships SET status = 'accepted' WHERE requester_id = ? AND addressee_id = ? AND status = 'pending'")
        .bind(other)
        .bind(user.id)
        .execute(&state.inner.db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    changed(&state, user.id, other);
    Ok(Json(list(&state, user.id).await?))
}

/// `DELETE /friends/{id}` — удалить из друзей, отклонить входящую или отменить исходящую заявку.
pub async fn remove(State(state): State<AppState>, user: AuthUser, Path(other): Path<i64>) -> AppResult<StatusCode> {
    let done = sqlx::query(
        "DELETE FROM friendships WHERE (requester_id = ?1 AND addressee_id = ?2) OR (requester_id = ?2 AND addressee_id = ?1)",
    )
    .bind(user.id)
    .bind(other)
    .execute(&state.inner.db)
    .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    changed(&state, user.id, other);
    Ok(StatusCode::NO_CONTENT)
}
