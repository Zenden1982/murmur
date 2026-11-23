//! Аккаунт: сеансы, смена пароля, удаление.
mod common;

use axum::http::{Method, StatusCode};
use common::*;
use serde_json::json;

#[tokio::test]
async fn sessions_list_and_revoke() {
    let t = setup().await;
    let (a, _) = t.user("neo").await;
    let b = t.login("neo", "password123").await;
    let (s, v) = t.call(Method::GET, "/auth/sessions", Some(&a), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let list: Vec<proto::SessionInfo> = serde_json::from_value(v).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list.iter().filter(|x| x.current).count(), 1);
    let other = list.iter().find(|x| !x.current).unwrap().id.clone();
    // текущий завершить нельзя
    let cur = list.iter().find(|x| x.current).unwrap().id.clone();
    assert_eq!(t.call(Method::DELETE, &format!("/auth/sessions/{cur}"), Some(&a), None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.call(Method::DELETE, &format!("/auth/sessions/{other}"), Some(&a), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(t.call(Method::GET, "/me", Some(&b), None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(t.call(Method::GET, "/me", Some(&a), None).await.0, StatusCode::OK);
    assert_eq!(t.call(Method::DELETE, &format!("/auth/sessions/{other}"), Some(&a), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(t.call(Method::DELETE, "/auth/sessions/zzz", Some(&a), None).await.0, StatusCode::NOT_FOUND);
    // чужой сеанс завершить нельзя
    let (c, _) = t.user("trinity").await;
    assert_eq!(t.call(Method::DELETE, &format!("/auth/sessions/{cur}"), Some(&c), None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn change_password() {
    let t = setup().await;
    let (a, _) = t.user("neo").await;
    let b = t.login("neo", "password123").await;
    let bad = t.call(Method::POST, "/auth/password", Some(&a), Some(json!({"old_password": "nope-nope", "new_password": "new-password-1"}))).await;
    assert_eq!(bad.0, StatusCode::UNAUTHORIZED);
    let short = t.call(Method::POST, "/auth/password", Some(&a), Some(json!({"old_password": "password123", "new_password": "x"}))).await;
    assert_eq!(short.0, StatusCode::UNPROCESSABLE_ENTITY);
    let ok = t.call(Method::POST, "/auth/password", Some(&a), Some(json!({"old_password": "password123", "new_password": "new-password-1"}))).await;
    assert_eq!(ok.0, StatusCode::NO_CONTENT);
    // этот сеанс жив, остальные закрыты
    assert_eq!(t.call(Method::GET, "/me", Some(&a), None).await.0, StatusCode::OK);
    assert_eq!(t.call(Method::GET, "/me", Some(&b), None).await.0, StatusCode::UNAUTHORIZED);
    t.login("neo", "new-password-1").await;
    let old = t.call(Method::POST, "/auth/login", None, Some(json!({"name": "neo", "password": "password123"}))).await;
    assert_eq!(old.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn delete_account_keeps_messages() {
    let t = setup().await;
    let admin = t.admin().await;
    let (neo, neo_id) = t.user("neo").await;
    let (trin, trin_id) = t.user("trinity").await;
    let srv = t.create_server(&admin, "S").await;
    t.add_member(srv.id, neo_id, 1).await;
    t.add_member(srv.id, trin_id, 1).await;
    let (s, v) = t.call(Method::POST, &format!("/servers/{}/channels", srv.id), Some(&admin), Some(json!({"name": "текст", "kind": "text"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let ch = v["id"].as_i64().unwrap();
    let (s, _) = t.call(Method::POST, &format!("/channels/{ch}/messages"), Some(&neo), Some(json!({"content": "привет"}))).await;
    assert_eq!(s, StatusCode::CREATED);

    // неверный пароль
    assert_eq!(t.call(Method::POST, "/auth/delete", Some(&neo), Some(json!({"password": "wrong-pass"}))).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(t.call(Method::POST, "/auth/delete", Some(&neo), Some(json!({"password": "password123"}))).await.0, StatusCode::NO_CONTENT);
    assert_eq!(t.call(Method::GET, "/me", Some(&neo), None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(t.call(Method::POST, "/auth/login", None, Some(json!({"name": "neo", "password": "password123"}))).await.0, StatusCode::UNAUTHORIZED);

    // сообщение осталось, автор — «Удалённый пользователь»
    let (s, v) = t.call(Method::GET, &format!("/channels/{ch}/messages"), Some(&trin), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let m = &v.as_array().unwrap()[0];
    assert_eq!(m["content"], "привет");
    assert_eq!(m["author"]["name"], "Удалённый пользователь");
    // имя освободилось, а войти под «удалённым» нельзя
    assert_eq!(t.call(Method::POST, "/auth/login", None, Some(json!({"name": "Удалённый пользователь", "password": "!"}))).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn owner_and_last_admin_cannot_delete() {
    let t = setup().await;
    let admin = t.admin().await;
    // админ — владелец сервера №1 и единственный админ
    let (s, v) = t.call(Method::POST, "/auth/delete", Some(&admin), Some(json!({"password": "admin-password"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
}
