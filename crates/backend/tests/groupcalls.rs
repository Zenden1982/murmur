//! Групповые звонки: создание, приглашения только друзьям, вход, предел, конец.

mod common;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use common::*;
use proto::{GroupCall, GroupCallJoin, ServerEvent};
use serde_json::json;
use tower::ServiceExt;

struct Stream {
    body: Body,
    buf: String,
}

impl Stream {
    async fn open(t: &T, tok: &str) -> Stream {
        let req = Request::builder().uri("/events").header(header::AUTHORIZATION, format!("Bearer {tok}")).body(Body::empty()).unwrap();
        let resp = t.app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let mut s = Stream { body: resp.into_body(), buf: String::new() };
        assert_eq!(s.next().await, ServerEvent::Hello);
        s
    }
    async fn next(&mut self) -> ServerEvent {
        next_event(&mut self.body, &mut self.buf).await
    }
    async fn next_call(&mut self) -> Option<GroupCall> {
        loop {
            if let ServerEvent::GroupCall { call, .. } = self.next().await {
                return call;
            }
        }
    }
}

async fn befriend(t: &T, a: &str, b_name: &str, a_id: i64, b: &str) {
    let (s, v) = t.call(Method::POST, "/friends/requests", Some(a), Some(json!({"name": b_name}))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, v) = t.call(Method::POST, &format!("/friends/{a_id}/accept"), Some(b), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn group_call_lifecycle() {
    let t = setup().await;
    let (a, a_id) = t.user("okabe").await;
    let (b, b_id) = t.user("kurisu").await;
    let (c, c_id) = t.user("daru").await;
    let (_d, d_id) = t.user("faris").await;
    befriend(&t, &a, "kurisu", a_id, &b).await;
    befriend(&t, &a, "daru", a_id, &c).await;
    let mut sb = Stream::open(&t, &b).await;
    let mut sc = Stream::open(&t, &c).await;

    // не друг — нельзя; пустой список — нельзя
    let (s, _) = t.call(Method::POST, "/calls", Some(&a), Some(json!({"user_ids": [d_id]}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = t.call(Method::POST, "/calls", Some(&a), Some(json!({"user_ids": []}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    // a зовёт b
    let (s, v) = t.call(Method::POST, "/calls", Some(&a), Some(json!({"user_ids": [b_id]}))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let j: GroupCallJoin = serde_json::from_value(v).unwrap();
    let id = j.call.id;
    assert_eq!(j.join.room, format!("call-{id}"));
    assert_eq!(j.call.host_id, a_id);
    let call = sb.next_call().await.expect("звонок b");
    assert_eq!((call.ringing, call.host_id), (vec![b_id], a_id));

    // c не звали — не зайти
    let (s, _) = t.call(Method::POST, &format!("/calls/{id}/join"), Some(&c), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // b заходит
    let (s, v) = t.call(Method::POST, &format!("/calls/{id}/join"), Some(&b), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    t.set_voice(&format!("call-{id}"), &[a_id, b_id]).await;
    let call = sb.next_call().await.unwrap();
    assert_eq!((call.ringing.clone(), call.participants.clone()), (vec![], vec![a_id, b_id]));

    // позвать может только тот, кто в комнате; c — не в комнате
    let (s, _) = t.call(Method::POST, &format!("/calls/{id}/invite"), Some(&c), Some(json!({"user_ids": [a_id]}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // a зовёт c
    let (s, v) = t.call(Method::POST, &format!("/calls/{id}/invite"), Some(&a), Some(json!({"user_ids": [c_id]}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    assert_eq!(sc.next_call().await.unwrap().ringing, vec![c_id]);
    let (s, _) = t.call(Method::GET, "/calls", Some(&c), None).await;
    assert_eq!(s, StatusCode::OK);

    // c отклоняет — звонок продолжается у остальных
    assert_eq!(sb.next_call().await.unwrap().ringing, vec![c_id]);
    let (s, _) = t.call(Method::DELETE, &format!("/calls/{id}"), Some(&c), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(sb.next_call().await.unwrap().ringing.is_empty());

    // все вышли — звонка нет
    t.set_voice(&format!("call-{id}"), &[]).await;
    assert_eq!(sb.next_call().await, None);
    let (s, _) = t.call(Method::POST, &format!("/calls/{id}/join"), Some(&b), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn group_call_limit_is_eight() {
    let t = setup().await;
    let (a, a_id) = t.user("okabe").await;
    let mut ids = Vec::new();
    for i in 0..8 {
        let name = format!("friend{i}");
        let (tok, id) = t.user(&name).await;
        befriend(&t, &a, &name, a_id, &tok).await;
        ids.push(id);
    }
    // 8 друзей + я = 9 — много
    let (s, _) = t.call(Method::POST, "/calls", Some(&a), Some(json!({"user_ids": ids}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    // 7 друзей + я = 8 — можно
    let (s, v) = t.call(Method::POST, "/calls", Some(&a), Some(json!({"user_ids": &ids[..7]}))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let id = serde_json::from_value::<GroupCallJoin>(v).unwrap().call.id;
    let (s, _) = t.call(Method::POST, &format!("/calls/{id}/invite"), Some(&a), Some(json!({"user_ids": [ids[7]]}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "пригласить может только тот, кто уже в комнате");
    t.set_voice(&format!("call-{id}"), &[a_id]).await;
    let (s, _) = t.call(Method::POST, &format!("/calls/{id}/invite"), Some(&a), Some(json!({"user_ids": [ids[7]]}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "девятый не помещается");
}
