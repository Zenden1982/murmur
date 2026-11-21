use std::str::FromStr;

use proto::{Channel, ChannelKind, Invite, User, limits};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

use crate::{
    error::{AppError, AppResult},
    now, security,
};

/// Подключение к SQLite (файл создаётся при необходимости) + миграции.
pub async fn connect(database_url: &str) -> anyhow::Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5));
    if let Some(dir) = opts.get_filename().parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;
    migrate(&pool).await?;
    Ok(pool)
}

/// Согласованный снимок базы в файл `out` (VACUUM INTO; можно на работающем сервисе).
/// Пишем во временный файл рядом и переименовываем — частично записанная копия не остаётся под итоговым именем.
pub async fn backup(pool: &SqlitePool, out: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;
    if let Some(dir) = out.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).context("создать папку для копии")?;
    }
    let mut tmp = out.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    let tmp_str = tmp.to_str().context("путь копии — не UTF-8")?;
    sqlx::query("VACUUM INTO ?")
        .bind(tmp_str)
        .execute(pool)
        .await
        .context("VACUUM INTO")?;
    std::fs::rename(&tmp, out).context("переименовать копию")?;
    Ok(())
}

pub(crate) async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

pub(crate) fn validate_name(name: &str) -> AppResult<String> {
    let name = name.trim().to_string();
    let n = name.chars().count();
    if !(limits::NAME_MIN..=limits::NAME_MAX).contains(&n) {
        return Err(AppError::Validation(format!(
            "имя: от {} до {} символов",
            limits::NAME_MIN,
            limits::NAME_MAX
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ' '))
    {
        return Err(AppError::Validation(
            "имя: только буквы, цифры, пробел и _ - .".into(),
        ));
    }
    if name.contains("  ") {
        return Err(AppError::Validation("имя: без двойных пробелов".into()));
    }
    Ok(name)
}

pub(crate) fn validate_password(password: &str) -> AppResult<()> {
    let n = password.chars().count();
    if !(limits::PASSWORD_MIN..=limits::PASSWORD_MAX).contains(&n) {
        return Err(AppError::Validation(format!(
            "пароль: от {} до {} символов",
            limits::PASSWORD_MIN,
            limits::PASSWORD_MAX
        )));
    }
    Ok(())
}

pub(crate) async fn hash_password_async(password: String) -> AppResult<String> {
    Ok(tokio::task::spawn_blocking(move || security::hash_password(&password)).await??)
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.is_unique_violation())
}

/// Создать пользователя напрямую (CLI: `backend create-admin`).
pub async fn create_user(
    pool: &SqlitePool,
    name: &str,
    password: &str,
    is_admin: bool,
) -> AppResult<User> {
    let name = validate_name(name)?;
    validate_password(password)?;
    let hash = hash_password_async(password.to_string()).await?;
    let mut tx = pool.begin().await?;
    let id = insert_user(&mut tx, &name, &hash, is_admin).await?;
    // Новый инстанс: первый админ получает первый сервер (как сервер №1 после миграции).
    let servers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM servers").fetch_one(&mut *tx).await?;
    if is_admin && servers == 0 {
        crate::servers::insert_server(&mut tx, "Amadeus", id, false).await?;
    }
    tx.commit().await?;
    Ok(User { id, name, is_admin, display_name: None, avatar: None, dm_from_servers: false, about: None, status: None })
}

pub(crate) async fn insert_user(
    conn: &mut sqlx::SqliteConnection,
    name: &str,
    hash: &str,
    is_admin: bool,
) -> AppResult<i64> {
    let res = sqlx::query(
        "INSERT INTO users (name, password_hash, is_admin, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(name)
    .bind(hash)
    .bind(is_admin)
    .bind(now())
    .execute(&mut *conn)
    .await;
    match res {
        Ok(r) => Ok(r.last_insert_rowid()),
        Err(e) if is_unique_violation(&e) => Err(AppError::NameTaken),
        Err(e) => Err(e.into()),
    }
}

/// Создать инвайт. `expires_in_hours = 0` — бессрочно.
pub async fn create_invite(
    pool: &SqlitePool,
    server_id: i64,
    created_by: i64,
    uses: u32,
    expires_in_hours: u32,
) -> AppResult<Invite> {
    if !(1..=limits::INVITE_USES_MAX).contains(&uses) {
        return Err(AppError::Validation(format!(
            "uses: от 1 до {}",
            limits::INVITE_USES_MAX
        )));
    }
    if expires_in_hours > 24 * 365 {
        return Err(AppError::Validation(
            "expires_in_hours: не больше года".into(),
        ));
    }
    let expires_at = (expires_in_hours > 0).then(|| now() + i64::from(expires_in_hours) * 3600);
    let code = security::new_invite_code();
    sqlx::query("INSERT INTO invites (code, created_by, uses_left, expires_at, created_at, server_id) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(&code)
        .bind(created_by)
        .bind(uses)
        .bind(expires_at)
        .bind(now())
        .bind(server_id)
        .execute(pool)
        .await?;
    Ok(Invite {
        server_id,
        code,
        uses_left: uses,
        expires_at,
        revoked: false,
        created_by,
    })
}

#[derive(sqlx::FromRow)]
pub(crate) struct InviteRow {
    code: String,
    uses_left: i64,
    expires_at: Option<i64>,
    revoked: bool,
    created_by: i64,
    server_id: Option<i64>,
}

impl From<InviteRow> for Invite {
    fn from(r: InviteRow) -> Self {
        Invite {
            server_id: r.server_id.unwrap_or(0),
            code: r.code,
            uses_left: u32::try_from(r.uses_left).unwrap_or(0),
            expires_at: r.expires_at,
            revoked: r.revoked,
            created_by: r.created_by,
        }
    }
}

/// Столбцы для `ChannelRow` (`WHERE id = ?`).
pub(crate) const CHANNEL_SELECT_BY_ID: &str =
    "SELECT id, name, kind, server_id, category_id, position, user_limit FROM channels WHERE id = ?";

#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct ChannelRow {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub server_id: Option<i64>,
    pub category_id: Option<i64>,
    pub position: i64,
    pub user_limit: Option<i64>,
}

impl TryFrom<ChannelRow> for Channel {
    type Error = AppError;
    fn try_from(r: ChannelRow) -> AppResult<Self> {
        let kind = ChannelKind::parse(&r.kind).ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!("bad channel kind in db: {}", r.kind))
        })?;
        Ok(Channel {
            server_id: r.server_id.unwrap_or(0),
            category_id: r.category_id,
            position: r.position as i32,
            user_limit: r.user_limit.map(|n| n as u32),
            id: r.id,
            name: r.name,
            kind,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(validate_name("  Вася_99 ").unwrap(), "Вася_99");
        assert!(validate_name("a").is_err());
        assert!(validate_name("bad<name>").is_err());
        assert!(validate_name("a  b").is_err());
        assert!(validate_name(&"x".repeat(33)).is_err());
    }

    #[test]
    fn passwords() {
        assert!(validate_password("1234567").is_err());
        assert!(validate_password("12345678").is_ok());
    }
}
