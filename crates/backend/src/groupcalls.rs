//! Групповые звонки: комната `call-{id}`, не привязана к ЛС. Звонок создаёт и пополняет тот, кто уже в нём;
//! звать можно только своих друзей, всего не больше `limits::GROUP_CALL_MAX`. Состояние — в памяти (как звонки в ЛС):
//! звонок живёт, пока в комнате кто-то есть или кому-то звонят.

use std::{
    collections::{BTreeSet, HashMap},
    sync::{Mutex, atomic::{AtomicI64, Ordering}},
    time::Duration,
};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use livekit_api::access_token::{AccessToken, VideoGrants};
use proto::{CallInviteRequest, GroupCall, GroupCallJoin, JoinResponse, ServerEvent, limits};

use crate::{
    AppState, LIVEKIT_TOKEN_TTL,
    error::{AppError, AppResult},
    events::{Audience, publish},
    extract::{ApiJson, AuthUser},
    now, voice,
};

/// Имя комнаты звонка.
pub fn room(id: i64) -> String {
    format!("call-{id}")
}

/// `call-7` → 7.
pub fn parse_room(name: &str) -> Option<i64> {
    name.strip_prefix("call-")?.parse().ok()
}

struct Call {
    host: i64,
    started_at: i64,
    /// Все, кому разрешён вход (хозяин и все, кого звали).
    allowed: BTreeSet<i64>,
    /// Кому звонят сейчас: человек → когда начали звонить.
    ringing: HashMap<i64, i64>,
    /// В комнате уже кто-то был (после этого пустая комната = конец звонка).
    seen: bool,
    /// Пока никто не подключился (токен выдан, LiveKit ещё соединяется) — звонок не закрывать до этого времени.
    hold_until: i64,
}

#[derive(Default)]
pub struct GroupCalls {
    calls: Mutex<HashMap<i64, Call>>,
    seq: AtomicI64,
}

impl GroupCalls {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Call>> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn next_id(&self) -> i64 {
        now() * 1000 + self.seq.fetch_add(1, Ordering::Relaxed) % 1000
    }

    /// Может ли человек зайти в комнату этого звонка.
    pub fn may_join(&self, id: i64, user: i64) -> bool {
        self.lock().get(&id).is_some_and(|c| c.allowed.contains(&user))
    }
}

fn view(id: i64, c: &Call, roster: Vec<i64>) -> GroupCall {
    let mut ringing: Vec<i64> = c.ringing.keys().copied().collect();
    ringing.sort_unstable();
    GroupCall { id, host_id: c.host, started_at: c.started_at, ringing, participants: roster }
}

fn announce(state: &AppState, id: i64, audience: Vec<i64>, call: Option<GroupCall>) {
    publish(state, Audience::Users(audience), ServerEvent::GroupCall { id, call });
}

/// Разослать текущее состояние (или конец звонка) всем, кому он разрешён.
fn broadcast(state: &AppState, id: i64) {
    let roster = state.inner.presence.roster(&room(id));
    let (audience, call) = {
        let mut calls = state.inner.group_calls.lock();
        let Some(c) = calls.get_mut(&id) else { return };
        c.ringing.retain(|u, _| !roster.contains(u));
        c.seen |= !roster.is_empty();
        let audience: Vec<i64> = c.allowed.iter().copied().collect();
        if roster.is_empty() && c.ringing.is_empty() && (c.seen || now() >= c.hold_until) {
            calls.remove(&id);
            (audience, None)
        } else {
            let view = view(id, &calls[&id], roster);
            (audience, Some(view))
        }
    };
    announce(state, id, audience, call);
}

/// Состав комнаты изменился (вебхук, сверка).
pub fn room_changed(state: &AppState, id: i64) {
    broadcast(state, id);
}

/// Звонок для меня: идёт, и я могу зайти.
pub fn mine(state: &AppState, user: i64) -> Vec<GroupCall> {
    let ids: Vec<i64> = state.inner.group_calls.lock().iter().filter(|(_, c)| c.allowed.contains(&user)).map(|(id, _)| *id).collect();
    ids.into_iter()
        .filter_map(|id| {
            let roster = state.inner.presence.roster(&room(id));
            state.inner.group_calls.lock().get(&id).map(|c| view(id, c, roster))
        })
        .collect()
}

