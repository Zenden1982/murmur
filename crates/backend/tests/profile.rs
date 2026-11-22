//! Профиль: отображаемое имя и аватар (фаза 9.1).

mod common;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use common::*;
use livekit_api::access_token::TokenVerifier;
use proto::{ChannelLayout, JoinResponse, Message, ServerEvent, User};
use serde_json::json;
use tower::ServiceExt;

/// Настоящий PNG w×h; seed меняет содержимое (другой хеш).
fn png(w: u32, h: u32, seed: u8) -> Vec<u8> {
    use image::ImageEncoder;
    let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, seed, 255]));
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out).write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8).unwrap();
    out
}

fn gif2(w: u16, h: u16) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = gif::Encoder::new(&mut out, w, h, &[0, 0, 0, 255, 255, 255]).unwrap();
        for i in 0..2u8 {
            let f = gif::Frame { width: w, height: h, delay: 10, buffer: std::borrow::Cow::Owned(vec![i; w as usize * h as usize]), ..Default::default() };
            enc.write_frame(&f).unwrap();
        }
    }
    out
}

fn is_hash(k: &str) -> bool {
    k.len() == 64 && k.bytes().all(|b| b.is_ascii_hexdigit())
}

async fn me(t: &T, tok: &str) -> User {
    let (s, v) = t.call(Method::GET, "/me", Some(tok), None).await;
    assert_eq!(s, StatusCode::OK);
    serde_json::from_value(v).unwrap()
}

