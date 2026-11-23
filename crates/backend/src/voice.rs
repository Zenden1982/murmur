//! Кто сейчас в голосовых каналах: вебхуки LiveKit + сверка через RoomService.
//! Комната голосового канала — `srv-{server}-ch-{channel}`; identity участника — id пользователя.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    pin::Pin,
    sync::Mutex,
};

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
};
use livekit_api::{access_token::TokenVerifier, services::room::RoomClient, webhooks::WebhookReceiver};
use proto::{ServerEvent, VoiceRoster};

use crate::{
    AppState,
    error::{AppError, AppResult},
    events::{Audience, publish},
    extract::AuthUser,
};

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
/// Комнаты и identity участников в них.
pub type RoomListing = Vec<(String, Vec<String>)>;

/// Комнаты LiveKit (RoomService). В тестах — мок.
pub trait Rooms: Send + Sync {
    /// Все комнаты и identity участников в них.
    fn list(&self) -> BoxFut<'_, anyhow::Result<RoomListing>>;
    /// Выкинуть участника из комнаты.
    fn remove<'a>(&'a self, room: &'a str, identity: &'a str) -> BoxFut<'a, anyhow::Result<()>>;
}

/// Без LiveKit (по умолчанию до настройки): ничего не знает и никого не выкидывает.
pub struct NoRooms;