/// Кого из `wanted` можно позвать: убрать себя и повторы; ошибка, если в звонке станет больше предела.
pub fn plan_invite(me: i64, present: &BTreeSet<i64>, ringing: &BTreeSet<i64>, wanted: &[i64]) -> Result<Vec<i64>, String> {
    let mut fresh: Vec<i64> = Vec::new();
    for u in wanted {
        if *u != me && !present.contains(u) && !fresh.contains(u) {
            fresh.push(*u);
        }
    }
    if fresh.is_empty() {
        return Err("некого звать".into());
    }
    let mut total: BTreeSet<i64> = present.union(ringing).copied().collect();
    total.insert(me);
    total.extend(fresh.iter().copied());
    if total.len() > limits::GROUP_CALL_MAX {
        return Err(format!("в звонке не больше {} человек", limits::GROUP_CALL_MAX));
    }
    Ok(fresh)
}

async fn require_friends(state: &AppState, me: i64, users: &[i64]) -> AppResult<()> {
    for u in users {
        let friends: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM friendships WHERE status = 'accepted'
             AND ((requester_id = ?1 AND addressee_id = ?2) OR (requester_id = ?2 AND addressee_id = ?1))",
        )
        .bind(me)
        .bind(u)
        .fetch_one(&state.inner.db)
        .await?;
        if friends == 0 {
            return Err(AppError::Validation("звать в звонок можно только друзей".into()));
        }
    }
    Ok(())
}

fn token(state: &AppState, user: i64, name: &str, room: String) -> AppResult<JoinResponse> {
    let lk = &state.inner.livekit;
    let grants = VideoGrants {
        room_join: true,
        room: room.clone(),
        can_publish: Some(true),
        can_subscribe: Some(true),
        can_publish_data: Some(true),
        can_publish_sources: ["microphone", "camera", "screen_share", "screen_share_audio"].map(String::from).to_vec(),
        can_update_own_metadata: Some(false),
        ..Default::default()
    };
    let token = AccessToken::with_api_key(&lk.api_key, &lk.api_secret)
        .with_identity(&user.to_string())
        .with_name(name)
        .with_grants(grants)
        .with_ttl(LIVEKIT_TOKEN_TTL)
        .to_jwt()?;
    Ok(JoinResponse { livekit_url: lk.public_url.clone(), livekit_token: token, room })
}

/// Токен выдан, но человек ещё соединяется с LiveKit: не закрывать звонок как пустой `HOLD_SECS`.
fn hold(state: &AppState, id: i64) {
    {
        let mut calls = state.inner.group_calls.lock();
        if let Some(c) = calls.get_mut(&id) {
            c.hold_until = c.hold_until.max(now() + HOLD_SECS);
        }
    }
    let st = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(HOLD_SECS as u64 + 1)).await;
        broadcast(&st, id);
    });
}

const HOLD_SECS: i64 = 20;

/// Позвонить: записать приглашения и через `CALL_RING_SECS` убрать тех, кто не зашёл.
fn ring(state: &AppState, id: i64, users: &[i64]) {
    let t = now();
    {
        let mut calls = state.inner.group_calls.lock();
        if let Some(c) = calls.get_mut(&id) {
            for u in users {
                c.allowed.insert(*u);
                c.ringing.insert(*u, t);
            }
        }
    }
    let (st, users) = (state.clone(), users.to_vec());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(limits::CALL_RING_SECS)).await;
        let changed = {
            let mut calls = st.inner.group_calls.lock();
            match calls.get_mut(&id) {
                Some(c) => {
                    let before = c.ringing.len();
                    c.ringing.retain(|u, started| !(users.contains(u) && *started == t));
                    c.ringing.len() != before
                }
                None => false,
            }
        };
        if changed {
            broadcast(&st, id);
        }
    });
    broadcast(state, id);
}

/// `POST /calls` — начать групповой звонок: позвать друзей, получить токен комнаты.
pub async fn create(State(state): State<AppState>, user: AuthUser, ApiJson(req): ApiJson<CallInviteRequest>) -> AppResult<(StatusCode, Json<GroupCallJoin>)> {
    state.inner.chat_limiter.check(user.id).map_err(|retry_after_secs| AppError::RateLimited { retry_after_secs })?;
    let who = plan_invite(user.id, &BTreeSet::new(), &BTreeSet::new(), &req.user_ids).map_err(AppError::Validation)?;
    require_friends(&state, user.id, &who).await?;
    let id = state.inner.group_calls.next_id();
    {
        let mut calls = state.inner.group_calls.lock();
        calls.insert(
            id,
            Call { host: user.id, started_at: now(), allowed: BTreeSet::from([user.id]), ringing: HashMap::new(), seen: false, hold_until: 0 },
        );
    }
    tracing::info!(call = id, host = user.id, invited = who.len(), "group call");
    ring(&state, id, &who);
    let join = enter(&state, id, user.id).await?;
    Ok((StatusCode::CREATED, Json(join)))
}

