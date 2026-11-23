//! Интеграционные тесты HTTP API (in-memory SQLite, без сети).

mod common;

use std::time::Duration;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use common::*;
use http_body_util::BodyExt;
use livekit_api::access_token::TokenVerifier;
use proto::{
    Attachment, AuthResponse, Channel, JoinResponse, Message, ServerEvent, User,
};
use serde_json::{Value, json};
use tower::ServiceExt;

#[tokio::test]
async fn health() {
    let t = setup().await;
    let (s, v) = t.call(Method::GET, "/health", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["ok"], true);
}

#[tokio::test]
async fn register_requires_valid_invite() {
    let t = setup().await;
    let admin = t.admin().await;

    let (s, v) = t.register("NOPE-NOPE-NOPE", "vasya").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(err(&v), "invalid_invite");

    let inv = t.invite(&admin, json!({})).await;
    assert_eq!(inv.uses_left, 1);
    // регистр и дефисы не важны
    let (s, v) = t
        .register(&inv.code.to_lowercase().replace('-', ""), "vasya")
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let auth: AuthResponse = serde_json::from_value(v).unwrap();
    assert_eq!(auth.user.name, "vasya");
    assert!(!auth.user.is_admin);

    // одноразовый инвайт исчерпан
    let (s, v) = t.register(&inv.code, "petya").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(err(&v), "invalid_invite");

    // сессия из регистрации рабочая
    let (s, v) = t.call(Method::GET, "/me", Some(&auth.token), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(serde_json::from_value::<User>(v).unwrap().name, "vasya");
}

#[tokio::test]
async fn multi_use_revoked_and_expired_invites() {
    let t = setup().await;
    let admin = t.admin().await;

    let inv = t.invite(&admin, json!({"uses": 2})).await;
    assert_eq!(t.register(&inv.code, "user1").await.0, StatusCode::CREATED);
    assert_eq!(t.register(&inv.code, "user2").await.0, StatusCode::CREATED);
    assert_eq!(
        t.register(&inv.code, "user3").await.0,
        StatusCode::BAD_REQUEST
    );

    let inv = t.invite(&admin, json!({"uses": 5})).await;
    let (s, _) = t
        .call(
            Method::DELETE,
            &format!("/invites/{}", inv.code),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(
        t.register(&inv.code, "user4").await.0,
        StatusCode::BAD_REQUEST
    );

    let inv = t.invite(&admin, json!({"uses": 5})).await;
    sqlx::query("UPDATE invites SET expires_at = 1 WHERE code = ?")
        .bind(&inv.code)
        .execute(&t.pool)
        .await
        .unwrap();
    assert_eq!(
        t.register(&inv.code, "user5").await.0,
        StatusCode::BAD_REQUEST
    );

    // бессрочный
    let inv = t.invite(&admin, json!({"expires_in_hours": 0})).await;
    assert!(inv.expires_at.is_none());
    assert_eq!(t.register(&inv.code, "user6").await.0, StatusCode::CREATED);

    let (s, v) = t.call(Method::GET, "/invites", Some(&admin), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn name_taken_does_not_burn_invite() {
    let t = setup().await;
    let admin = t.admin().await;
    let inv = t.invite(&admin, json!({})).await;
    let (s, v) = t.register(&inv.code, "ADMIN").await; // без учёта регистра
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(err(&v), "name_taken");
    // транзакция откатилась — инвайт ещё жив
    assert_eq!(t.register(&inv.code, "vasya").await.0, StatusCode::CREATED);
}

#[tokio::test]
async fn validation_errors() {
    let t = setup().await;
    let admin = t.admin().await;
    let inv = t.invite(&admin, json!({})).await;
    let (s, v) = t
        .call(
            Method::POST,
            "/auth/register",
            None,
            Some(json!({"invite_code": inv.code, "name": "ok_name", "password": "short"})),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(err(&v), "validation");
    let (s, v) = t
        .call(Method::POST, "/auth/login", None, Some(json!({"name": 1})))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(err(&v), "bad_request");
    let (s, _) = t
        .call(
            Method::POST,
            "/invites",
            Some(&admin),
            Some(json!({"uses": 0})),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn login_and_logout() {
    let t = setup().await;
    t.admin().await;
    let (s, v) = t
        .call(
            Method::POST,
            "/auth/login",
            None,
            Some(json!({"name": "admin", "password": "wrong-password"})),
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(err(&v), "bad_credentials");
    let (s, v) = t
        .call(
            Method::POST,
            "/auth/login",
            None,
            Some(json!({"name": "ghost", "password": "whatever1"})),
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(err(&v), "bad_credentials");

    let tok = t.login("Admin", "admin-password").await; // имя без учёта регистра
    assert_eq!(
        t.call(Method::GET, "/me", Some(&tok), None).await.0,
        StatusCode::OK
    );
    assert_eq!(
        t.call(Method::POST, "/auth/logout", Some(&tok), None)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        t.call(Method::GET, "/me", Some(&tok), None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        t.call(Method::GET, "/me", Some("garbage"), None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        t.call(Method::GET, "/channels", None, None).await.0,
        StatusCode::UNAUTHORIZED
    );

    // сессии хранятся только хэшем
    let stored: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM sessions")
        .fetch_all(&t.pool)
        .await
        .unwrap();
    assert!(!stored.contains(&tok));
}

#[tokio::test]
async fn expired_session_rejected() {
    let t = setup().await;
    let tok = t.admin().await;
    sqlx::query("UPDATE sessions SET expires_at = 1")
        .execute(&t.pool)
        .await
        .unwrap();
    assert_eq!(
        t.call(Method::GET, "/me", Some(&tok), None).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn login_rate_limited_per_ip() {
    let t = setup_with_limit(3).await;
    t.admin().await; // удачный вход попытку не расходует (возвращается)
    for _ in 0..3 {
        let (s, _) = t
            .call(
                Method::POST,
                "/auth/login",
                None,
                Some(json!({"name": "admin", "password": "bad-password"})),
            )
            .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    let (s, v) = t
        .call(
            Method::POST,
            "/auth/login",
            None,
            Some(json!({"name": "admin", "password": "admin-password"})),
        )
        .await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(err(&v), "rate_limited");
    // другой IP (из X-Forwarded-For от доверенного прокси) — не заблокирован;
    // подделанное начало заголовка не помогает обойти лимит
    let (s, _) = t
        .call_from(
            Method::POST,
            "/auth/login",
            None,
            Some(json!({"name": "admin", "password": "admin-password"})),
            "198.51.100.7",
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = t
        .call_from(
            Method::POST,
            "/auth/login",
            None,
            Some(json!({"name": "admin", "password": "admin-password"})),
            "198.51.100.9, 203.0.113.1",
        )
        .await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn channels_admin_only() {
    let t = setup().await;
    let admin = t.admin().await;
    let inv = t.invite(&admin, json!({})).await;
    let user: AuthResponse =
        serde_json::from_value(t.register(&inv.code, "vasya").await.1).unwrap();

    let (s, _) = t
        .call(
            Method::POST,
            "/channels",
            Some(&user.token),
            Some(json!({"name": "x", "kind": "voice"})),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // фаза 9.1: приглашать может любой участник (видит только свои приглашения)
    let (s, _) = t
        .call(Method::POST, "/invites", Some(&user.token), Some(json!({})))
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, v) = t.call(Method::GET, "/invites", Some(&user.token), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 1);

    let (s, v) = t
        .call(
            Method::POST,
            "/channels",
            Some(&admin),
            Some(json!({"name": " Общий ", "kind": "voice"})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let ch: Channel = serde_json::from_value(v).unwrap();
    assert_eq!(ch.name, "Общий");
    t.call(
        Method::POST,
        "/channels",
        Some(&admin),
        Some(json!({"name": "чат", "kind": "text"})),
    )
    .await;

    let (s, v) = t
        .call(Method::GET, "/channels", Some(&user.token), None)
        .await;
    assert_eq!(s, StatusCode::OK);
    let list: Vec<Channel> = serde_json::from_value(v).unwrap();
    assert_eq!(list.len(), 2);

    assert_eq!(
        t.call(
            Method::DELETE,
            &format!("/channels/{}", ch.id),
            Some(&user.token),
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        t.call(
            Method::DELETE,
            &format!("/channels/{}", ch.id),
            Some(&admin),
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        t.call(
            Method::DELETE,
            &format!("/channels/{}", ch.id),
            Some(&admin),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn join_issues_valid_livekit_token() {
    let t = setup().await;
    let admin = t.admin().await;
    let inv = t.invite(&admin, json!({})).await;
    let user: AuthResponse = serde_json::from_value(t.register(&inv.code, "Вася").await.1).unwrap();
    let (_, v) = t
        .call(
            Method::POST,
            "/channels",
            Some(&admin),
            Some(json!({"name": "Голос", "kind": "voice"})),
        )
        .await;
    let voice: Channel = serde_json::from_value(v).unwrap();
    let (_, v) = t
        .call(
            Method::POST,
            "/channels",
            Some(&admin),
            Some(json!({"name": "чат", "kind": "text"})),
        )
        .await;
    let text: Channel = serde_json::from_value(v).unwrap();

    assert_eq!(
        t.call(
            Method::POST,
            &format!("/channels/{}/join", voice.id),
            None,
            None
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        t.call(Method::POST, "/channels/999/join", Some(&user.token), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        t.call(
            Method::POST,
            &format!("/channels/{}/join", text.id),
            Some(&user.token),
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );

    let (s, v) = t
        .call(
            Method::POST,
            &format!("/channels/{}/join", voice.id),
            Some(&user.token),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let j: JoinResponse = serde_json::from_value(v).unwrap();
    assert_eq!(j.livekit_url, LK_URL);

    let claims = TokenVerifier::with_api_key(LK_KEY, LK_SECRET)
        .verify(&j.livekit_token)
        .expect("valid jwt");
    assert_eq!(claims.sub, user.user.id.to_string());
    assert_eq!(claims.name, "Вася");
    assert_eq!(claims.iss, LK_KEY);
    assert!(claims.video.room_join);
    assert_eq!(claims.video.room, j.room);
    assert_eq!(claims.video.room, format!("srv-1-ch-{}", voice.id));
    assert!(claims.video.can_publish() && claims.video.can_subscribe());
    assert_eq!(
        claims.video.can_publish_sources,
        ["microphone", "screen_share", "screen_share_audio"]
    );
    assert!(!claims.video.can_update_own_metadata());
    assert!(!claims.video.room_admin && !claims.video.room_create);
    let ttl = claims.exp as i64 - claims.nbf as i64;
    assert!((15 * 60 - 5..=15 * 60 + 5).contains(&ttl), "ttl {ttl}");

    // чужим секретом не проверяется
    assert!(
        TokenVerifier::with_api_key(LK_KEY, "other-secret-other-secret-other-secret")
            .verify(&j.livekit_token)
            .is_err()
    );
}

#[tokio::test]
async fn backup_makes_consistent_copy() {
    let dir = std::env::temp_dir().join(format!("amadeus-backup-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let src = dir.join("src.db");
    let url = format!("sqlite://{}", src.to_str().unwrap().replace('\\', "/"));
    let pool = backend::connect(&url).await.unwrap();
    backend::create_user(&pool, "alice", "password-123", true).await.unwrap();
    let out = dir.join("backup").join("app.db");
    backend::backup(&pool, &out).await.unwrap();
    // повторный бэкап заменяет файл
    backend::create_user(&pool, "bob", "password-123", false).await.unwrap();
    backend::backup(&pool, &out).await.unwrap();
    let copy_url = format!("sqlite://{}", out.to_str().unwrap().replace('\\', "/"));
    let copy = backend::connect(&copy_url).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users").fetch_one(&copy).await.unwrap();
    assert_eq!(n, 2);
    copy.close().await;
    pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ чат

/// PNG: настоящий (декодируется), если небольшой; для огромных размеров — только сигнатура + IHDR.
fn png(w: u32, h: u32) -> Vec<u8> {
    if (w as u64) * (h as u64) <= 1_000_000 {
        let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, 90, 255]));
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        return out;
    }
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend_from_slice(&13u32.to_be_bytes());
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    v
}

struct Chat {
    t: T,
    admin: String,
    user: String,
    text: i64,
    voice: i64,
}

async fn chat_setup() -> Chat {
    let t = setup().await;
    let admin = t.admin().await;
    let inv = t.invite(&admin, json!({})).await;
    let user: AuthResponse =
        serde_json::from_value(t.register(&inv.code, "vasya").await.1).unwrap();
    let mk = |name: &'static str, kind: &'static str| {
        let t = &t;
        let admin = admin.clone();
        async move {
            let (s, v) = t
                .call(
                    Method::POST,
                    "/channels",
                    Some(&admin),
                    Some(json!({"name": name, "kind": kind})),
                )
                .await;
            assert_eq!(s, StatusCode::CREATED);
            serde_json::from_value::<Channel>(v).unwrap().id
        }
    };
    let text = mk("чат", "text").await;
    let voice = mk("Общий", "voice").await;
    Chat {
        t,
        admin,
        user: user.token,
        text,
        voice,
    }
}

impl Chat {
    async fn send(&self, tok: &str, ch: i64, body: Value) -> (StatusCode, Value) {
        self.t
            .call(
                Method::POST,
                &format!("/channels/{ch}/messages"),
                Some(tok),
                Some(body),
            )
            .await
    }

    async fn history(&self, tok: &str, ch: i64, q: &str) -> Vec<Message> {
        let (s, v) = self
            .t
            .call(
                Method::GET,
                &format!("/channels/{ch}/messages{q}"),
                Some(tok),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        serde_json::from_value(v).unwrap()
    }
}

#[tokio::test]
async fn chat_send_and_history() {
    let c = chat_setup().await;
    for i in 0..7 {
        let tok = if i % 2 == 0 { &c.user } else { &c.admin };
        let (s, v) = c
            .send(tok, c.text, json!({"content": format!("  msg {i}\n ")}))
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        let m: Message = serde_json::from_value(v).unwrap();
        assert_eq!(m.content, format!("msg {i}")); // пробелы по краям обрезаны
        assert_eq!(m.channel_id, c.text);
    }
    let all = c.history(&c.user, c.text, "").await;
    assert_eq!(all.len(), 7);
    assert!(all.windows(2).all(|w| w[0].id < w[1].id), "по возрастанию");
    assert_eq!(all[0].author.name, "vasya");
    assert_eq!(all[1].author.name, "admin");

    // страница «старше» — последние 3 перед all[5]
    let page = c
        .history(&c.user, c.text, &format!("?before={}&limit=3", all[5].id))
        .await;
    assert_eq!(
        page.iter().map(|m| m.id).collect::<Vec<_>>(),
        all[2..5].iter().map(|m| m.id).collect::<Vec<_>>()
    );
    // последние 2
    let last = c.history(&c.user, c.text, "?limit=2").await;
    assert_eq!(last[1].id, all[6].id);
    assert_eq!(last.len(), 2);
    // «новее» — дозапрос после переподключения
    let newer = c
        .history(&c.user, c.text, &format!("?after={}", all[4].id))
        .await;
    assert_eq!(newer.len(), 2);
    assert_eq!(newer[0].id, all[5].id);
    let (s, _) = c
        .t
        .call(
            Method::GET,
            &format!("/channels/{}/messages?before=1&after=1", c.text),
            Some(&c.user),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = c
        .t
        .call(
            Method::GET,
            &format!("/channels/{}/messages?limit=abc", c.text),
            Some(&c.user),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn chat_validation() {
    let c = chat_setup().await;
    let (s, v) = c.send(&c.user, c.text, json!({"content": "   "})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let long = "я".repeat(proto::limits::MESSAGE_MAX + 1);
    let (s, _) = c.send(&c.user, c.text, json!({ "content": long })).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let ok = "я".repeat(proto::limits::MESSAGE_MAX);
    let (s, _) = c.send(&c.user, c.text, json!({ "content": ok })).await;
    assert_eq!(s, StatusCode::CREATED);
    // голосовой канал — не чат
    let (s, _) = c.send(&c.user, c.voice, json!({"content": "x"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = c
        .t
        .call(
            Method::GET,
            &format!("/channels/{}/messages", c.voice),
            Some(&c.user),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = c.send(&c.user, 9999, json!({"content": "x"})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // без входа
    let (s, _) = c
        .t
        .call(
            Method::GET,
            &format!("/channels/{}/messages", c.text),
            None,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn chat_rate_limited() {
    let c = chat_setup().await;
    for _ in 0..backend::CHAT_MESSAGES {
        assert_eq!(
            c.send(&c.user, c.text, json!({"content": "spam"})).await.0,
            StatusCode::CREATED
        );
    }
    let (s, v) = c.send(&c.user, c.text, json!({"content": "spam"})).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(err(&v), "rate_limited");
    // у другого пользователя свой лимит
    assert_eq!(
        c.send(&c.admin, c.text, json!({"content": "ok"})).await.0,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn chat_images() {
    let c = chat_setup().await;
    let (s, v, _, _) = c
        .t
        .call_raw(Method::POST, "/uploads", Some(&c.user), b"<svg></svg>".to_vec())
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let (s, _, _, _) = c
        .t
        .call_raw(Method::POST, "/uploads", None, png(10, 10))
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _, _) = c
        .t
        .call_raw(Method::POST, "/uploads", Some(&c.user), png(20_000, 10))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let mut big = png(10, 10);
    big.resize(proto::limits::IMAGE_MAX_BYTES + 1, 0);
    let (s, v, _, _) = c
        .t
        .call_raw(Method::POST, "/uploads", Some(&c.user), big)
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let mut too_big = png(10, 10);
    too_big.resize(proto::limits::IMAGE_MAX_BYTES + 200 * 1024, 0);
    let (s, v, _, _) = c
        .t
        .call_raw(Method::POST, "/uploads", Some(&c.user), too_big)
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");

    let img = png(640, 480);
    let (s, v, _, _) = c
        .t
        .call_raw(Method::POST, "/uploads", Some(&c.user), img.clone())
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let att: Attachment = serde_json::from_value(v).unwrap();
    assert_eq!((att.width, att.height, att.mime.as_str()), (640, 480, "image/png"));

    // неотправленную видит только загрузивший
    let url = format!("/attachments/{}", att.id);
    assert_eq!(
        c.t.call_raw(Method::GET, &url, Some(&c.admin), vec![]).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        c.t.call_raw(Method::GET, &url, Some(&c.user), vec![]).await.0,
        StatusCode::OK
    );
    // чужое вложение прикрепить нельзя (и сообщение не создаётся)
    let (s, _) = c
        .send(&c.admin, c.text, json!({"content": "x", "attachments": [att.id]}))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(c.history(&c.admin, c.text, "").await.is_empty());

    // только картинка, без текста
    let (s, v) = c
        .send(&c.user, c.text, json!({"attachments": [att.id, att.id]}))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let m: Message = serde_json::from_value(v).unwrap();
    assert_eq!(m.attachments, vec![att.clone()]);
    assert_eq!(c.history(&c.admin, c.text, "").await[0].attachments, vec![att.clone()]);
    // повторно — уже отправлено
    let (s, _) = c
        .send(&c.user, c.text, json!({"attachments": [att.id]}))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // теперь видят все, с правильным типом
    let (s, _, h, body) = c.t.call_raw(Method::GET, &url, Some(&c.admin), vec![]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[header::CONTENT_TYPE], "image/png");
    assert_eq!(h["x-content-type-options"], "nosniff");
    // файл перекодируется сервером (метаданные не переносятся): остаётся PNG тех же размеров
    assert!(body.starts_with(b"\x89PNG") && body.len() == att.size as usize);
    let _ = &img;
    assert_eq!(
        c.t.call_raw(Method::GET, &url, None, vec![]).await.0,
        StatusCode::UNAUTHORIZED
    );
    let files = || std::fs::read_dir(&c.t.uploads).map(|d| d.count()).unwrap_or(0);
    assert_eq!(files(), 1);

    // удаление канала уносит и файлы
    let (s, _) = c
        .t
        .call(
            Method::DELETE,
            &format!("/channels/{}", c.text),
            Some(&c.admin),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(files(), 0);
    assert_eq!(
        c.t.call_raw(Method::GET, &url, Some(&c.admin), vec![]).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn stale_uploads_cleaned() {
    let c = chat_setup().await;
    let (s, v, _, _) = c
        .t
        .call_raw(Method::POST, "/uploads", Some(&c.user), png(4, 4))
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let att: Attachment = serde_json::from_value(v).unwrap();
    assert_eq!(backend::chat::cleanup_pending(&c.t.state).await.unwrap(), 0);
    sqlx::query("UPDATE attachments SET created_at = 1")
        .execute(&c.t.pool)
        .await
        .unwrap();
    assert_eq!(backend::chat::cleanup_pending(&c.t.state).await.unwrap(), 1);
    let (s, _) = c
        .send(&c.user, c.text, json!({"attachments": [att.id]}))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// Читает из SSE-потока следующее событие с данными (пропуская комментарии keep-alive).

#[tokio::test]
async fn events_stream() {
    let c = chat_setup().await;
    let unauth = Request::builder().uri("/events").body(Body::empty()).unwrap();
    assert_eq!(
        c.t.app.clone().oneshot(unauth).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let req = Request::builder()
        .uri("/events")
        .header(header::AUTHORIZATION, format!("Bearer {}", c.admin))
        .body(Body::empty())
        .unwrap();
    let resp = c.t.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/event-stream");
    assert_eq!(resp.headers()["x-accel-buffering"], "no");
    let mut body = resp.into_body();
    let mut buf = String::new();
    assert_eq!(next_event(&mut body, &mut buf).await, ServerEvent::Hello);

    let (s, v) = c.send(&c.user, c.text, json!({"content": "привет"})).await;
    assert_eq!(s, StatusCode::CREATED);
    let sent: Message = serde_json::from_value(v).unwrap();
    match next_event(&mut body, &mut buf).await {
        ServerEvent::MessageCreated { message } => assert_eq!(message, sent),
        e => panic!("ожидали message_created, пришло {e:?}"),
    }

    // остановка сервиса закрывает поток
    c.t.state.close_streams();
    let end = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match body.frame().await {
                None => break,
                Some(Ok(_)) => continue,
                Some(Err(e)) => panic!("{e}"),
            }
        }
    })
    .await;
    assert!(end.is_ok(), "поток не закрылся");
}

// ------------------------------------------------------------ выпуски клиента

#[tokio::test]
async fn releases_version_and_download() {
    let t = setup().await;
    // выпуска ещё нет
    let (st, v) = t.call(Method::GET, "/version", None, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
    let (st, _, _, _) = t.call_raw(Method::GET, "/get", None, vec![]).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let dir = t.uploads.join("releases");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("amadeus-0.2.0.exe"), b"MZ-new-client").unwrap();
    let info = json!({
        "version": "0.2.0",
        "notes": "Трей и автообновление",
        "file": "amadeus-0.2.0.exe",
        "sha256": "0".repeat(64),
        "size": 13,
        "installer": "amadeus-setup-0.2.0.exe"
    });
    std::fs::write(dir.join("latest.json"), info.to_string()).unwrap();

    // без авторизации
    let (st, _, headers, bytes) = t.call_raw(Method::GET, "/version", None, vec![]).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
    let got: proto::ReleaseInfo = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(got.version, "0.2.0");
    assert_eq!(got.size, 13);

    let (st, _, _, bytes) = t.call_raw(Method::GET, "/download/amadeus-0.2.0.exe", None, vec![]).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(bytes, b"MZ-new-client");

    let (st, _, headers, _) = t.call_raw(Method::GET, "/get", None, vec![]).await;
    assert_eq!(st, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(headers[header::LOCATION], "download/amadeus-setup-0.2.0.exe");

    // наружу из папки не выйти
    for bad in ["/download/../api.rs", "/download/..%2F..%2FCargo.toml", "/download/missing.exe"] {
        let (st, _, _, _) = t.call_raw(Method::GET, bad, None, vec![]).await;
        assert!(st == StatusCode::NOT_FOUND || st == StatusCode::BAD_REQUEST, "{bad}: {st}");
    }

    // испорченный latest.json — ошибка сервера, а не странный ответ
    std::fs::write(
        dir.join("latest.json"),
        json!({"version": "0.2.0", "file": "../x", "sha256": "0".repeat(64), "size": 1}).to_string(),
    )
    .unwrap();
    let (st, _) = t.call(Method::GET, "/version", None, None).await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
}


// ------------------------------------------------------------ обновления с официального сервера (upstream)

mod upstream {
    use super::common::*;
    use axum::{
        Router,
        http::{Method, StatusCode, header},
        response::IntoResponse,
        routing::get,
    };
    use backend::releases::Upstream;
    use serde_json::{Value, json};
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    #[derive(Clone)]
    enum Resp {
        Json(Value),
        Status(u16),
        Body(Vec<u8>),
        Hang,
    }

    #[derive(Clone)]
    struct Fake {
        resp: Arc<Mutex<Resp>>,
        hits: Arc<AtomicUsize>,
        base: String,
    }

    impl Fake {
        fn set(&self, r: Resp) {
            *self.resp.lock().unwrap() = r;
        }
        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    /// Настоящий HTTP-сервер на 127.0.0.1 — «официальный сервер» с /version.
    async fn fake() -> Fake {
        let f = Fake {
            resp: Arc::new(Mutex::new(Resp::Status(404))),
            hits: Arc::new(AtomicUsize::new(0)),
            base: String::new(),
        };
        let st = f.clone();
        let app = Router::new().route(
            "/version",
            get(move || {
                let st = st.clone();
                async move {
                    st.hits.fetch_add(1, Ordering::SeqCst);
                    let r = st.resp.lock().unwrap().clone();
                    match r {
                        Resp::Json(v) => axum::Json(v).into_response(),
                        Resp::Status(s) => StatusCode::from_u16(s).unwrap().into_response(),
                        Resp::Body(b) => b.into_response(),
                        Resp::Hang => {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            StatusCode::OK.into_response()
                        }
                    }
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        Fake { base: format!("http://{addr}"), ..f }
    }

    fn release(version: &str, min_server: Option<&str>) -> Value {
        let mut v = json!({
            "version": version,
            "notes": "новое",
            "file": format!("murmur-{version}.exe"),
            "sha256": "0".repeat(64),
            "size": 10,
            "installer": format!("murmur-setup-{version}.exe"),
        });
        if let Some(m) = min_server {
            v["min_server"] = json!(m);
        }
        v
    }

    fn up(f: &Fake) -> Upstream {
        Upstream::new(&f.base, "0.3.13").with_timings(
            Duration::from_secs(300),
            Duration::from_secs(86400),
            Duration::from_millis(500),
        )
    }

    #[tokio::test]
    async fn upstream_relays_version() {
        let f = fake().await;
        f.set(Resp::Json(release("0.3.14", None)));
        let t = setup_with_upstream(up(&f)).await;
        let (st, v) = t.call(Method::GET, "/version", None, None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["version"], "0.3.14");
        let (st, _) = t.call(Method::GET, "/version", None, None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(f.hits(), 1, "второй запрос — из кеша");
    }

    #[tokio::test]
    async fn upstream_local_wins() {
        let f = fake().await;
        f.set(Resp::Json(release("0.3.14", None)));
        let t = setup_with_upstream(up(&f)).await;
        let dir = t.uploads.join("releases");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("latest.json"), release("0.3.10", None).to_string()).unwrap();
        let (st, v) = t.call(Method::GET, "/version", None, None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["version"], "0.3.10");
        assert_eq!(f.hits(), 0);
    }

    #[tokio::test]
    async fn upstream_incompatible_hidden() {
        let f = fake().await;
        f.set(Resp::Json(release("0.3.14", Some("99.0.0"))));
        let t = setup_with_upstream(up(&f)).await;
        let (st, _) = t.call(Method::GET, "/version", None, None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        f.set(Resp::Json(release("0.3.14", Some("0.3.13"))));
        let t = setup_with_upstream(up(&f)).await;
        let (st, _) = t.call(Method::GET, "/version", None, None).await;
        assert_eq!(st, StatusCode::OK);
    }

    #[tokio::test]
    async fn upstream_garbage_is_not_found() {
        let f = fake().await;
        for bad in [
            Resp::Body(b"<html>maintenance</html>".to_vec()),
            Resp::Body(vec![b'{'; 2 * 1024 * 1024]),
            Resp::Status(500),
            Resp::Json(json!({"version": "0.3.14", "file": "../x", "sha256": "0", "size": 1})),
        ] {
            f.set(bad);
            let t = setup_with_upstream(up(&f)).await;
            let (st, _) = t.call(Method::GET, "/version", None, None).await;
            assert_eq!(st, StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn upstream_timeout_uses_stale_cache() {
        let f = fake().await;
        f.set(Resp::Json(release("0.3.14", None)));
        let stale = Upstream::new(&f.base, "0.3.13").with_timings(
            Duration::ZERO,
            Duration::from_secs(86400),
            Duration::from_millis(300),
        );
        let t = setup_with_upstream(stale).await;
        let (st, _) = t.call(Method::GET, "/version", None, None).await;
        assert_eq!(st, StatusCode::OK);
        f.set(Resp::Hang);
        let started = std::time::Instant::now();
        let (st, v) = t.call(Method::GET, "/version", None, None).await;
        assert_eq!(st, StatusCode::OK, "старый кеш");
        assert_eq!(v["version"], "0.3.14");
        assert!(started.elapsed() < Duration::from_secs(5), "не висим дольше таймаута");

        // кеш слишком старый — «обновлений нет»
        f.set(Resp::Json(release("0.3.14", None)));
        let no_stale = Upstream::new(&f.base, "0.3.13").with_timings(Duration::ZERO, Duration::ZERO, Duration::from_millis(300));
        let t = setup_with_upstream(no_stale).await;
        let (st, _) = t.call(Method::GET, "/version", None, None).await;
        assert_eq!(st, StatusCode::OK);
        f.set(Resp::Hang);
        let (st, _) = t.call(Method::GET, "/version", None, None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn upstream_download_redirects() {
        let f = fake().await;
        let t = setup_with_upstream(up(&f)).await;
        let (st, _, h, _) = t.call_raw(Method::GET, "/download/murmur-9.9.9.exe", None, vec![]).await;
        assert_eq!(st, StatusCode::FOUND);
        assert_eq!(h[header::LOCATION], format!("{}/download/murmur-9.9.9.exe", f.base));
        for bad in ["/download/..%2Fx", "/download/.hidden", "/download/a%5Cb"] {
            let (st, _, _, _) = t.call_raw(Method::GET, bad, None, vec![]).await;
            assert!(st == StatusCode::NOT_FOUND || st == StatusCode::BAD_REQUEST, "{bad}: {st}");
        }
        // свой файл по-прежнему отдаётся сам
        let dir = t.uploads.join("releases");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("murmur-0.3.10.exe"), b"MZ-local").unwrap();
        let (st, _, _, b) = t.call_raw(Method::GET, "/download/murmur-0.3.10.exe", None, vec![]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(b, b"MZ-local");
    }

    #[tokio::test]
    async fn upstream_hang_parallel_not_queued() {
        let f = fake().await;
        f.set(Resp::Json(release("0.3.14", None)));
        let up = Upstream::new(&f.base, "0.3.13").with_timings(
            Duration::ZERO,
            Duration::from_secs(86400),
            Duration::from_millis(400),
        );
        let t = setup_with_upstream(up).await;
        let (st, _) = t.call(Method::GET, "/version", None, None).await;
        assert_eq!(st, StatusCode::OK);
        f.set(Resp::Hang);
        let started = std::time::Instant::now();
        let mut tasks = Vec::new();
        for _ in 0..5 {
            let app = t.app.clone();
            tasks.push(tokio::spawn(async move {
                use tower::ServiceExt;
                let req = axum::http::Request::builder()
                    .uri("/version")
                    .header("x-forwarded-for", "203.0.113.1")
                    .body(axum::body::Body::empty())
                    .unwrap();
                app.oneshot(req).await.unwrap().status()
            }));
        }
        for t in tasks {
            assert_eq!(t.await.unwrap(), StatusCode::OK, "старый кеш, а не ошибка");
        }
        assert!(started.elapsed() < Duration::from_millis(800), "не в очереди: {:?}", started.elapsed());
        // после неудачи upstream какое-то время не дёргается
        let hits = f.hits();
        let (st, _) = t.call(Method::GET, "/version", None, None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(f.hits(), hits, "повтор к зависшему upstream не сразу");
    }

    #[tokio::test]
    async fn upstream_disabled_keeps_404() {
        let t = setup().await;
        let (st, _, _, _) = t.call_raw(Method::GET, "/download/murmur-9.9.9.exe", None, vec![]).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn admin_exists_cli() {
    let t = setup().await;
    assert!(!backend::admin_exists(&t.pool).await.unwrap());
    backend::create_user(&t.pool, "user", "user-password", false).await.unwrap();
    assert!(!backend::admin_exists(&t.pool).await.unwrap(), "обычный пользователь — не админ");
    backend::create_user(&t.pool, "boss", "boss-password", true).await.unwrap();
    assert!(backend::admin_exists(&t.pool).await.unwrap());
}
