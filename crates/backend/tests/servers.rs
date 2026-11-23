//! Серверы, участники, роли, баны, приглашения (фаза 9.1).

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use proto::{Member, Server};
use serde_json::json;

/// Заголовок PNG нужного размера (сервер смотрит только на заголовок).
pub fn fake_png(w: u32, h: u32) -> Vec<u8> {
    if (w as u64) * (h as u64) <= 1_000_000 && w > 0 && h > 0 {
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
    v.extend_from_slice(&[0u8; 64]);
    v
}

async fn servers_of(t: &T, tok: &str) -> Vec<Server> {
    let (s, v) = t.call(Method::GET, "/servers", Some(tok), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    serde_json::from_value(v).unwrap()
}

#[tokio::test]
async fn create_server_makes_owner_and_default_channels() {
    let t = setup().await;
    let (tok, me) = t.user("okabe").await;
    let srv = t.create_server(&tok, "Лаборатория").await;
    assert_eq!(srv.owner_id, me);
    assert_eq!(srv.name, "Лаборатория");
    let mut pos: Vec<i32> = srv.roles.iter().map(|r| r.position).collect();
    pos.sort();
    assert_eq!(pos, vec![0, 1, 2, 3]);
    let mine = srv.roles.iter().find(|r| r.id == srv.my_role_id).unwrap();
    assert_eq!(mine.position, 3);
    let chans: Vec<(String, String)> = sqlx::query_as("SELECT name, kind FROM channels WHERE server_id = ? ORDER BY position")
        .bind(srv.id).fetch_all(&t.pool).await.unwrap();
    assert_eq!(chans, vec![("general".into(), "text".into()), ("General".into(), "voice".into())]);
    assert_eq!(servers_of(&t, &tok).await, vec![srv]);
}

#[tokio::test]
async fn name_validated() {
    let t = setup().await;
    let (tok, _) = t.user("okabe").await;
    let (s, v) = t.call(Method::POST, "/servers", Some(&tok), Some(json!({"name": "   "}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let long = "я".repeat(51);
    let (s, _) = t.call(Method::POST, "/servers", Some(&tok), Some(json!({"name": long}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn member_cannot_rename_admin_can() {
    let t = setup().await;
    let (owner, _) = t.user("okabe").await;
    let (member, mid) = t.user("daru").await;
    let (admin, aid) = t.user("kurisu").await;
    let srv = t.create_server(&owner, "Лаб").await;
    t.add_member(srv.id, mid, 0).await;
    t.add_member(srv.id, aid, 2).await;
    let uri = format!("/servers/{}", srv.id);
    let (s, _) = t.call(Method::PATCH, &uri, Some(&member), Some(json!({"name": "Взлом"}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, v) = t.call(Method::PATCH, &uri, Some(&admin), Some(json!({"name": "Лаборатория"}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(serde_json::from_value::<Server>(v).unwrap().name, "Лаборатория");
}

#[tokio::test]
async fn non_member_gets_404() {
    let t = setup().await;
    let (owner, _) = t.user("okabe").await;
    let (stranger, _) = t.user("suzuha").await;
    let srv = t.create_server(&owner, "Лаб").await;
    for (m, uri) in [(Method::GET, format!("/servers/{}/members", srv.id)), (Method::PATCH, format!("/servers/{}", srv.id)), (Method::DELETE, format!("/servers/{}", srv.id))] {
        let (s, _) = t.call(m, &uri, Some(&stranger), Some(json!({"name": "x"}))).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{uri}");
    }
    assert!(servers_of(&t, &stranger).await.is_empty());
}

#[tokio::test]
async fn members_list() {
    let t = setup().await;
    let (owner, oid) = t.user("okabe").await;
    let (_, mid) = t.user("daru").await;
    let srv = t.create_server(&owner, "Лаб").await;
    t.add_member(srv.id, mid, 0).await;
    let (s, v) = t.call(Method::GET, &format!("/servers/{}/members", srv.id), Some(&owner), None).await;
    assert_eq!(s, StatusCode::OK);
    let mut ms: Vec<Member> = serde_json::from_value(v).unwrap();
    ms.sort_by_key(|m| m.user.id);
    assert_eq!(ms.iter().map(|m| (m.user.id, m.user.name.as_str())).collect::<Vec<_>>(), vec![(oid, "okabe"), (mid, "daru")]);
}

#[tokio::test]
async fn owner_cannot_leave_member_can() {
    let t = setup().await;
    let (owner, _) = t.user("okabe").await;
    let (member, mid) = t.user("daru").await;
    let srv = t.create_server(&owner, "Лаб").await;
    t.add_member(srv.id, mid, 0).await;
    let uri = format!("/servers/{}/members/@me", srv.id);
    let (s, _) = t.call(Method::DELETE, &uri, Some(&owner), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = t.call(Method::DELETE, &uri, Some(&member), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(servers_of(&t, &member).await.is_empty());
}

#[tokio::test]
async fn transfer_ownership_swaps_roles() {
    let t = setup().await;
    let (owner, oid) = t.user("okabe").await;
    let (heir, hid) = t.user("kurisu").await;
    let srv = t.create_server(&owner, "Лаб").await;
    t.add_member(srv.id, hid, 0).await;
    let (s, _) = t.call(Method::POST, &format!("/servers/{}/transfer", srv.id), Some(&heir), Some(json!({"user_id": hid}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "передать может только владелец");
    let (s, v) = t.call(Method::POST, &format!("/servers/{}/transfer", srv.id), Some(&owner), Some(json!({"user_id": hid}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let now = &servers_of(&t, &heir).await[0];
    assert_eq!(now.owner_id, hid);
    assert_eq!(now.roles.iter().find(|r| r.id == now.my_role_id).unwrap().position, 3);
    let old = &servers_of(&t, &owner).await[0];
    assert_eq!(old.roles.iter().find(|r| r.id == old.my_role_id).unwrap().position, 2);
    let _ = oid;
}

#[tokio::test]
async fn icon_upload_validates_and_replaces() {
    let t = setup().await;
    let (owner, _) = t.user("okabe").await;
    let srv = t.create_server(&owner, "Лаб").await;
    let uri = format!("/servers/{}/icon", srv.id);
    let (s, _, _, _) = t.call_raw(Method::PUT, &uri, Some(&owner), fake_png(256, 200)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "не квадрат");
    let (s, _, _, _) = t.call_raw(Method::PUT, &uri, Some(&owner), fake_png(1024, 1024)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "больше 512");
    let (s, v, _, _) = t.call_raw(Method::PUT, &uri, Some(&owner), fake_png(256, 256)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let k1 = serde_json::from_value::<Server>(v).unwrap().icon.unwrap();
    assert!(t.uploads.join(&k1).exists());
    let (s, v, _, _) = t.call_raw(Method::PUT, &uri, Some(&owner), fake_png(128, 128)).await;
    assert_eq!(s, StatusCode::OK);
    let k2 = serde_json::from_value::<Server>(v).unwrap().icon.unwrap();
    assert!(!t.uploads.join(&k1).exists(), "старая иконка удалена");
    let (s, _, _, body) = t.call_raw(Method::GET, &format!("/images/{k2}"), Some(&owner), vec![]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(&body[..4], b"\x89PNG");
}

#[tokio::test]
async fn delete_server_removes_files_and_voice() {
    let t = setup().await;
    let (owner, _) = t.user("okabe").await;
    let (admin, aid) = t.user("kurisu").await;
    let srv = t.create_server(&owner, "Лаб").await;
    t.add_member(srv.id, aid, 2).await;
    let (_, v, _, _) = t.call_raw(Method::PUT, &format!("/servers/{}/icon", srv.id), Some(&owner), fake_png(64, 64)).await;
    let key = serde_json::from_value::<Server>(v).unwrap().icon.unwrap();
    let voice: i64 = sqlx::query_scalar("SELECT id FROM channels WHERE server_id = ? AND kind = 'voice'")
        .bind(srv.id).fetch_one(&t.pool).await.unwrap();
    let room = format!("srv-{}-ch-{voice}", srv.id);
    t.set_voice(&room, &[aid]).await;
    let (s, _) = t.call(Method::DELETE, &format!("/servers/{}", srv.id), Some(&admin), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "удалить может только владелец");
    let (s, _) = t.call(Method::DELETE, &format!("/servers/{}", srv.id), Some(&owner), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(!t.uploads.join(&key).exists());
    assert_eq!(t.removed(), vec![(room, aid.to_string())]);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channels WHERE server_id = ?").bind(srv.id).fetch_one(&t.pool).await.unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn server_limit_per_owner() {
    let t = setup().await;
    let (tok, _) = t.user("okabe").await;
    for i in 0..proto::limits::SERVERS_OWNED_MAX {
        t.create_server(&tok, &format!("s{i}")).await;
    }
    let (s, _) = t.call(Method::POST, "/servers", Some(&tok), Some(json!({"name": "лишний"}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}

// ------------------------------------------------------------------ приглашения (задача 6)

async fn server_invite(t: &T, tok: &str, server: i64, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    t.call(Method::POST, &format!("/servers/{server}/invites"), Some(tok), Some(body)).await
}

async fn new_invite(t: &T, tok: &str, server: i64, uses: u32) -> proto::Invite {
    let (s, v) = server_invite(t, tok, server, json!({"uses": uses})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    serde_json::from_value(v).unwrap()
}

async fn invites_of(t: &T, tok: &str, server: i64) -> Vec<proto::Invite> {
    let (s, v) = t.call(Method::GET, &format!("/servers/{server}/invites"), Some(tok), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    serde_json::from_value(v).unwrap()
}

#[tokio::test]
async fn register_by_server_invite_joins_that_server() {
    let t = setup().await;
    let (owner, _) = t.user("okabe").await;
    let srv = t.create_server(&owner, "Лаб").await;
    let inv = new_invite(&t, &owner, srv.id, 1).await;
    assert_eq!(inv.server_id, srv.id);
    let (s, v) = t.register(&inv.code, "mayuri").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let tok = serde_json::from_value::<proto::AuthResponse>(v).unwrap().token;
    let list = servers_of(&t, &tok).await;
    assert_eq!(list.iter().map(|s| s.id).collect::<Vec<_>>(), vec![srv.id]);
    let mine = &list[0];
    assert_eq!(mine.roles.iter().find(|r| r.id == mine.my_role_id).unwrap().position, 0);
}

#[tokio::test]
async fn existing_user_joins_by_code() {
    let t = setup().await;
    let (owner, _) = t.user("okabe").await;
    let (guest, _) = t.user("suzuha").await;
    let srv = t.create_server(&owner, "Лаб").await;
    let inv = new_invite(&t, &owner, srv.id, 2).await;
    let (s, v) = t.call(Method::POST, &format!("/invites/{}/join", inv.code.to_lowercase()), Some(&guest), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(serde_json::from_value::<Server>(v).unwrap().id, srv.id);
    assert_eq!(servers_of(&t, &guest).await.len(), 1);
    // второй раз — без списания
    let (s, _) = t.call(Method::POST, &format!("/invites/{}/join", inv.code), Some(&guest), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(invites_of(&t, &owner, srv.id).await[0].uses_left, 1);
    let (s, v) = t.call(Method::POST, "/invites/NOPE-NOPE-NOPE/join", Some(&guest), None).await;
    assert_eq!((s, err(&v)), (StatusCode::BAD_REQUEST, "invalid_invite".to_string()));
}

#[tokio::test]
async fn banned_cannot_join_by_new_invite() {
    let t = setup().await;
    let (owner, oid) = t.user("okabe").await;
    let (guest, gid) = t.user("suzuha").await;
    let srv = t.create_server(&owner, "Лаб").await;
    sqlx::query("INSERT INTO server_bans (server_id, user_id, by_user, created_at) VALUES (?, ?, ?, 0)")
        .bind(srv.id).bind(gid).bind(oid).execute(&t.pool).await.unwrap();
    let inv = new_invite(&t, &owner, srv.id, 5).await;
    let (s, v) = t.call(Method::POST, &format!("/invites/{}/join", inv.code), Some(&guest), None).await;
    assert_eq!((s, err(&v)), (StatusCode::FORBIDDEN, "banned".to_string()));
    assert_eq!(invites_of(&t, &owner, srv.id).await[0].uses_left, 5, "попытка не списала использование");
}

#[tokio::test]
async fn any_member_can_invite_and_sees_own() {
    let t = setup().await;
    let (owner, _) = t.user("okabe").await;
    let (member, mid) = t.user("daru").await;
    let (stranger, _) = t.user("suzuha").await;
    let srv = t.create_server(&owner, "Лаб").await;
    t.add_member(srv.id, mid, 0).await;
    let mine = new_invite(&t, &member, srv.id, 1).await;
    let owners = new_invite(&t, &owner, srv.id, 1).await;
    assert_eq!(invites_of(&t, &member, srv.id).await, vec![mine.clone()]);
    assert_eq!(invites_of(&t, &owner, srv.id).await.len(), 2);
    let (s, _) = server_invite(&t, &stranger, srv.id, json!({})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // отозвать: своё — да, чужое — нет, владелец — любое
    let (s, _) = t.call(Method::DELETE, &format!("/invites/{}", owners.code), Some(&member), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = t.call(Method::DELETE, &format!("/invites/{}", mine.code), Some(&member), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = t.call(Method::DELETE, &format!("/invites/{}", owners.code), Some(&owner), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(invites_of(&t, &owner, srv.id).await.iter().all(|i| i.revoked));
}

#[tokio::test]
async fn invite_preview_public() {
    let t = setup().await;
    let (owner, _) = t.user("okabe").await;
    let srv = t.create_server(&owner, "Лаборатория").await;
    let inv = new_invite(&t, &owner, srv.id, 1).await;
    let (s, v) = t.call(Method::GET, &format!("/invites/{}", inv.code), None, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(serde_json::from_value::<proto::InvitePreview>(v).unwrap().server_name, "Лаборатория");
    let (s, _) = t.call(Method::GET, "/invites/NOPE-NOPE-NOPE", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn legacy_post_invites_targets_server_1() {
    let t = setup().await;
    let admin = t.admin().await;
    let inv = t.invite(&admin, json!({})).await;
    assert_eq!(inv.server_id, 1);
    let (s, v) = t.register(&inv.code, "mayuri").await;
    assert_eq!(s, StatusCode::CREATED);
    let tok = serde_json::from_value::<proto::AuthResponse>(v).unwrap().token;
    assert_eq!(servers_of(&t, &tok).await[0].id, 1);
}

// ------------------------------------------------------------------ роли и модерация (задача 7)

fn role_id(srv: &Server, position: i32) -> i64 {
    srv.roles.iter().find(|r| r.position == position).unwrap().id
}

async fn set_role(t: &T, tok: &str, srv: &Server, uid: i64, position: i32) -> StatusCode {
    t.call(Method::PUT, &format!("/servers/{}/members/{uid}/role", srv.id), Some(tok), Some(json!({"role_id": role_id(srv, position)})))
        .await
        .0
}

/// Сервер с владельцем, админом, модератором и участником.
async fn crew(t: &T) -> (Server, [(String, i64); 4]) {
    let o = t.user("okabe").await;
    let a = t.user("kurisu").await;
    let m = t.user("daru").await;
    let u = t.user("mayuri").await;
    let srv = t.create_server(&o.0, "Лаб").await;
    t.add_member(srv.id, a.1, 2).await;
    t.add_member(srv.id, m.1, 1).await;
    t.add_member(srv.id, u.1, 0).await;
    (srv, [o, a, m, u])
}

#[tokio::test]
async fn admin_promotes_to_moderator_not_admin() {
    let t = setup().await;
    let (srv, [_, a, _, u]) = crew(&t).await;
    assert_eq!(set_role(&t, &a.0, &srv, u.1, 2).await, StatusCode::FORBIDDEN);
    assert_eq!(set_role(&t, &a.0, &srv, u.1, 1).await, StatusCode::OK);
    let (_, v) = t.call(Method::GET, &format!("/servers/{}/members", srv.id), Some(&a.0), None).await;
    let ms: Vec<Member> = serde_json::from_value(v).unwrap();
    assert_eq!(ms.iter().find(|m| m.user.id == u.1).unwrap().role_id, role_id(&srv, 1));
}

#[tokio::test]
async fn admin_cannot_demote_admin_owner_can() {
    let t = setup().await;
    let (srv, [o, a, _, u]) = crew(&t).await;
    assert_eq!(set_role(&t, &o.0, &srv, u.1, 2).await, StatusCode::OK, "владелец назначает админа");
    assert_eq!(set_role(&t, &a.0, &srv, u.1, 0).await, StatusCode::FORBIDDEN, "админ не снимает админа");
    assert_eq!(set_role(&t, &o.0, &srv, u.1, 0).await, StatusCode::OK);
}

#[tokio::test]
async fn owner_role_not_assignable() {
    let t = setup().await;
    let (srv, [o, _, _, u]) = crew(&t).await;
    assert_eq!(set_role(&t, &o.0, &srv, u.1, 3).await, StatusCode::FORBIDDEN);
    assert_eq!(set_role(&t, &o.0, &srv, o.1, 0).await, StatusCode::FORBIDDEN, "себя/владельца не трогаем");
}

#[tokio::test]
async fn moderator_kicks_member_not_admin() {
    let t = setup().await;
    let (srv, [_, a, m, u]) = crew(&t).await;
    let kick = |uid: i64| format!("/servers/{}/members/{uid}", srv.id);
    assert_eq!(t.call(Method::DELETE, &kick(a.1), Some(&m.0), None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(t.call(Method::DELETE, &kick(m.1), Some(&u.0), None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(t.call(Method::DELETE, &kick(u.1), Some(&m.0), None).await.0, StatusCode::NO_CONTENT);
    assert!(servers_of(&t, &u.0).await.is_empty());
    // кикнутый может вернуться по приглашению
    let inv = new_invite(&t, &m.0, srv.id, 1).await;
    assert_eq!(t.call(Method::POST, &format!("/invites/{}/join", inv.code), Some(&u.0), None).await.0, StatusCode::OK);
}

#[tokio::test]
async fn kick_removes_from_voice() {
    let t = setup().await;
    let (srv, [o, _, _, u]) = crew(&t).await;
    let room = format!("srv-{}-ch-99", srv.id);
    t.set_voice(&room, &[u.1, o.1]).await;
    assert_eq!(t.call(Method::DELETE, &format!("/servers/{}/members/{}", srv.id, u.1), Some(&o.0), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(t.removed(), vec![(room, u.1.to_string())]);
}

#[tokio::test]
async fn disconnect_only_from_voice() {
    let t = setup().await;
    let (srv, [_, a, m, u]) = crew(&t).await;
    let room = format!("srv-{}-ch-7", srv.id);
    t.set_voice(&room, &[u.1, a.1]).await;
    let dc = |uid: i64| format!("/servers/{}/members/{uid}/disconnect", srv.id);
    assert_eq!(t.call(Method::POST, &dc(a.1), Some(&m.0), None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(t.call(Method::POST, &dc(u.1), Some(&m.0), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(t.removed(), vec![(room, u.1.to_string())]);
    assert_eq!(servers_of(&t, &u.0).await.len(), 1, "остался на сервере");
}

#[tokio::test]
async fn ban_removes_and_blocks_rejoin() {
    let t = setup().await;
    let (srv, [o, _, m, u]) = crew(&t).await;
    let room = format!("srv-{}-ch-5", srv.id);
    t.set_voice(&room, &[u.1]).await;
    let bans = format!("/servers/{}/bans", srv.id);
    let (s, _) = t.call(Method::POST, &bans, Some(&m.0), Some(json!({"user_id": u.1, "reason": "спам"}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(servers_of(&t, &u.0).await.is_empty());
    assert_eq!(t.removed(), vec![(room, u.1.to_string())]);
    let (s, v) = t.call(Method::GET, &bans, Some(&m.0), None).await;
    assert_eq!(s, StatusCode::OK);
    let list: Vec<proto::Ban> = serde_json::from_value(v).unwrap();
    assert_eq!((list[0].user.id, list[0].reason.as_deref()), (u.1, Some("спам")));
    let inv = new_invite(&t, &o.0, srv.id, 1).await;
    let (s, v) = t.call(Method::POST, &format!("/invites/{}/join", inv.code), Some(&u.0), None).await;
    assert_eq!((s, err(&v)), (StatusCode::FORBIDDEN, "banned".to_string()));
    // разбан — можно вернуться
    assert_eq!(t.call(Method::DELETE, &format!("{bans}/{}", u.1), Some(&m.0), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(t.call(Method::POST, &format!("/invites/{}/join", inv.code), Some(&u.0), None).await.0, StatusCode::OK);
}

#[tokio::test]
async fn member_cannot_ban() {
    let t = setup().await;
    let (srv, [_, _, m, u]) = crew(&t).await;
    let (s, _) = t.call(Method::POST, &format!("/servers/{}/bans", srv.id), Some(&u.0), Some(json!({"user_id": m.1}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}