async fn enter(state: &AppState, id: i64, user: i64) -> AppResult<GroupCallJoin> {
    let me = crate::servers::public_user(&state.inner.db, user).await?;
    let roster = state.inner.presence.roster(&room(id));
    let call = {
        let calls = state.inner.group_calls.lock();
        let c = calls.get(&id).ok_or(AppError::NotFound)?;
        if !c.allowed.contains(&user) {
            return Err(AppError::NotFound);
        }
        if !roster.contains(&user) && roster.len() >= limits::GROUP_CALL_MAX {
            return Err(AppError::ChannelFull);
        }
        view(id, c, roster)
    };
    hold(state, id);
    let join = token(state, user, me.shown_name(), room(id))?;
    Ok(GroupCallJoin { call, join })
}

/// `POST /calls/{id}/join` — войти (ответить на звонок).
pub async fn join(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> AppResult<Json<GroupCallJoin>> {
    let j = enter(&state, id, user.id).await?;
    // зашедшему больше не звонят
    Ok(Json(j))
}

/// `POST /calls/{id}/invite` — позвать ещё друзей (может любой, кто в звонке).
pub async fn invite(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>, ApiJson(req): ApiJson<CallInviteRequest>) -> AppResult<StatusCode> {
    state.inner.chat_limiter.check(user.id).map_err(|retry_after_secs| AppError::RateLimited { retry_after_secs })?;
    let roster: BTreeSet<i64> = state.inner.presence.roster(&room(id)).into_iter().collect();
    let ringing: BTreeSet<i64> = {
        let calls = state.inner.group_calls.lock();
        let c = calls.get(&id).ok_or(AppError::NotFound)?;
        if !c.allowed.contains(&user.id) {
            return Err(AppError::NotFound);
        }
        if !roster.contains(&user.id) {
            return Err(AppError::Forbidden);
        }
        c.ringing.keys().copied().collect()
    };
    // кто уже в звонке — не звать; кому уже звонят — позвать заново (сбросить таймер)
    let who = plan_invite(user.id, &roster, &ringing, &req.user_ids).map_err(AppError::Validation)?;
    require_friends(&state, user.id, &who).await?;
    ring(&state, id, &who);
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /calls/{id}` — положить трубку: отклонить или выйти.
pub async fn hang_up(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> AppResult<StatusCode> {
    {
        let mut calls = state.inner.group_calls.lock();
        let c = calls.get_mut(&id).ok_or(AppError::NotFound)?;
        if !c.allowed.contains(&user.id) {
            return Err(AppError::NotFound);
        }
        c.ringing.remove(&user.id);
    }
    voice::left(&state, &room(id), user.id, None);
    broadcast(&state, id);
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /calls` — звонки, в которые меня звали и которые ещё идут.
pub async fn list(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Vec<GroupCall>>> {
    Ok(Json(mine(&state, user.id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &[i64]) -> BTreeSet<i64> {
        v.iter().copied().collect()
    }

    #[test]
    fn rooms() {
        assert_eq!(room(7), "call-7");
        assert_eq!(parse_room("call-7"), Some(7));
        assert_eq!(parse_room("dm-7"), None);
        assert_eq!(parse_room("call-"), None);
    }

    #[test]
    fn invite_skips_me_present_and_duplicates() {
        assert_eq!(plan_invite(1, &set(&[1, 2]), &set(&[]), &[1, 2, 3, 3, 4]), Ok(vec![3, 4]));
    }

    #[test]
    fn invite_nobody_is_error() {
        assert!(plan_invite(1, &set(&[1, 2]), &set(&[]), &[1, 2]).is_err());
    }

    #[test]
    fn invite_limit_is_eight_including_ringing() {
        let present = set(&[1, 2, 3]);
        let ringing = set(&[4, 5, 6]);
        assert_eq!(plan_invite(1, &present, &ringing, &[7, 8]), Ok(vec![7, 8]));
        assert!(plan_invite(1, &present, &ringing, &[7, 8, 9]).is_err());
    }
}
