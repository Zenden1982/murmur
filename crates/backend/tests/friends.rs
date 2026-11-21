//! Друзья: заявки, принятие, удаление, онлайн-статус и события.

mod common;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use common::*;
use proto::{FriendsList, ServerEvent};
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

async fn friends(t: &T, tok: &str) -> FriendsList {
    let (s, v) = t.call(Method::GET, "/friends", Some(tok), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    serde_json::from_value(v).unwrap()
}

async fn add(t: &T, tok: &str, name: &str) -> (StatusCode, serde_json::Value) {
    t.call(Method::POST, "/friends/requests", Some(tok), Some(json!({"name": name}))).await
}

#[tokio::test]
async fn request_accept_remove() {
    let t = setup().await;
    let (a, a_id) = t.user("okabe").await;
    let (b, b_id) = t.user("kurisu").await;

    let (s, v) = add(&t, &a, "kurisu").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let la = friends(&t, &a).await;
    assert_eq!((la.outgoing.len(), la.incoming.len(), la.friends.len()), (1, 0, 0));
    assert_eq!(la.outgoing[0].user.id, b_id);
    let lb = friends(&t, &b).await;
    assert_eq!(lb.incoming.len(), 1);
    assert_eq!(lb.incoming[0].user.name, "okabe");

    // повторная заявка — без дубля
    let (s, _) = add(&t, &a, "kurisu").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(friends(&t, &a).await.outgoing.len(), 1);

    // принять может только адресат
    let (s, _) = t.call(Method::POST, &format!("/friends/{b_id}/accept"), Some(&a), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, v) = t.call(Method::POST, &format!("/friends/{a_id}/accept"), Some(&b), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let lb: FriendsList = serde_json::from_value(v).unwrap();
    assert_eq!(lb.friends.len(), 1);
    assert_eq!(lb.friends[0].user.id, a_id);
    assert!(!lb.friends[0].online, "поток событий не открыт — не в сети");
    assert_eq!(friends(&t, &a).await.friends.len(), 1);

    let (s, v) = add(&t, &a, "kurisu").await;
    assert_eq!((s, err(&v).as_str()), (StatusCode::BAD_REQUEST, "bad_request"), "уже друзья");

    // удалить может любой из двоих
    let (s, _) = t.call(Method::DELETE, &format!("/friends/{a_id}"), Some(&b), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(friends(&t, &a).await, FriendsList::default());
    let (s, _) = t.call(Method::DELETE, &format!("/friends/{a_id}"), Some(&b), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn counter_request_becomes_friendship() {
    let t = setup().await;
    let (a, _) = t.user("okabe").await;
    let (b, _) = t.user("kurisu").await;
    add(&t, &a, "kurisu").await;
    let (s, _) = add(&t, &b, "okabe").await;
    assert_eq!(s, StatusCode::CREATED);
    let la = friends(&t, &a).await;
    assert_eq!((la.friends.len(), la.outgoing.len(), la.incoming.len()), (1, 0, 0));
}

#[tokio::test]
async fn decline_and_cancel() {
    let t = setup().await;
    let (a, a_id) = t.user("okabe").await;
    let (b, b_id) = t.user("kurisu").await;
    add(&t, &a, "kurisu").await;
    // отклонить входящую
    let (s, _) = t.call(Method::DELETE, &format!("/friends/{a_id}"), Some(&b), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(friends(&t, &a).await.outgoing.is_empty());
    // отменить свою
    add(&t, &a, "kurisu").await;
    let (s, _) = t.call(Method::DELETE, &format!("/friends/{b_id}"), Some(&a), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(friends(&t, &b).await.incoming.is_empty());
}

#[tokio::test]
async fn bad_targets() {
    let t = setup().await;
    let (a, _) = t.user("okabe").await;
    t.user("Kurisu").await;
    let (s, v) = add(&t, &a, "nobody").await;
    assert_eq!((s, err(&v).as_str()), (StatusCode::NOT_FOUND, "not_found"));
    let (s, v) = add(&t, &a, "okabe").await;
    assert_eq!((s, err(&v).as_str()), (StatusCode::UNPROCESSABLE_ENTITY, "validation"));
    // регистр логина не важен
    let (s, v) = add(&t, &a, "  kurisu ").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, _) = t.call(Method::GET, "/friends", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn events_and_presence() {
    let t = setup().await;
    let (a, a_id) = t.user("okabe").await;
    let (b, b_id) = t.user("kurisu").await;
    let mut sb = Stream::open(&t, &b).await;
    add(&t, &a, "kurisu").await;
    assert_eq!(sb.next().await, ServerEvent::FriendsChanged, "заявка пришла адресату");
    let mut sa = Stream::open(&t, &a).await;
    // пока не друзья — о входе в сеть не сообщаем; принять
    t.call(Method::POST, &format!("/friends/{a_id}/accept"), Some(&b), None).await;
    assert_eq!(sb.next().await, ServerEvent::FriendsChanged);
    assert_eq!(sa.next().await, ServerEvent::FriendsChanged);
    // оба в сети
    let lb = friends(&t, &b).await;
    assert!(lb.friends[0].online);
    // a закрыл клиент — b узнаёт
    drop(sa);
    // «в сети» рассылается в фоне и может прийти уже после принятия заявки — пропускаем
    let mut ev = sb.next().await;
    if ev == (ServerEvent::Presence { user_id: a_id, online: true }) {
        ev = sb.next().await;
    }
    assert_eq!(ev, ServerEvent::Presence { user_id: a_id, online: false });
    assert!(!friends(&t, &b).await.friends[0].online);
    // a вернулся
    let _sa = Stream::open(&t, &a).await;
    assert_eq!(sb.next().await, ServerEvent::Presence { user_id: a_id, online: true });
    // смена имени у друга (общих серверов нет) — тоже приходит
    t.call(Method::PATCH, "/me", Some(&a), Some(json!({"display_name": "Окабэ"}))).await;
    match sb.next().await {
        ServerEvent::UserUpdated { user } => assert_eq!((user.id, user.display_name.as_deref()), (a_id, Some("Окабэ"))),
        e => panic!("{e:?}"),
    }
    let _ = b_id;
}

#[tokio::test]
async fn two_streams_one_presence() {
    let t = setup().await;
    let (a, a_id) = t.user("okabe").await;
    let (b, _) = t.user("kurisu").await;
    add(&t, &a, "kurisu").await;
    add(&t, &b, "okabe").await;
    let mut sb = Stream::open(&t, &b).await;
    let s1 = Stream::open(&t, &a).await;
    assert_eq!(sb.next().await, ServerEvent::Presence { user_id: a_id, online: true });
    let s2 = Stream::open(&t, &a).await;
    drop(s1);
    // второй поток ещё открыт — по-прежнему в сети
    assert!(friends(&t, &b).await.friends[0].online);
    drop(s2);
    assert_eq!(sb.next().await, ServerEvent::Presence { user_id: a_id, online: false });
}
