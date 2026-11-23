//! Адресная рассылка `GET /events`: события только участникам своих серверов (фаза 9.1).

mod common;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use common::*;
use proto::{ChannelLayout, ServerEvent};
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
}

async fn text_channel(t: &T, tok: &str, server: i64) -> i64 {
    let (_, v) = t.call(Method::GET, &format!("/servers/{server}/channels"), Some(tok), None).await;
    let l: ChannelLayout = serde_json::from_value(v).unwrap();
    l.channels.iter().find(|c| c.kind == proto::ChannelKind::Text).unwrap().id
}

async fn say(t: &T, tok: &str, ch: i64, text: &str) {
    let (s, v) = t.call(Method::POST, &format!("/channels/{ch}/messages"), Some(tok), Some(json!({"content": text}))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
}

fn text_of(e: ServerEvent) -> String {
    match e {
        ServerEvent::MessageCreated { message } => message.content,
        e => panic!("ожидали сообщение, пришло {e:?}"),
    }
}

#[tokio::test]
async fn events_only_for_member_servers() {
    let t = setup().await;
    let a = t.user("okabe").await;
    let b = t.user("suzuha").await;
    let sa = t.create_server(&a.0, "A").await;
    let sb = t.create_server(&b.0, "B").await;
    let (ca, cb) = (text_channel(&t, &a.0, sa.id).await, text_channel(&t, &b.0, sb.id).await);
    let mut s = Stream::open(&t, &a.0).await;
    say(&t, &b.0, cb, "секрет B").await;
    say(&t, &a.0, ca, "своё").await;
    assert_eq!(text_of(s.next().await), "своё", "сообщение чужого сервера не пришло");
}

#[tokio::test]
async fn joined_server_starts_receiving() {
    let t = setup().await;
    let a = t.user("okabe").await;
    let g = t.user("mayuri").await;
    let sa = t.create_server(&a.0, "A").await;
    let ca = text_channel(&t, &a.0, sa.id).await;
    let mut s = Stream::open(&t, &g.0).await;
    let (_, v) = t.call(Method::POST, &format!("/servers/{}/invites", sa.id), Some(&a.0), Some(json!({}))).await;
    let code = v["code"].as_str().unwrap().to_string();
    t.call(Method::POST, &format!("/invites/{code}/join"), Some(&g.0), None).await;
    match s.next().await {
        ServerEvent::ServerUpserted { server } => assert_eq!(server.id, sa.id),
        e => panic!("{e:?}"),
    }
    match s.next().await {
        ServerEvent::MemberUpserted { server_id, member } => assert_eq!((server_id, member.user.id), (sa.id, g.1)),
        e => panic!("{e:?}"),
    }
    say(&t, &a.0, ca, "привет").await;
    assert_eq!(text_of(s.next().await), "привет");
}

#[tokio::test]
async fn kicked_stops_receiving() {
    let t = setup().await;
    let a = t.user("okabe").await;
    let g = t.user("mayuri").await;
    let sa = t.create_server(&a.0, "A").await;
    let ca = text_channel(&t, &a.0, sa.id).await;
    t.add_member(sa.id, g.1, 0).await;
    let mut s = Stream::open(&t, &g.0).await;
    t.call(Method::DELETE, &format!("/servers/{}/members/{}", sa.id, g.1), Some(&a.0), None).await;
    assert_eq!(s.next().await, ServerEvent::ServerRemoved { server_id: sa.id });
    say(&t, &a.0, ca, "после кика").await;
    // событие, адресованное ему самому, — должно прийти раньше (и вместо) сообщения
    let own = t.create_server(&g.0, "Своё").await;
    match s.next().await {
        ServerEvent::ServerUpserted { server } => assert_eq!(server.id, own.id),
        e => panic!("кикнутому пришло {e:?}"),
    }
}

#[tokio::test]
async fn layout_and_role_events() {
    let t = setup().await;
    let a = t.user("okabe").await;
    let g = t.user("mayuri").await;
    let sa = t.create_server(&a.0, "A").await;
    t.add_member(sa.id, g.1, 0).await;
    let mut s = Stream::open(&t, &g.0).await;
    t.call(Method::POST, &format!("/servers/{}/channels", sa.id), Some(&a.0), Some(json!({"name": "новый", "kind": "text"}))).await;
    match s.next().await {
        ServerEvent::LayoutChanged { server_id, layout } => {
            assert_eq!(server_id, sa.id);
            assert!(layout.channels.iter().any(|c| c.name == "новый"));
        }
        e => panic!("{e:?}"),
    }
    let moder = sa.roles.iter().find(|r| r.position == 1).unwrap().id;
    t.call(Method::PUT, &format!("/servers/{}/members/{}/role", sa.id, g.1), Some(&a.0), Some(json!({"role_id": moder}))).await;
    match s.next().await {
        ServerEvent::MemberUpserted { member, .. } => assert_eq!(member.role_id, moder),
        e => panic!("{e:?}"),
    }
    match s.next().await {
        ServerEvent::ServerUpserted { server } => assert_eq!(server.my_role_id, moder),
        e => panic!("{e:?}"),
    }
}

#[tokio::test]
async fn old_client_events_shape() {
    let t = setup().await;
    let a = t.user("okabe").await;
    let sa = t.create_server(&a.0, "A").await;
    let ca = text_channel(&t, &a.0, sa.id).await;
    let req = Request::builder().uri("/events").header(header::AUTHORIZATION, format!("Bearer {}", a.0)).body(Body::empty()).unwrap();
    let mut body = t.app.clone().oneshot(req).await.unwrap().into_body();
    let mut buf = String::new();
    next_event(&mut body, &mut buf).await;
    say(&t, &a.0, ca, "x").await;
    // сырой JSON: старый клиент 0.2.x читает type/message/{id,channel_id,author{id,name},content,created_at}
    use http_body_util::BodyExt;
    while !buf.contains("message_created") {
        let f = body.frame().await.unwrap().unwrap();
        if let Ok(d) = f.into_data() {
            buf.push_str(std::str::from_utf8(&d).unwrap());
        }
    }
    let line = buf.lines().find(|l| l.contains("message_created")).unwrap();
    let v: serde_json::Value = serde_json::from_str(line.trim_start_matches("data:").trim()).unwrap();
    assert_eq!(v["type"], "message_created");
    for k in ["id", "channel_id", "content", "created_at"] {
        assert!(!v["message"][k].is_null(), "{k}");
    }
    assert_eq!(v["message"]["author"]["name"], "okabe");
}

#[tokio::test]
async fn streams_per_user_are_capped() {
    let t = setup().await;
    let a = t.user("okabe").await;
    let mut open = Vec::new();
    for _ in 0..8 {
        open.push(Stream::open(&t, &a.0).await);
    }
    let req = Request::builder().uri("/events").header(header::AUTHORIZATION, format!("Bearer {}", a.0)).body(Body::empty()).unwrap();
    assert_eq!(t.app.clone().oneshot(req).await.unwrap().status(), StatusCode::TOO_MANY_REQUESTS);
    // закрыли один — можно снова
    drop(open.pop());
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let _again = Stream::open(&t, &a.0).await;
}
