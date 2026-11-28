//! Права на сервере: кто ты на этом сервере и что тебе можно (proto::perm, иерархия ролей).

use proto::perm;
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};

/// Участник сервера с его ролью.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberCtx {
    pub server_id: i64,
    pub user_id: i64,
    pub role_id: i64,
    pub position: i32,
    pub perms: u32,
    pub is_owner: bool,
}

impl MemberCtx {
    pub fn require(&self, p: u32) -> AppResult<()> {
        if self.is_owner || perm::has(self.perms, p) { Ok(()) } else { Err(AppError::Forbidden) }
    }

    pub fn require_owner(&self) -> AppResult<()> {
        if self.is_owner { Ok(()) } else { Err(AppError::Forbidden) }
    }

    /// Действовать над человеком (кик, бан, роль, выкинуть из голоса) — только если его роль строго ниже.
    pub fn can_act_on(&self, target: &MemberCtx) -> bool {
        !target.is_owner && (self.is_owner || target.position < self.position)
    }

    /// Назначить роль можно только строго ниже своей; «Владелец» (позиция 3) — никогда.
    pub fn can_assign(&self, role_position: i32) -> bool {
        role_position < OWNER_POSITION && role_position < self.position
    }
}

pub const OWNER_POSITION: i32 = 3;

/// Кто ты на сервере. Не участник — NotFound (не раскрываем, что сервер есть).
pub async fn member(db: &SqlitePool, server_id: i64, user_id: i64) -> AppResult<MemberCtx> {
    let row: Option<(i64, i32, i64, i64)> = sqlx::query_as(
        "SELECT m.role_id, r.position, r.permissions, s.owner_id
         FROM server_members m
         JOIN server_roles r ON r.id = m.role_id
         JOIN servers s ON s.id = m.server_id
         WHERE m.server_id = ? AND m.user_id = ?",
    )
    .bind(server_id)
    .bind(user_id)
    .fetch_optional(db)
    .await?;
    let (role_id, position, perms, owner_id) = row.ok_or(AppError::NotFound)?;
    Ok(MemberCtx { server_id, user_id, role_id, position, perms: perms as u32, is_owner: owner_id == user_id })
}

/// Канал и твоё место на его сервере. Канал чужого сервера — NotFound.
pub async fn channel_member(db: &SqlitePool, channel_id: i64, user_id: i64) -> AppResult<(MemberCtx, crate::db::ChannelRow)> {
    let ch: Option<crate::db::ChannelRow> = sqlx::query_as(crate::db::CHANNEL_SELECT_BY_ID)
        .bind(channel_id)
        .fetch_optional(db)
        .await?;
    let ch = ch.ok_or(AppError::NotFound)?;
    let server_id = ch.server_id.ok_or(AppError::NotFound)?;
    let m = member(db, server_id, user_id).await?;
    Ok((m, ch))
}

pub async fn is_banned(db: &SqlitePool, server_id: i64, user_id: i64) -> AppResult<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM server_bans WHERE server_id = ? AND user_id = ?")
        .bind(server_id)
        .bind(user_id)
        .fetch_one(db)
        .await?;
    Ok(n > 0)
}

/// id стандартных ролей сервера.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct DefaultRoles {
    pub owner: i64,
    pub admin: i64,
    pub moderator: i64,
    pub member: i64,
}

/// Стандартные роли (PLAN: владелец/админ/модератор/участник).
pub const STANDARD_ROLES: [(&str, i32, u32, bool); 4] = [
    ("Владелец", 3, perm::ALL, false),
    (
        "Админ",
        2,
        perm::INVITE | perm::KICK | perm::BAN | perm::DISCONNECT | perm::MANAGE_CHANNELS | perm::MANAGE_ROLES | perm::MANAGE_SERVER | perm::MANAGE_MESSAGES,
        false,
    ),
    ("Модератор", 1, perm::INVITE | perm::KICK | perm::BAN | perm::DISCONNECT | perm::MANAGE_MESSAGES, false),
    ("Участник", 0, perm::INVITE, true),
];

pub async fn create_default_roles(tx: &mut sqlx::SqliteConnection, server_id: i64) -> AppResult<DefaultRoles> {
    let mut ids = [0i64; 4];
    for (i, (name, position, perms, is_default)) in STANDARD_ROLES.iter().enumerate() {
        ids[i] = sqlx::query(
            "INSERT INTO server_roles (server_id, name, position, permissions, is_default) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(server_id)
        .bind(name)
        .bind(position)
        .bind(*perms as i64)
        .bind(is_default)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
    }
    Ok(DefaultRoles { owner: ids[0], admin: ids[1], moderator: ids[2], member: ids[3] })
}

/// Роль «по умолчанию» (для вступающих).
pub async fn default_role(db: impl sqlx::SqliteExecutor<'_>, server_id: i64) -> AppResult<i64> {
    let id: Option<i64> = sqlx::query_scalar("SELECT id FROM server_roles WHERE server_id = ? AND is_default = 1 LIMIT 1")
        .bind(server_id)
        .fetch_optional(db)
        .await?;
    id.ok_or_else(|| AppError::Internal(anyhow::anyhow!("server {server_id} has no default role")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(position: i32, is_owner: bool) -> MemberCtx {
        let perms = match position {
            3 | 2 => perm::ALL,
            1 => perm::INVITE | perm::KICK | perm::BAN | perm::DISCONNECT,
            _ => perm::INVITE,
        };
        MemberCtx { server_id: 1, user_id: position as i64 + 10, role_id: position as i64, position, perms, is_owner }
    }

    #[test]
    fn hierarchy() {
        let (owner, admin, moder, member) = (m(3, true), m(2, false), m(1, false), m(0, false));
        assert!(owner.can_act_on(&admin));
        assert!(!admin.can_act_on(&admin));
        assert!(!admin.can_act_on(&owner));
        assert!(admin.can_act_on(&moder));
        assert!(moder.can_act_on(&member));
        assert!(!member.can_act_on(&member));
        assert!(!moder.can_act_on(&owner));
        // назначать можно только роль строго ниже своей
        assert!(admin.can_assign(1));
        assert!(!admin.can_assign(2));
        assert!(owner.can_assign(2));
        assert!(!owner.can_assign(3), "владелец не назначается — только передача");
    }

    #[test]
    fn require() {
        assert!(m(0, false).require(perm::INVITE).is_ok());
        assert!(matches!(m(0, false).require(perm::KICK), Err(AppError::Forbidden)));
        assert!(m(3, true).require_owner().is_ok());
        assert!(matches!(m(2, false).require_owner(), Err(AppError::Forbidden)));
    }
}