#[tokio::test]
async fn display_name_set_and_reset() {
    let t = setup().await;
    let (tok, _) = t.user("okabe").await;
    let (s, v) = t.call(Method::PATCH, "/me", Some(&tok), Some(json!({"display_name": "  Окабэ Ринтаро "}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(serde_json::from_value::<User>(v).unwrap().display_name.as_deref(), Some("Окабэ Ринтаро"));
    assert_eq!(me(&t, &tok).await.display_name.as_deref(), Some("Окабэ Ринтаро"));
    let login = t.login("okabe", "password123").await;
    assert_eq!(me(&t, &login).await.display_name.as_deref(), Some("Окабэ Ринтаро"));
    let (s, _) = t.call(Method::PATCH, "/me", Some(&tok), Some(json!({"display_name": ""}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(me(&t, &tok).await.display_name, None);
}

#[tokio::test]
async fn display_name_too_long() {
    let t = setup().await;
    let (tok, _) = t.user("okabe").await;
    let (s, _) = t.call(Method::PATCH, "/me", Some(&tok), Some(json!({"display_name": "я".repeat(33)}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn avatar_upload_replace_delete() {
    let t = setup().await;
    let (tok, _) = t.user("okabe").await;
    let (s, v, _, _) = t.call_raw(Method::PUT, "/me/avatar", Some(&tok), png(200, 100, 1)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let (s, v, _, _) = t.call_raw(Method::PUT, "/me/avatar", Some(&tok), png(256, 256, 1)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let k1 = serde_json::from_value::<User>(v).unwrap().avatar.unwrap();
    assert!(t.uploads.join(&k1).exists());
    let (_, v, _, _) = t.call_raw(Method::PUT, "/me/avatar", Some(&tok), png(256, 256, 2)).await;
    let k2 = serde_json::from_value::<User>(v).unwrap().avatar.unwrap();
    assert!(!t.uploads.join(&k1).exists());
    assert_eq!(me(&t, &tok).await.avatar.as_deref(), Some(k2.as_str()));
    let (s, _) = t.call(Method::DELETE, "/me/avatar", Some(&tok), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(!t.uploads.join(&k2).exists());
    assert_eq!(me(&t, &tok).await.avatar, None);
}

#[tokio::test]
async fn author_and_token_use_display_name() {
    let t = setup().await;
    let (tok, _) = t.user("okabe").await;
    let srv = t.create_server(&tok, "Лаб").await;
    t.call(Method::PATCH, "/me", Some(&tok), Some(json!({"display_name": "Окабэ"}))).await;
    t.call_raw(Method::PUT, "/me/avatar", Some(&tok), png(256, 256, 3)).await;
    let (_, v) = t.call(Method::GET, &format!("/servers/{}/channels", srv.id), Some(&tok), None).await;
    let l: ChannelLayout = serde_json::from_value(v).unwrap();
    let text = l.channels.iter().find(|c| c.kind == proto::ChannelKind::Text).unwrap().id;
    let voice = l.channels.iter().find(|c| c.kind == proto::ChannelKind::Voice).unwrap().id;
    let (_, v) = t.call(Method::POST, &format!("/channels/{text}/messages"), Some(&tok), Some(json!({"content": "эль псай конгру"}))).await;
    let m: Message = serde_json::from_value(v).unwrap();
    assert_eq!(m.author.display_name.as_deref(), Some("Окабэ"));
    assert!(m.author.avatar.is_some());
    let (_, v) = t.call(Method::POST, &format!("/channels/{voice}/join"), Some(&tok), None).await;
    let j: JoinResponse = serde_json::from_value(v).unwrap();
    assert_eq!(TokenVerifier::with_api_key(LK_KEY, LK_SECRET).verify(&j.livekit_token).unwrap().name, "Окабэ");
    // участники сервера видят новое имя
    let (_, v) = t.call(Method::GET, &format!("/servers/{}/members", srv.id), Some(&tok), None).await;
    assert_eq!(v[0]["user"]["display_name"], "Окабэ");
}

#[tokio::test]
async fn user_updated_goes_to_shared_servers_only() {
    let t = setup().await;
    let a = t.user("okabe").await;
    let b = t.user("kurisu").await;
    let c = t.user("suzuha").await;
    let srv = t.create_server(&a.0, "Лаб").await;
    t.add_member(srv.id, b.1, 0).await;
    let open = |tok: String| {
        let app = t.app.clone();
        async move {
            let req = Request::builder().uri("/events").header(header::AUTHORIZATION, format!("Bearer {tok}")).body(Body::empty()).unwrap();
            let mut body = app.oneshot(req).await.unwrap().into_body();
            let mut buf = String::new();
            assert_eq!(next_event(&mut body, &mut buf).await, ServerEvent::Hello);
            (body, buf)
        }
    };
    let (mut bb, mut bbuf) = open(b.0.clone()).await;
    let (mut cb, mut cbuf) = open(c.0.clone()).await;
    t.call(Method::PATCH, "/me", Some(&a.0), Some(json!({"display_name": "Хооин Кёма"}))).await;
    match next_event(&mut bb, &mut bbuf).await {
        ServerEvent::UserUpdated { user } => assert_eq!(user.display_name.as_deref(), Some("Хооин Кёма")),
        e => panic!("{e:?}"),
    }
    let own = t.create_server(&c.0, "Своё").await;
    match next_event(&mut cb, &mut cbuf).await {
        ServerEvent::ServerUpserted { server } => assert_eq!(server.id, own.id),
        e => panic!("чужому пришло {e:?}"),
    }
}

#[tokio::test]
async fn avatar_gif_upload() {
    let t = setup().await;
    let (tok, _) = t.user("okabe").await;
    let (s, v, _, _) = t.call_raw(Method::PUT, "/me/avatar", Some(&tok), gif2(256, 256)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let k = serde_json::from_value::<User>(v).unwrap().avatar.unwrap();
    assert!(is_hash(&k), "{k}");
    let (s, _, h, body) = t.call_raw(Method::GET, &format!("/images/{k}"), Some(&tok), vec![]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[header::CONTENT_TYPE], "image/gif");
    assert!(body.starts_with(b"GIF89a"));
}

#[tokio::test]
async fn banner_upload_and_delete() {
    let t = setup().await;
    let (a, _) = t.user("okabe").await;
    let (b, bid) = t.user("kurisu").await;
    let srv = t.create_server(&b, "Лаб").await;
    let aid = me(&t, &a).await.id;
    t.add_member(srv.id, aid, 0).await;
    let (s, v, _, _) = t.call_raw(Method::PUT, "/me/banner", Some(&a), png(960, 384, 1)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let k = serde_json::from_value::<User>(v).unwrap().banner.unwrap();
    assert!(is_hash(&k));
    assert_eq!(me(&t, &a).await.banner.as_deref(), Some(k.as_str()));
    let (s, v) = t.call(Method::GET, &format!("/users/{aid}"), Some(&b), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["banner"], k.as_str());
    let (s, _, _, _) = t.call_raw(Method::GET, &format!("/images/{k}"), Some(&b), vec![]).await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = t.call(Method::DELETE, "/me/banner", Some(&a), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(me(&t, &a).await.banner, None);
    assert!(!t.uploads.join(&k).exists());
    let _ = bid;
}

#[tokio::test]
async fn banner_wrong_size() {
    let t = setup().await;
    let (tok, _) = t.user("okabe").await;
    let (s, _, _, _) = t.call_raw(Method::PUT, "/me/banner", Some(&tok), png(960, 480, 1)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn shared_key_not_deleted() {
    let t = setup().await;
    let (a, _) = t.user("okabe").await;
    let (b, _) = t.user("kurisu").await;
    let (_, va, _, _) = t.call_raw(Method::PUT, "/me/avatar", Some(&a), png(256, 256, 7)).await;
    let (_, vb, _, _) = t.call_raw(Method::PUT, "/me/avatar", Some(&b), png(256, 256, 7)).await;
    let ka = serde_json::from_value::<User>(va).unwrap().avatar.unwrap();
    let kb = serde_json::from_value::<User>(vb).unwrap().avatar.unwrap();
    assert_eq!(ka, kb);
    let (s, _) = t.call(Method::DELETE, "/me/avatar", Some(&a), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(t.uploads.join(&kb).exists());
    let (s, _, _, _) = t.call_raw(Method::GET, &format!("/images/{kb}"), Some(&b), vec![]).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn same_file_twice_keeps_file() {
    let t = setup().await;
    let (a, _) = t.user("okabe").await;
    t.call_raw(Method::PUT, "/me/avatar", Some(&a), png(256, 256, 9)).await;
    let (s, v, _, _) = t.call_raw(Method::PUT, "/me/avatar", Some(&a), png(256, 256, 9)).await;
    assert_eq!(s, StatusCode::OK);
    let k = serde_json::from_value::<User>(v).unwrap().avatar.unwrap();
    assert!(t.uploads.join(&k).exists());
}

#[tokio::test]
async fn old_random_avatar_still_served() {
    let t = setup().await;
    let (a, aid) = t.user("okabe").await;
    let key = "oldrandomtoken0123456789abcdefABCDEF";
    std::fs::create_dir_all(&t.uploads).unwrap();
    std::fs::write(t.uploads.join(key), png(256, 256, 1)).unwrap();
    sqlx::query("UPDATE users SET avatar_key = ? WHERE id = ?").bind(key).bind(aid).execute(&t.pool).await.unwrap();
    let (s, _, _, _) = t.call_raw(Method::GET, &format!("/images/{key}"), Some(&a), vec![]).await;
    assert_eq!(s, StatusCode::OK);
}
