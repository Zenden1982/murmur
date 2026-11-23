//! Присутствие в голосе: вебхуки LiveKit, сверка, `GET /servers/{id}/voice` (фаза 9.1).

mod common;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use base64::Engine;
use common::*;
use http_body_util::BodyExt;
use livekit_api::access_token::AccessToken;
use proto::VoiceRoster;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

fn signed(body: &str, secret: &str) -> String {
    let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body.as_bytes()));
    AccessToken::with_api_key(LK_KEY, secret).with_sha256(&hash).to_jwt().unwrap()
}

async fn hook_with(t: &T, body: String, secret: &str) -> StatusCode {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/livekit/webhook")
        .header(header::AUTHORIZATION, signed(&body, secret))
        .header(header::CONTENT_TYPE, "application/webhook+json")
        .body(Body::from(body))
        .unwrap();
    let resp = t.app.clone().oneshot(req).await.unwrap();
    let s = resp.status();
    let _ = resp.into_body().collect().await;
    s
}

async fn hook(t: &T, event: &str, room: &str, identity: &str, sid: &str) {
    let body = format!(r#"{{"event":"{event}","room":{{"name":"{room}"}},"participant":{{"identity":"{identity}","sid":"{sid}"}}}}"#);
    assert_eq!(hook_with(t, body, LK_SECRET).await, StatusCode::OK);
}

async fn voice_of(t: &T, tok: &str, server: i64) -> Vec<VoiceRoster> {
    let (s, v) = t.call(Method::GET, &format!("/servers/{server}/voice"), Some(tok), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    serde_json::from_value(v).unwrap()
}

/// Настоящий голосовой канал сервера (создаётся вместе с сервером): (комната, id канала).
async fn voice_room(t: &T, server: i64) -> (String, i64) {
    let ch: i64 = sqlx::query_scalar("SELECT id FROM channels WHERE server_id = ? AND kind = 'voice' ORDER BY id LIMIT 1")
        .bind(server)
        .fetch_one(&t.state.inner.db)
        .await
        .unwrap();
    (format!("srv-{server}-ch-{ch}"), ch)
}

#[tokio::test]
async fn webhook_bad_signature_401() {
    let t = setup().await;
    let body = r#"{"event":"participant_joined","room":{"name":"srv-1-ch-2"},"participant":{"identity":"5","sid":"PA_1"}}"#.to_string();
    assert_eq!(hook_with(&t, body, "другой-секрет-другой-секрет-другой-0000").await, StatusCode::UNAUTHORIZED);
    let req = Request::builder().method(Method::POST).uri("/livekit/webhook").body(Body::from("{}")).unwrap();
    assert_eq!(t.app.clone().oneshot(req).await.unwrap().status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn webhook_join_leave_updates_roster() {
    let t = setup().await;
    let (tok, me) = t.user("okabe").await;
    let srv = t.create_server(&tok, "Лаб").await;
    let (room, ch) = voice_room(&t, srv.id).await;
    hook(&t, "participant_joined", &room, &me.to_string(), "PA_1").await;
    assert_eq!(voice_of(&t, &tok, srv.id).await, vec![VoiceRoster { channel_id: ch, user_ids: vec![me] }]);
    hook(&t, "participant_left", &room, &me.to_string(), "PA_1").await;
    assert!(voice_of(&t, &tok, srv.id).await.is_empty());
    // чужие комнаты и мусор игнорируются без ошибок
    hook(&t, "participant_joined", "ch-7", "1", "PA_2").await;
    hook(&t, "participant_joined", &room, "бот", "PA_3").await;
    hook(&t, "track_published", &room, &me.to_string(), "PA_1").await;
    assert!(voice_of(&t, &tok, srv.id).await.is_empty());
}

#[tokio::test]
async fn left_before_joined_and_duplicates() {
    let t = setup().await;
    let (tok, me) = t.user("okabe").await;
    let srv = t.create_server(&tok, "Лаб").await;
    let (room, _) = voice_room(&t, srv.id).await;
    let id = me.to_string();
    hook(&t, "participant_left", &room, &id, "PA_1").await;
    hook(&t, "participant_joined", &room, &id, "PA_2").await;
    hook(&t, "participant_joined", &room, &id, "PA_2").await;
    assert_eq!(voice_of(&t, &tok, srv.id).await[0].user_ids, vec![me]);
    // переподключение: новый sid вошёл раньше, чем ушёл старый — человек остаётся
    hook(&t, "participant_joined", &room, &id, "PA_3").await;
    hook(&t, "participant_left", &room, &id, "PA_2").await;
    assert_eq!(voice_of(&t, &tok, srv.id).await[0].user_ids, vec![me]);
    hook(&t, "room_finished", &room, "", "").await;
    assert!(voice_of(&t, &tok, srv.id).await.is_empty());
}

#[tokio::test]
async fn reconcile_fixes_missed_left() {
    let t = setup().await;
    let (tok, me) = t.user("okabe").await;
    let srv = t.create_server(&tok, "Лаб").await;
    let (room, _) = voice_room(&t, srv.id).await;
    hook(&t, "participant_joined", &room, &me.to_string(), "PA_1").await;
    // вебхук о выходе потерялся; LiveKit говорит, что комната пуста
    backend::voice::reconcile(&t.state).await.unwrap();
    assert!(voice_of(&t, &tok, srv.id).await.is_empty());
}

#[tokio::test]
async fn voice_requires_membership() {
    let t = setup().await;
    let (tok, _) = t.user("okabe").await;
    let (other, _) = t.user("suzuha").await;
    let srv = t.create_server(&tok, "Лаб").await;
    let (s, _) = t.call(Method::GET, &format!("/servers/{}/voice", srv.id), Some(&other), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn webhook_join_without_access_is_kicked() {
    let t = setup().await;
    let (tok, me) = t.user("okabe").await;
    let (_, stranger) = t.user("suzuha").await; // не участник сервера (токен у него мог остаться с прошлых времён)
    let srv = t.create_server(&tok, "Лаб").await;
    let (room, _) = voice_room(&t, srv.id).await;
    hook(&t, "participant_joined", &room, &stranger.to_string(), "PA_9").await;
    assert!(voice_of(&t, &tok, srv.id).await.is_empty(), "чужой не должен попасть в состав");
    hook(&t, "participant_joined", &room, &me.to_string(), "PA_1").await;
    assert_eq!(voice_of(&t, &tok, srv.id).await[0].user_ids, vec![me]);
    // канала нет — в комнате делать нечего
    hook(&t, "participant_joined", &format!("srv-{}-ch-99999", srv.id), &me.to_string(), "PA_2").await;
    assert_eq!(voice_of(&t, &tok, srv.id).await.len(), 1);
}
