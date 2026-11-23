//! Профиль сервера (описание, узор, цвет, баннер), активности, карточка профиля, «позвать».

mod common;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use common::*;
use proto::{FriendsList, Member, Server, ServerEvent, UserProfile};
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
    /// Следующее событие, пропуская «в сети / не в сети».
    async fn next_skip_presence(&mut self) -> ServerEvent {
        loop {
            match self.next().await {
                ServerEvent::Presence { .. } => continue,
                e => return e,
            }
        }
    }
}

#[tokio::test]
async fn server_profile_fields() {
    let t = setup().await;
    let admin = t.admin().await;
    let (s, v) = t.call(Method::POST, "/servers", Some(&admin), Some(json!({"name": "Лаборатория"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let id = serde_json::from_value::<Server>(v).unwrap().id;
    let url = format!("/servers/{id}");
    let (s, v) = t.call(Method::PATCH, &url, Some(&admin), Some(json!({"description": "  гаджеты  ", "banner": "night", "color": "#8b5cf6"}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let srv: Server = serde_json::from_value(v).unwrap();
    assert_eq!(srv.description.as_deref(), Some("гаджеты"));
    assert_eq!(srv.banner.as_deref(), Some("night"));
    assert_eq!(srv.color.as_deref(), Some("#8B5CF6"));
    for bad in [json!({"banner": "lava"}), json!({"color": "#123456"}), json!({"description": "я".repeat(121)})] {
        let (s, v) = t.call(Method::PATCH, &url, Some(&admin), Some(bad)).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    }
    // пустые строки — сбросить
    let (_, v) = t.call(Method::PATCH, &url, Some(&admin), Some(json!({"description": "", "banner": "", "color": ""}))).await;
    let srv: Server = serde_json::from_value(v).unwrap();
    assert_eq!((srv.description, srv.banner, srv.color), (None, None, None));
    // баннер — только 960×480
    let (s, _, _, _) = t.call_raw(Method::PUT, &format!("{url}/banner"), Some(&admin), png(64, 64)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, v, _, _) = t.call_raw(Method::PUT, &format!("{url}/banner"), Some(&admin), png(960, 480)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let key = serde_json::from_value::<Server>(v).unwrap().banner_image.unwrap();
    let (s, _) = t.call(Method::GET, &format!("/images/{key}"), Some(&admin), None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = t.call(Method::DELETE, &format!("{url}/banner"), Some(&admin), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(serde_json::from_value::<Server>(v).unwrap().banner_image.is_none());
    // участник без права — нельзя
    let (u, u_id) = t.user("mayuri").await;
    t.add_member(1, u_id, 0).await;
    let (s, _) = t.call(Method::PATCH, "/servers/1", Some(&u), Some(json!({"description": "x"}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn activity_reaches_friends_and_members() {
    let t = setup().await;
    t.admin().await; // сервер №1
    let (a, a_id) = t.user("okabe").await;
    let (b, b_id) = t.user("kurisu").await;
    t.add_member(1, a_id, 0).await;
    t.add_member(1, b_id, 0).await;
    let mut sb = Stream::open(&t, &b).await;
    let mut sa = Stream::open(&t, &a).await;
    // b видит, что a в сети (общий сервер №1)
    assert_eq!(sb.next().await, ServerEvent::Presence { user_id: a_id, online: true });
    let (s, _) = t.call(Method::PUT, "/me/activity", Some(&a), Some(json!({"game": "Dota 2", "since": 100}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    match sb.next_skip_presence().await {
        ServerEvent::Activity { user_id, activity: Some(act) } => {
            assert_eq!(user_id, a_id);
            assert_eq!((act.game.as_deref(), act.since), (Some("Dota 2"), Some(100)));
        }
        e => panic!("{e:?}"),
    }
    // свой статус
    let (s, _) = t.call(Method::PATCH, "/me", Some(&a), Some(json!({"status": "чиню сервер", "about": "безумный учёный"}))).await;
    assert_eq!(s, StatusCode::OK);
    match sb.next_skip_presence().await {
        ServerEvent::Activity { activity: Some(act), .. } => assert_eq!(act.status.as_deref(), Some("чиню сервер")),
        e => panic!("{e:?}"),
    }
    // список участников
    let (_, v) = t.call(Method::GET, "/servers/1/members", Some(&b), None).await;
    let members: Vec<Member> = serde_json::from_value(v).unwrap();
    let ma = members.iter().find(|m| m.user.id == a_id).unwrap();
    assert!(ma.online);
    assert_eq!(ma.activity.as_ref().and_then(|a| a.game.as_deref()), Some("Dota 2"));
    // карточка профиля
    let (s, v) = t.call(Method::GET, &format!("/users/{a_id}"), Some(&b), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let p: UserProfile = serde_json::from_value(v).unwrap();
    assert_eq!(p.about.as_deref(), Some("безумный учёный"));
    assert!(p.online && !p.is_friend);
    // друзья — активность в списке
    t.call(Method::POST, "/friends/requests", Some(&b), Some(json!({"name": "okabe"}))).await;
    let (_, v) = t.call(Method::GET, "/friends", Some(&b), None).await;
    let l: FriendsList = serde_json::from_value(v).unwrap();
    assert!(l.friends.is_empty() && l.outgoing.len() == 1);
    t.call(Method::POST, &format!("/friends/{b_id}/accept"), Some(&a), None).await;
    let (_, v) = t.call(Method::GET, "/friends", Some(&b), None).await;
    let l: FriendsList = serde_json::from_value(v).unwrap();
    assert_eq!(l.friends[0].activity.as_ref().and_then(|x| x.game.as_deref()), Some("Dota 2"));
    // вышел — активность пропала
    drop(sa.body);
    drop(sa.buf);
    sa = Stream::open(&t, &b).await; // поток держим у b, a офлайн
    let _ = &mut sa;
    let (_, v) = t.call(Method::GET, &format!("/users/{a_id}"), Some(&b), None).await;
    let p: UserProfile = serde_json::from_value(v).unwrap();
    assert!(!p.online && p.activity.is_none());
}

#[tokio::test]
async fn profile_hidden_from_strangers() {
    let t = setup().await;
    let admin = t.admin().await;
    let (_, v) = t.call(Method::POST, "/servers", Some(&admin), Some(json!({"name": "Тайный"}))).await;
    let sid = serde_json::from_value::<Server>(v).unwrap().id;
    // чужак — на отдельном сервере, общего нет
    let (s, v) = t.call(Method::POST, &format!("/servers/{sid}/invites"), Some(&admin), Some(json!({}))).await;
    assert_eq!(s, StatusCode::CREATED);
    let code = v["code"].as_str().unwrap().to_string();
    let (s, v) = t.register(&code, "stranger").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let stranger = v["token"].as_str().unwrap().to_string();
    let (_, me_id) = t.user("okabe").await;
    let (s, _) = t.call(Method::GET, &format!("/users/{me_id}"), Some(&stranger), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn summon_to_voice() {
    let t = setup().await;
    let admin = t.admin().await;
    let (_, v) = t.call(Method::POST, "/servers/1/channels", Some(&admin), Some(json!({"name": "Голос", "kind": "voice"}))).await;
    let voice = v["id"].as_i64().unwrap();
    let (_, v) = t.call(Method::POST, "/servers/1/channels", Some(&admin), Some(json!({"name": "текст", "kind": "text"}))).await;
    let text = v["id"].as_i64().unwrap();
    let (b, b_id) = t.user("kurisu").await;
    t.add_member(1, b_id, 0).await;
    let mut sb = Stream::open(&t, &b).await;
    let (s, v) = t.call(Method::POST, &format!("/channels/{voice}/summon"), Some(&admin), Some(json!({"user_id": b_id}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    match sb.next_skip_presence().await {
        ServerEvent::Summon { from, server_id, channel_id } => {
            assert_eq!((from.name.as_str(), server_id, channel_id), ("admin", 1, voice));
        }
        e => panic!("{e:?}"),
    }
    let (s, _) = t.call(Method::POST, &format!("/channels/{text}/summon"), Some(&admin), Some(json!({"user_id": b_id}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = t.call(Method::POST, &format!("/channels/{voice}/summon"), Some(&admin), Some(json!({"user_id": 999}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

/// Минимальный валидный PNG нужного размера (заголовок IHDR — imagesize читает только его).
fn png(w: u32, h: u32) -> Vec<u8> {
    if (w as u64) * (h as u64) <= 1_000_000 && w > 0 && h > 0 {
        let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, 90, 255]));
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        return out;
    }
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    v
}
