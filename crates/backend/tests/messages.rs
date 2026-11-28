//! Удаление сообщений и файлы любого формата.

mod common;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use common::*;
use proto::{Message, ServerEvent};
use serde_json::json;
use tower::ServiceExt;

struct S {
    t: T,
    owner: (String, i64),
    modr: (String, i64),
    member: (String, i64),
    other: (String, i64),
    text: i64,
}

async fn setup_server() -> S {
    let t = setup().await;
    let owner = t.user("owner").await;
    let modr = t.user("modr").await;
    let member = t.user("member").await;
    let other = t.user("other").await;
    let srv = t.create_server(&owner.0, "Лаб").await;
    t.add_member(srv.id, modr.1, 1).await;
    t.add_member(srv.id, member.1, 0).await;
    t.add_member(srv.id, other.1, 0).await;
    let (s, v) = t.call(Method::GET, &format!("/servers/{}/channels", srv.id), Some(&owner.0), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let list: proto::ChannelLayout = serde_json::from_value(v).unwrap();
    let text = list.channels.iter().find(|c| c.kind == proto::ChannelKind::Text).unwrap().id;
    S { t, owner, modr, member, other, text }
}

impl S {
    async fn say(&self, tok: &str, content: &str, attachments: Vec<i64>) -> Message {
        let (s, v) = self.t.call(Method::POST, &format!("/channels/{}/messages", self.text), Some(tok), Some(json!({"content": content, "attachments": attachments}))).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        serde_json::from_value(v).unwrap()
    }
    async fn del(&self, tok: &str, mid: i64) -> StatusCode {
        self.t.call(Method::DELETE, &format!("/messages/{mid}"), Some(tok), None).await.0
    }
    async fn history(&self, tok: &str) -> Vec<Message> {
        let (s, v) = self.t.call(Method::GET, &format!("/channels/{}/messages", self.text), Some(tok), None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        serde_json::from_value(v).unwrap()
    }
}

#[tokio::test]
async fn author_and_moderators_delete() {
    let s = setup_server().await;
    let a = s.say(&s.member.0, "моё", vec![]).await;
    let b = s.say(&s.member.0, "чужое для other", vec![]).await;
    let c = s.say(&s.owner.0, "владельца", vec![]).await;
    // обычный участник чужое — нельзя
    assert_eq!(s.del(&s.other.0, a.id).await, StatusCode::FORBIDDEN);
    // автор — можно, повтор — 404
    assert_eq!(s.del(&s.member.0, a.id).await, StatusCode::NO_CONTENT);
    assert_eq!(s.del(&s.member.0, a.id).await, StatusCode::NOT_FOUND);
    // модератор — чужое можно (MANAGE_MESSAGES)
    assert_eq!(s.del(&s.modr.0, b.id).await, StatusCode::NO_CONTENT);
    let h = s.history(&s.owner.0).await;
    assert_eq!(h.iter().map(|m| m.id).collect::<Vec<_>>(), vec![c.id]);
    // реакции на удалённое — 404
    let (st, _) = s.t.call(Method::PUT, &format!("/messages/{}/reactions/fire", a.id), Some(&s.owner.0), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // не участник сервера — 404
    let (stranger, _) = s.t.user("stranger").await;
    assert_eq!(s.del(&stranger, c.id).await, StatusCode::NOT_FOUND);
    assert_eq!(s.del(&s.owner.0, c.id).await, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn delete_removes_attachment_and_publishes_event() {
    let s = setup_server().await;
    let (st, v, _, _) = s.t.call_raw(Method::POST, "/uploads", Some(&s.member.0), include_bytes!("../../client/assets/icon-32.png").to_vec()).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let att = v["id"].as_i64().unwrap();
    let m = s.say(&s.member.0, "", vec![att]).await;
    let (st, _, _, _) = s.t.call_raw(Method::GET, &format!("/attachments/{att}"), Some(&s.other.0), vec![]).await;
    assert_eq!(st, StatusCode::OK);

    let req = Request::builder().uri("/events").header(header::AUTHORIZATION, format!("Bearer {}", s.other.0)).body(Body::empty()).unwrap();
    let mut body = s.t.app.clone().oneshot(req).await.unwrap().into_body();
    let mut buf = String::new();
    assert_eq!(next_event(&mut body, &mut buf).await, ServerEvent::Hello);
    assert_eq!(s.del(&s.member.0, m.id).await, StatusCode::NO_CONTENT);
    loop {
        match next_event(&mut body, &mut buf).await {
            ServerEvent::MessageDeleted { channel_id, message_id } => {
                assert_eq!((channel_id, message_id), (s.text, m.id));
                break;
            }
            _ => continue,
        }
    }
    let (st, _, _, _) = s.t.call_raw(Method::GET, &format!("/attachments/{att}"), Some(&s.member.0), vec![]).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn any_file_upload_and_download() {
    let s = setup_server().await;
    let data = b"MZ\x90\0not really an exe".to_vec();
    let (st, v, _, _) = s.t.call_raw(Method::POST, "/uploads?file=1&name=%D0%BE%D1%82%D1%87%D1%91%D1%82.exe", Some(&s.member.0), data.clone()).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert_eq!(v["name"], "отчёт.exe");
    assert_eq!(v["mime"], "application/octet-stream");
    assert_eq!(v["width"], 0);
    let att = v["id"].as_i64().unwrap();
    let m = s.say(&s.member.0, "файл", vec![att]).await;
    assert_eq!(m.attachments.len(), 1);
    assert!(!m.attachments[0].is_image());
    assert_eq!(m.attachments[0].name, "отчёт.exe");
    let (st, _, h, bytes) = s.t.call_raw(Method::GET, &format!("/attachments/{att}"), Some(&s.other.0), vec![]).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(bytes, data);
    assert_eq!(h[header::CONTENT_TYPE], "application/octet-stream");
    assert!(h[header::CONTENT_DISPOSITION].to_str().unwrap().starts_with("attachment;"));
    // пустой файл — нельзя; картинка без file=1 — по-прежнему только картинки
    let (st, _, _, _) = s.t.call_raw(Method::POST, "/uploads?file=1&name=x", Some(&s.member.0), vec![]).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let (st, _, _, _) = s.t.call_raw(Method::POST, "/uploads", Some(&s.member.0), data).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    // больше 8 МБ (но меньше 50) — как файл можно
    let big = vec![7u8; 9 * 1024 * 1024];
    let (st, v, _, _) = s.t.call_raw(Method::POST, "/uploads?file=1&name=big.bin", Some(&s.member.0), big).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let too_big = vec![7u8; proto::limits::FILE_MAX_BYTES + 1];
    let (st, _, _, _) = s.t.call_raw(Method::POST, "/uploads?file=1&name=huge.bin", Some(&s.member.0), too_big).await;
    assert!(st == StatusCode::UNPROCESSABLE_ENTITY || st == StatusCode::PAYLOAD_TOO_LARGE, "{st}");
}