impl Rooms for NoRooms {
    fn list(&self) -> BoxFut<'_, anyhow::Result<RoomListing>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn remove<'a>(&'a self, _room: &'a str, _identity: &'a str) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// Имя комнаты голосового канала.
pub fn room_name(server_id: i64, channel_id: i64) -> String {
    format!("srv-{server_id}-ch-{channel_id}")
}

/// `srv-3-ch-7` → (3, 7). Чужие и старые (`ch-7`) имена — None.
pub fn parse_room(name: &str) -> Option<(i64, i64)> {
    let rest = name.strip_prefix("srv-")?;
    let (s, c) = rest.split_once("-ch-")?;
    Some((s.parse().ok()?, c.parse().ok()?))
}

/// Наша комната: голосовой канал сервера или звонок в ЛС.
pub fn is_ours(name: &str) -> bool {
    parse_room(name).is_some() || crate::dm::parse_dm_room(name).is_some() || crate::groupcalls::parse_room(name).is_some()
}

/// Присутствие: комната → (user id → sid участника LiveKit; "" — неизвестен, после сверки).
#[derive(Default)]
pub struct Presence {
    rooms: Mutex<HashMap<String, BTreeMap<i64, String>>>,
}

impl Presence {
    pub fn roster(&self, room: &str) -> Vec<i64> {
        self.rooms.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(room).map(|s| s.keys().copied().collect()).unwrap_or_default()
    }

    /// Все непустые голосовые каналы сервера.
    pub fn of_server(&self, server_id: i64) -> Vec<VoiceRoster> {
        let rooms = self.rooms.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut v: Vec<VoiceRoster> = rooms
            .iter()
            .filter_map(|(room, ids)| {
                let (s, c) = parse_room(room)?;
                (s == server_id && !ids.is_empty()).then(|| VoiceRoster { channel_id: c, user_ids: ids.keys().copied().collect() })
            })
            .collect();
        v.sort_by_key(|r| r.channel_id);
        v
    }

    /// Комнаты сервера, где сейчас этот человек.
    pub fn rooms_of(&self, server_id: i64, user_id: i64) -> Vec<String> {
        self.rooms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(room, ids)| parse_room(room).is_some_and(|(s, _)| s == server_id) && ids.contains_key(&user_id))
            .map(|(r, _)| r.clone())
            .collect()
    }

    /// Изменить состав комнаты; true — если поменялся список людей.
    fn update(&self, room: &str, f: impl FnOnce(&mut BTreeMap<i64, String>)) -> bool {
        let mut rooms = self.rooms.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let set = rooms.entry(room.to_string()).or_default();
        let before: Vec<i64> = set.keys().copied().collect();
        f(set);
        let changed = set.keys().copied().ne(before.iter().copied());
        if set.is_empty() {
            rooms.remove(room);
        }
        changed
    }
}

fn broadcast_roster(state: &AppState, room: &str) {
    if let Some(dm) = crate::dm::parse_dm_room(room) {
        crate::dm::room_changed(state, dm);
        return;
    }
    if let Some(call) = crate::groupcalls::parse_room(room) {
        crate::groupcalls::room_changed(state, call);
        return;
    }
    if let Some((server_id, channel_id)) = parse_room(room) {
        let user_ids = state.inner.presence.roster(room);
        publish(state, Audience::Server(server_id), ServerEvent::Voice { server_id, roster: VoiceRoster { channel_id, user_ids } });
    }
}

pub fn joined(state: &AppState, room: &str, user_id: i64, sid: &str) {
    if state.inner.presence.update(room, |s| {
        s.insert(user_id, sid.to_string());
    }) {
        broadcast_roster(state, room);
    }
}

/// Вышел участник с этим sid. Если человек уже переподключился (другой sid) — он остаётся.
/// `sid = None` — убрать безусловно (кик).
pub fn left(state: &AppState, room: &str, user_id: i64, sid: Option<&str>) {
    if state.inner.presence.update(room, |s| {
        let same = match (s.get(&user_id), sid) {
            (Some(cur), Some(sid)) => cur.is_empty() || cur == sid,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if same {
            s.remove(&user_id);
        }
    }) {
        broadcast_roster(state, room);
    }
}

pub fn room_finished(state: &AppState, room: &str) {
    if state.inner.presence.update(room, |s| s.clear()) {
        broadcast_roster(state, room);
    }
}

/// Сверить присутствие со списком LiveKit (потерянные вебхуки, перезапуск бэкенда).
pub async fn reconcile(state: &AppState) -> anyhow::Result<()> {
    let listing = state.inner.rooms.list().await?;
    let changed: Vec<String> = {
        let mut rooms = state.inner.presence.rooms.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut fresh: HashMap<String, BTreeMap<i64, String>> = HashMap::new();
        for (room, ids) in listing {
            if !is_ours(&room) {
                continue;
            }
            let old = rooms.get(&room);
            let set: BTreeMap<i64, String> = ids
                .iter()
                .filter_map(|i| i.parse().ok())
                .map(|uid| (uid, old.and_then(|o| o.get(&uid)).cloned().unwrap_or_default()))
                .collect();
            if !set.is_empty() {
                fresh.insert(room, set);
            }
        }
        let ids_of = |m: Option<&BTreeMap<i64, String>>| m.map(|m| m.keys().copied().collect::<Vec<_>>()).unwrap_or_default();
        let mut keys: BTreeSet<String> = rooms.keys().cloned().collect();
        keys.extend(fresh.keys().cloned());
        let changed = keys.into_iter().filter(|k| ids_of(rooms.get(k)) != ids_of(fresh.get(k))).collect();
        *rooms = fresh;
        changed
    };
    for room in changed {
        broadcast_roster(state, &room);
    }
    Ok(())
}

/// Выкинуть человека из всех голосовых комнат сервера (кик, бан, удаление).
pub async fn kick_from_server_voice(state: &AppState, server_id: i64, user_id: i64) {
    for room in state.inner.presence.rooms_of(server_id, user_id) {
        remove_from_room(state, &room, user_id).await;
    }
}

pub async fn remove_from_room(state: &AppState, room: &str, user_id: i64) {
    if let Err(e) = state.inner.rooms.remove(room, &user_id.to_string()).await {
        tracing::warn!(%room, user_id, error = ?e, "не удалось выкинуть из комнаты");
    }
    left(state, room, user_id, None);
}

/// Очистить комнату целиком (канал или сервер удалён).
pub async fn clear_room(state: &AppState, room: &str) {
    for uid in state.inner.presence.roster(room) {
        remove_from_room(state, room, uid).await;
    }
}

// ---------------------------------------------------------------- HTTP

/// `POST /livekit/webhook` — события LiveKit (подпись: JWT с sha256 тела, ключ API).
pub async fn webhook(State(state): State<AppState>, headers: HeaderMap, body: String) -> AppResult<StatusCode> {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v).trim())
        .filter(|v| !v.is_empty())
        .ok_or(AppError::Unauthorized)?;
    let lk = &state.inner.livekit;
    let receiver = WebhookReceiver::new(TokenVerifier::with_api_key(&lk.api_key, &lk.api_secret));
    let ev = receiver.receive(&body, auth).map_err(|e| {
        tracing::warn!(error = %e, "webhook rejected");
        AppError::Unauthorized
    })?;
    let room = ev.room.as_ref().map(|r| r.name.as_str()).unwrap_or("");
    if !is_ours(room) {
        return Ok(StatusCode::OK);
    }
    let participant = ev.participant.as_ref();
    let uid = participant.and_then(|p| p.identity.parse::<i64>().ok());
    let sid = participant.map(|p| p.sid.as_str()).unwrap_or("");
    match (ev.event.as_str(), uid) {
        ("participant_joined", Some(uid)) => {
            // Токен мог быть выдан до кика/бана или подобран: пускаем только тех, у кого сейчас есть доступ.
            if may_be_in_room(&state, room, uid).await {
                joined(&state, room, uid, sid)
            } else {
                tracing::warn!(%room, user_id = uid, "участник без доступа в комнате — выкидываю");
                remove_from_room(&state, room, uid).await;
            }
        }
        ("participant_left" | "participant_connection_aborted", Some(uid)) => left(&state, room, uid, Some(sid)),
        ("room_finished", _) => room_finished(&state, room),
        _ => {}
    }
    tracing::debug!(event = %ev.event, %room, ?uid, "livekit webhook");
    Ok(StatusCode::OK)
}

