//! GIF без ключа Klipy: запросы отвечают понятной ошибкой, ничего наружу не уходит.
mod common;

use axum::http::{Method, StatusCode};
use common::*;

#[tokio::test]
async fn gifs_disabled_without_key() {
    let t = setup().await;
    let (tok, _) = t.user("neo").await;
    for (m, uri, body) in [
        (Method::GET, "/gifs/search?q=cat", None),
        (Method::GET, "/gifs/media?u=https://static.klipy.com/a.jpg", None),
    ] {
        let (s, v) = t.call(m, uri, Some(&tok), body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{uri}: {v}");
        assert!(err(&v).contains("bad_request"), "{v}");
    }
    // без входа — 401
    assert_eq!(t.call(Method::GET, "/gifs/search", None, None).await.0, StatusCode::UNAUTHORIZED);
}
