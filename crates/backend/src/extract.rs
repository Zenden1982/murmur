//! Экстракторы: авторизованный пользователь, IP клиента, JSON с нашими ошибками.

use std::net::{IpAddr, SocketAddr};

use axum::{
    Json,
    extract::{ConnectInfo, FromRequest, FromRequestParts, Request},
    http::{header, request::Parts},
};
use serde::de::DeserializeOwned;

use crate::{AppState, error::AppError, now, security};

/// Пользователь из `Authorization: Bearer <token>`.
pub struct AuthUser {
    pub id: i64,
    pub name: String,
    pub is_admin: bool,
    pub token_hash: String,
}

impl AuthUser {
    pub fn require_admin(&self) -> Result<(), AppError> {
        if self.is_admin {
            Ok(())
        } else {
            Err(AppError::Forbidden)
        }
    }
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or(AppError::Unauthorized)?;
        let token_hash = security::token_hash(token);
        let row: Option<(i64, String, bool)> = sqlx::query_as(
            "SELECT u.id, u.name, u.is_admin FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = ? AND s.expires_at > ?",
        )
        .bind(&token_hash)
        .bind(now())
        .fetch_optional(&state.inner.db)
        .await?;
        let (id, name, is_admin) = row.ok_or(AppError::Unauthorized)?;
        // «последняя активность» — не чаще раза в минуту на сеанс
        let t = now();
        sqlx::query("UPDATE sessions SET last_seen = ? WHERE token_hash = ? AND last_seen < ?")
            .bind(t)
            .bind(&token_hash)
            .bind(t - 60)
            .execute(&state.inner.db)
            .await?;
        Ok(AuthUser {
            id,
            name,
            is_admin,
            token_hash,
        })
    }
}

/// Адрес TCP-пира (или клиента из PROXY v2) — тип для `into_make_service_with_connect_info`.
#[derive(Clone, Copy, Debug)]
pub struct ClientAddr(pub SocketAddr);

impl
    axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, tokio::net::TcpListener>>
    for ClientAddr
{
    fn connect_info(s: axum::serve::IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        ClientAddr(*s.remote_addr())
    }
}

impl
    axum::extract::connect_info::Connected<
        axum::serve::IncomingStream<'_, crate::proxy_protocol::ProxyProtocolListener>,
    > for ClientAddr
{
    fn connect_info(
        s: axum::serve::IncomingStream<'_, crate::proxy_protocol::ProxyProtocolListener>,
    ) -> Self {
        ClientAddr(*s.remote_addr())
    }
}

/// IP клиента с учётом доверенных прокси (TRUSTED_PROXY_HOPS).
pub struct ClientIp(pub IpAddr);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<ClientAddr>>()
            .map(|c| c.0.0.ip())
            .unwrap_or(IpAddr::from([0, 0, 0, 0]));
        let xff = parts
            .headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect::<Vec<_>>()
            .join(",");
        Ok(ClientIp(client_ip(
            peer,
            &xff,
            state.inner.trusted_proxy_hops,
        )))
    }
}

/// hops = 0 → адрес пира. Иначе N-й с конца элемент X-Forwarded-For
/// (последний дописал ближайший к нам доверенный прокси). Мусор/нехватка — адрес пира.
pub fn client_ip(peer: IpAddr, xff: &str, hops: usize) -> IpAddr {
    if hops == 0 {
        return peer;
    }
    let list: Vec<&str> = xff
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    list.len()
        .checked_sub(hops)
        .and_then(|i| list.get(i))
        .and_then(|s| s.parse().ok())
        .unwrap_or(peer)
}

/// Как `Json<T>`, но ошибки разбора — в нашем формате ErrorBody.
pub struct ApiJson<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, AppError> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(ApiJson(v)),
            Err(e) => Err(AppError::BadRequest(e.body_text())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xff_parsing() {
        let peer: IpAddr = "172.17.0.1".parse().unwrap();
        assert_eq!(client_ip(peer, "1.2.3.4", 0), peer);
        assert_eq!(
            client_ip(peer, "1.2.3.4", 1),
            "1.2.3.4".parse::<IpAddr>().unwrap()
        );
        // клиент подделал начало заголовка — берём то, что дописал наш прокси
        assert_eq!(
            client_ip(peer, "6.6.6.6, 1.2.3.4", 1),
            "1.2.3.4".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            client_ip(peer, "6.6.6.6, 1.2.3.4, 10.0.0.2", 2),
            "1.2.3.4".parse::<IpAddr>().unwrap()
        );
        assert_eq!(client_ip(peer, "", 1), peer);
        assert_eq!(client_ip(peer, "garbage", 1), peer);
    }
}