/// Есть ли у пользователя сейчас доступ в эту комнату (голосовой канал сервера или звонок в ЛС).
async fn may_be_in_room(state: &AppState, room: &str, user_id: i64) -> bool {
    if let Some((server_id, channel_id)) = parse_room(room) {
        return match crate::perm::channel_member(&state.inner.db, channel_id, user_id).await {
            Ok((m, row)) => m.server_id == server_id && row.kind == "voice",
            Err(_) => false,
        };
    }
    if let Some(dm) = crate::dm::parse_dm_room(room) {
        return matches!(crate::dm::dm_member(&state.inner.db, dm, user_id).await, Ok(Some(_)));
    }
    if let Some(call) = crate::groupcalls::parse_room(room) {
        return state.inner.group_calls.may_join(call, user_id);
    }
    false
}

/// `GET /servers/{id}/voice` — кто в каких голосовых каналах сервера.
pub async fn server_voice(State(state): State<AppState>, user: AuthUser, Path(id): Path<i64>) -> AppResult<Json<Vec<VoiceRoster>>> {
    crate::perm::member(&state.inner.db, id, user.id).await?;
    Ok(Json(state.inner.presence.of_server(id)))
}

/// Настоящий RoomService LiveKit.
pub struct LiveKitRooms {
    client: RoomClient,
}

impl LiveKitRooms {
    /// `api_url` — https-адрес LiveKit (пусто — из публичного wss-адреса).
    pub fn new(api_url: &str, lk: &crate::config::LiveKitConfig) -> Self {
        let host = if api_url.is_empty() {
            lk.public_url.replacen("wss://", "https://", 1).replacen("ws://", "http://", 1)
        } else {
            api_url.to_string()
        };
        Self { client: RoomClient::with_api_key(&host, &lk.api_key, &lk.api_secret) }
    }
}

impl Rooms for LiveKitRooms {
    fn list(&self) -> BoxFut<'_, anyhow::Result<RoomListing>> {
        Box::pin(async move {
            let mut out = Vec::new();
            for room in self.client.list_rooms(Vec::new()).await? {
                if !is_ours(&room.name) {
                    continue;
                }
                let ps = self.client.list_participants(&room.name).await?;
                out.push((room.name, ps.into_iter().map(|p| p.identity).collect()));
            }
            Ok(out)
        })
    }

    fn remove<'a>(&'a self, room: &'a str, identity: &'a str) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(async move { Ok(self.client.remove_participant(room, identity).await?) })
    }
}

/// Сверка при старте и раз в минуту.
pub fn spawn_reconciler(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        let mut shutdown = state.inner.shutdown.subscribe();
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Err(e) = reconcile(&state).await {
                        tracing::warn!(error = ?e, "сверка голосовых комнат не удалась");
                    }
                }
                r = shutdown.changed() => {
                    if r.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_room_cases() {
        assert_eq!(parse_room("srv-3-ch-7"), Some((3, 7)));
        assert_eq!(parse_room("ch-7"), None);
        assert_eq!(parse_room("srv-x-ch-7"), None);
        assert_eq!(parse_room("srv-3-ch-"), None);
        assert_eq!(room_name(3, 7), "srv-3-ch-7");
    }
}
