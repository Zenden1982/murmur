use std::{
    io::{BufRead, Write},
    time::Duration,
};

use anyhow::Context;
use backend::{AppState, ClientAddr, config::Config, proxy_protocol::ProxyProtocolListener};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

/// Бэкенд Amadeus.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Запустить HTTP API (по умолчанию).
    Serve(Config),
    /// Создать администратора. Пароль — из ADMIN_PASSWORD или вводится в консоли.
    CreateAdmin {
        #[arg(long)]
        name: String,
        #[arg(long, env = "DATABASE_URL", default_value = "sqlite://data/app.db")]
        database_url: String,
    },
    /// Согласованная копия базы на ходу (VACUUM INTO) — для бэкапа. Существующий файл заменяется.
    Backup {
        /// Куда положить копию, например /data/backup/app.db
        out: std::path::PathBuf,
        #[arg(long, env = "DATABASE_URL", default_value = "sqlite://data/app.db")]
        database_url: String,
    },
    /// Есть ли администратор: код выхода 0 — есть, 2 — нет, 1 — ошибка (для установщика).
    AdminExists {
        #[arg(long, env = "DATABASE_URL", default_value = "sqlite://data/app.db")]
        database_url: String,
    },
    /// Выпустить инвайт-код от имени администратора (первый админ, если --by не задан).
    CreateInvite {
        #[arg(long, default_value_t = 1)]
        uses: u32,
        /// Срок действия в часах (0 — бессрочно).
        #[arg(long, default_value_t = 168)]
        hours: u32,
        #[arg(long, env = "DATABASE_URL", default_value = "sqlite://data/app.db")]
        database_url: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info,sqlx=warn".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        None => serve(EnvOnly::parse_from(["backend"]).cfg).await,
        Some(Cmd::Serve(cfg)) => serve(cfg).await,
        Some(Cmd::CreateAdmin { name, database_url }) => {
            let pool = backend::connect(&database_url).await?;
            let password = match std::env::var("ADMIN_PASSWORD") {
                Ok(p) if !p.is_empty() => p,
                _ => read_password()?,
            };
            let user = backend::create_user(&pool, &name, &password, true)
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            println!("Администратор создан: id={} name={}", user.id, user.name);
            Ok(())
        }
        Some(Cmd::Backup { out, database_url }) => {
            let pool = backend::connect(&database_url).await?;
            backend::backup(&pool, &out).await?;
            println!("Копия базы: {}", out.display());
            Ok(())
        }
        Some(Cmd::AdminExists { database_url }) => {
            let r = async { backend::admin_exists(&backend::connect(&database_url).await?).await }.await;
            match &r {
                Ok(yes) => println!("{}", if *yes { "есть" } else { "нет" }),
                Err(e) => eprintln!("ошибка: {e:#}"),
            }
            std::process::exit(admin_exit_code(&r));
        }
        Some(Cmd::CreateInvite {
            uses,
            hours,
            database_url,
        }) => {
            let pool = backend::connect(&database_url).await?;
            let admin: Option<i64> =
                sqlx::query_scalar("SELECT id FROM users WHERE is_admin = 1 ORDER BY id LIMIT 1")
                    .fetch_optional(&pool)
                    .await?;
            let admin = admin.context("нет администратора: сначала backend create-admin")?;
            // Сервер администратора (самый ранний его); иначе самый ранний вообще — а не жёстко id=1.
            let server: Option<i64> = sqlx::query_scalar("SELECT id FROM servers WHERE owner_id = ? ORDER BY id LIMIT 1")
                .bind(admin)
                .fetch_optional(&pool)
                .await?;
            let server = match server {
                Some(s) => s,
                None => sqlx::query_scalar("SELECT id FROM servers ORDER BY id LIMIT 1")
                    .fetch_optional(&pool)
                    .await?
                    .context("нет ни одного сервера")?,
            };
            let inv = backend::create_invite(&pool, server, admin, uses, hours)
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            println!("{}", inv.code);
            Ok(())
        }
    }
}

/// Код выхода `admin-exists`: «нет» отличается от ошибки, чтобы установщик не создал второго админа при сбое.
fn admin_exit_code(r: &anyhow::Result<bool>) -> i32 {
    match r {
        Ok(true) => 0,
        Ok(false) => 2,
        Err(_) => 1,
    }
}

/// Запуск без подкоманды: `Config` целиком из окружения.
#[derive(Parser)]
struct EnvOnly {
    #[command(flatten)]
    cfg: Config,
}

fn read_password() -> anyhow::Result<String> {
    eprint!("Пароль администратора (будет виден при вводе): ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

async fn serve(cfg: Config) -> anyhow::Result<()> {
    cfg.livekit.validate()?;
    let pool = backend::connect(&cfg.database_url)
        .await
        .context("открыть базу")?;
    std::fs::create_dir_all(&cfg.uploads_dir)
        .with_context(|| format!("создать папку вложений {}", cfg.uploads_dir.display()))?;
    std::fs::create_dir_all(&cfg.releases_dir)
        .with_context(|| format!("создать папку выпусков {}", cfg.releases_dir.display()))?;
    let state = AppState::new(
        pool,
        cfg.livekit.clone(),
        cfg.trusted_proxy_hops,
        cfg.uploads_dir.clone(),
        cfg.releases_dir.clone(),
    );
    let mut state = state.with_rooms(std::sync::Arc::new(backend::voice::LiveKitRooms::new(&cfg.livekit_api_url, &cfg.livekit)));
    if let Some(up) = backend::releases::Upstream::from_config(&cfg.updates_upstream, env!("CARGO_PKG_VERSION")) {
        tracing::info!(upstream = %up.base(), "обновления клиента — с официального сервера, если своих нет");
        state = state.with_upstream(up);
    }
    backend::chat::spawn_janitor(state.clone());
    backend::voice::spawn_reconciler(state.clone());
    let app = backend::router(state.clone());
    let listener = tokio::net::TcpListener::bind(cfg.bind_addr).await?;
    tracing::info!(
        addr = %cfg.bind_addr,
        proxy_hops = cfg.trusted_proxy_hops,
        uploads = %cfg.uploads_dir.display(),
        releases = %cfg.releases_dir.display(),
        proxy_protocol = cfg.proxy_protocol,
        "listening"
    );
    let svc = app.into_make_service_with_connect_info::<ClientAddr>();
    if cfg.proxy_protocol {
        let listener = ProxyProtocolListener::new(listener, Duration::from_secs(5))?;
        axum::serve(listener, svc)
            .with_graceful_shutdown(shutdown(state))
            .await?;
    } else {
        axum::serve(listener, svc)
            .with_graceful_shutdown(shutdown(state))
            .await?;
    }
    Ok(())
}

async fn shutdown(state: AppState) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutting down");
    state.close_streams();
}

#[cfg(test)]
mod tests {
    use super::admin_exit_code;

    #[test]
    fn admin_exists_exit_codes() {
        assert_eq!(admin_exit_code(&Ok(true)), 0);
        assert_eq!(admin_exit_code(&Ok(false)), 2, "«нет» — отдельный код, не как ошибка");
        assert_eq!(admin_exit_code(&Err(anyhow::anyhow!("база занята"))), 1);
    }
}
