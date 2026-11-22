//! Реакции на сообщения и стикеры.

mod common;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use common::*;
use proto::{AuthResponse, Channel, Message, ServerEvent};
use serde_json::{Value, json};
use tower::ServiceExt;

struct C {
    t: T,
    admin: String,
    user: String,
    user_id: i64,
    text: i64,
}

async fn setup_chat() -> C {
    let t = setup().await;
    let admin = t.admin().await;
    let inv = t.invite(&admin, json!({})).await;
    let user: AuthResponse = serde_json::from_value(t.register(&inv.code, "vasya").await.1).unwrap();
    let (s, v) = t.call(Method::POST, "/channels", Some(&admin), Some(json!({"name": "чат", "kind": "text"}))).await;
    assert_eq!(s, StatusCode::CREATED);
    let text = serde_json::from_value::<Channel>(v).unwrap().id;
    C { t, admin, user: user.token, user_id: user.user.id, text }
}

impl C {
    async fn say(&self, tok: &str, body: Value) -> (StatusCode, Value) {
        self.t.call(Method::POST, &format!("/channels/{}/messages", self.text), Some(tok), Some(body)).await
    }
    async fn react(&self, tok: &str, method: Method, mid: i64, name: &str) -> StatusCode {
        self.t.call(method, &format!("/messages/{mid}/reactions/{name}"), Some(tok), None).await.0
    }
    async fn history(&self, tok: &str) -> Vec<Message> {
        let (s, v) = self.t.call(Method::GET, &format!("/channels/{}/messages", self.text), Some(tok), None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        serde_json::from_value(v).unwrap()
    }
}

#[tokio::test]
async fn reactions_add_remove_and_history() {
    let c = setup_chat().await;
    let (_, v) = c.say(&c.user, json!({"content": "привет"})).await;
    let m: Message = serde_json::from_value(v).unwrap();
    assert!(m.reactions.is_empty());
    assert_eq!(c.react(&c.user, Method::PUT, m.id, "fire").await, StatusCode::NO_CONTENT);
    assert_eq!(c.react(&c.admin, Method::PUT, m.id, "fire").await, StatusCode::NO_CONTENT);
    assert_eq!(c.react(&c.admin, Method::PUT, m.id, "+1").await, StatusCode::NO_CONTENT);
    // повтор — не ошибка и не дубль
    assert_eq!(c.react(&c.user, Method::PUT, m.id, "fire").await, StatusCode::NO_CONTENT);
    let h = c.history(&c.user).await;
    let r = &h[0].reactions;
    assert_eq!(r.len(), 2, "{r:?}");
    let fire = r.iter().find(|x| x.name == "fire").unwrap();
    assert_eq!(fire.users.len(), 2);
    assert!(fire.users.contains(&c.user_id));
    // порядок — по первой реакции
    assert_eq!(r[0].name, "fire");
    assert_eq!(c.react(&c.user, Method::DELETE, m.id, "fire").await, StatusCode::NO_CONTENT);
    let h = c.history(&c.user).await;
    let fire = h[0].reactions.iter().find(|x| x.name == "fire").unwrap();
    assert_eq!(fire.users.len(), 1);
    assert!(!fire.users.contains(&c.user_id));
}

#[tokio::test]
async fn reaction_unknown_name_or_message_rejected() {
    let c = setup_chat().await;
    let (_, v) = c.say(&c.user, json!({"content": "x"})).await;
    let m: Message = serde_json::from_value(v).unwrap();
    assert_eq!(c.react(&c.user, Method::PUT, m.id, "skull-and-bones").await, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(c.react(&c.user, Method::PUT, 999_999, "fire").await, StatusCode::NOT_FOUND);
    // чужой: не участник сервера
    let inv = c.t.invite(&c.admin, json!({})).await;
    let other: AuthResponse = serde_json::from_value(c.t.register(&inv.code, "petya").await.1).unwrap();
    let (s, _) = c.t.call(Method::DELETE, "/servers/1/members/@me", Some(&other.token), None).await;
    assert!(s.is_success(), "{s}");
    assert_eq!(c.react(&other.token, Method::PUT, m.id, "fire").await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unicode_reaction_roundtrip() {
    let c = setup_chat().await;
    let (_, v) = c.say(&c.user, json!({"content": "x"})).await;
    let m: Message = serde_json::from_value(v).unwrap();
    assert_eq!(c.react(&c.user, Method::PUT, m.id, "%F0%9F%A6%86").await, StatusCode::NO_CONTENT);
    assert_eq!(c.history(&c.user).await[0].reactions[0].name, "🦆");
    // c: — зарезервировано под свои эмодзи сервера
    assert_eq!(c.react(&c.user, Method::PUT, m.id, "c:1").await, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(c.react(&c.user, Method::DELETE, m.id, "%F0%9F%A6%86").await, StatusCode::NO_CONTENT);
    assert!(c.history(&c.user).await[0].reactions.is_empty());
}

#[tokio::test]
async fn too_many_distinct_reactions() {
    let c = setup_chat().await;
    let (_, v) = c.say(&c.user, json!({"content": "x"})).await;
    let m: Message = serde_json::from_value(v).unwrap();
    let all: Vec<&str> = vec!["😀", "😃", "😄", "😁", "😆", "😅", "🤣", "😊", "😇", "🙂", "🙃", "😉", "😌", "😍", "🥰", "😘", "😗", "😙", "😚", "😋", "😛"];
    let enc = |e: &str| e.bytes().map(|b| format!("%{b:02X}")).collect::<String>();
    for (i, e) in all.iter().enumerate() {
        let tok = if i % 2 == 0 { &c.user } else { &c.admin };
        let want = if i < proto::REACTIONS_PER_MESSAGE_MAX { StatusCode::NO_CONTENT } else { StatusCode::UNPROCESSABLE_ENTITY };
        assert_eq!(c.react(tok, Method::PUT, m.id, &enc(e)).await, want, "{i} {e}");
    }
    // уже стоящую — можно поставить ещё одному человеку
    assert_eq!(c.react(&c.admin, Method::PUT, m.id, &enc(all[0])).await, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn reaction_event_published() {
    let c = setup_chat().await;
    let (_, v) = c.say(&c.user, json!({"content": "x"})).await;
    let m: Message = serde_json::from_value(v).unwrap();
    let req = Request::builder().uri("/events").header(header::AUTHORIZATION, format!("Bearer {}", c.admin)).body(Body::empty()).unwrap();
    let mut body = c.t.app.clone().oneshot(req).await.unwrap().into_body();
    let mut buf = String::new();
    assert_eq!(next_event(&mut body, &mut buf).await, ServerEvent::Hello);
    c.react(&c.user, Method::PUT, m.id, "heart").await;
    loop {
        match next_event(&mut body, &mut buf).await {
            ServerEvent::ReactionsChanged { channel_id, message_id, reactions } => {
                assert_eq!((channel_id, message_id), (c.text, m.id));
                assert_eq!(reactions.len(), 1);
                assert_eq!(reactions[0].users, vec![c.user_id]);
                break;
            }
            _ => continue,
        }
    }
}

#[tokio::test]
async fn sticker_messages() {
    let c = setup_chat().await;
    let (s, v) = c.say(&c.user, json!({"content": "[стикер]", "sticker": "cat-hello"})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let m: Message = serde_json::from_value(v).unwrap();
    assert_eq!(m.sticker.as_deref(), Some("cat-hello"));
    assert_eq!(c.history(&c.admin).await[0].sticker.as_deref(), Some("cat-hello"));
    // только стикер, без текста — можно
    let (s, _) = c.say(&c.user, json!({"sticker": "cat-sleep"})).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, _) = c.say(&c.user, json!({"content": "x", "sticker": "../etc/passwd"})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}
