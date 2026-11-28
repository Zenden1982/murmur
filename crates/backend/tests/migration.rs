//! Миграция 0003: нынешние данные переезжают на сервер №1.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

async fn pool() -> sqlx::SqlitePool {
    let opts: SqliteConnectOptions = "sqlite::memory:".parse().unwrap();
    SqlitePoolOptions::new().max_connections(1).connect_with(opts.foreign_keys(true)).await.unwrap()
}

#[tokio::test]
async fn migration_moves_existing_data() {
    let pool = pool().await;
    let m = sqlx::migrate!("./migrations");
    m.run_to(2, &pool).await.unwrap();
    for (id, name, admin) in [(1, "okabe", 1), (2, "kurisu", 0), (3, "daru", 0)] {
        sqlx::query("INSERT INTO users (id, name, password_hash, is_admin, created_at) VALUES (?, ?, 'x', ?, 0)")
            .bind(id).bind(name).bind(admin).execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO channels (id, name, kind, created_at) VALUES (5, 'General', 'voice', 0), (9, 'general', 'text', 0)")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO invites (code, created_by, uses_left, created_at) VALUES ('ABC', 1, 1, 0)").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO messages (channel_id, author_id, content, created_at) VALUES (9, 2, 'hi', 0)").execute(&pool).await.unwrap();

    m.run(&pool).await.unwrap();

    let (sid, owner): (i64, i64) = sqlx::query_as("SELECT id, owner_id FROM servers").fetch_one(&pool).await.unwrap();
    assert_eq!((sid, owner), (1, 1));
    let roles: Vec<(String, i64, i64)> =
        sqlx::query_as("SELECT name, position, permissions FROM server_roles WHERE server_id = 1 ORDER BY position DESC")
            .fetch_all(&pool).await.unwrap();
    assert_eq!(roles.iter().map(|r| r.1).collect::<Vec<_>>(), vec![3, 2, 1, 0]);
    // 0010: модератору (и выше) — MANAGE_MESSAGES (128)
    assert_eq!((roles[2].2, roles[3].2), (15 | 128, 1));
    assert_eq!(roles[0].2 & 128, 128);
    let members: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT m.user_id, r.position FROM server_members m JOIN server_roles r ON r.id = m.role_id WHERE m.server_id = 1 ORDER BY m.user_id",
    )
    .fetch_all(&pool).await.unwrap();
    assert_eq!(members, vec![(1, 3), (2, 0), (3, 0)]);
    let chans: Vec<(i64, i64, i64)> =
        sqlx::query_as("SELECT id, server_id, position FROM channels ORDER BY id").fetch_all(&pool).await.unwrap();
    assert_eq!(chans, vec![(5, 1, 0), (9, 1, 1)]);
    let inv: i64 = sqlx::query_scalar("SELECT server_id FROM invites WHERE code = 'ABC'").fetch_one(&pool).await.unwrap();
    assert_eq!(inv, 1);
    let msgs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages").fetch_one(&pool).await.unwrap();
    assert_eq!(msgs, 1);
}

#[tokio::test]
async fn migration_admins_become_server_admins() {
    let pool = pool().await;
    let m = sqlx::migrate!("./migrations");
    m.run_to(2, &pool).await.unwrap();
    sqlx::query("INSERT INTO users (id, name, password_hash, is_admin, created_at) VALUES (1, 'a', 'x', 0, 0), (2, 'b', 'x', 1, 0), (3, 'c', 'x', 1, 0)")
        .execute(&pool).await.unwrap();
    m.run(&pool).await.unwrap();
    let owner: i64 = sqlx::query_scalar("SELECT owner_id FROM servers WHERE id = 1").fetch_one(&pool).await.unwrap();
    assert_eq!(owner, 2, "владелец — первый админ инстанса");
    let pos: Vec<i64> = sqlx::query_scalar(
        "SELECT r.position FROM server_members m JOIN server_roles r ON r.id = m.role_id ORDER BY m.user_id",
    )
    .fetch_all(&pool).await.unwrap();
    assert_eq!(pos, vec![0, 3, 2]);
}

#[tokio::test]
async fn migration_on_empty_db() {
    let pool = pool().await;
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM servers").fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0);
}
