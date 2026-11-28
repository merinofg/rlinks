use anyhow::{Result, bail};
use rlinks::jobs::{self, Jobs};
use rlinks::scrape::Fetcher;
use rlinks::{AppState, config, domain::users, web};
use std::sync::Arc;

const USAGE: &str = "usage: rlinks [serve | migrate | create-user <username> <password> [--admin] | sql <statement>...]";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let db = config::db_from_env()?;
    db.migrate().await?;

    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] | ["serve"] => serve(db).await,
        ["migrate"] => Ok(()),
        ["create-user", name, pass, rest @ ..] => {
            if !users::create(&db, name, pass, rest == ["--admin"]).await? {
                bail!("user {name:?} already exists");
            }
            println!("created user {name}");
            Ok(())
        }
        // Each argument is one statement; all run atomically. Rows print as JSON lines.
        ["sql", sql @ ..] if !sql.is_empty() => {
            let stmts = sql.iter().map(|s| rlinks::db::Stmt { sql: s.to_string(), params: vec![] }).collect();
            for row in db.batch(stmts).await?.into_iter().flatten() {
                println!("{}", serde_json::Value::Object(row));
            }
            Ok(())
        }
        _ => bail!(USAGE),
    }
}

/// Snapshots still pending (e.g. the server stopped mid-capture) are captured again.
async fn requeue_snapshots(ctx: &jobs::Context, jobs: &Jobs) -> Result<usize> {
    let pending = rlinks::domain::assets::pending_snapshots(&ctx.db).await?;
    for id in &pending {
        jobs.enqueue_snapshot(*id);
    }
    Ok(pending.len())
}

async fn serve(db: rlinks::db::Db) -> Result<()> {
    let cfg = config::Config::from_env()?;
    if let Some((name, pass)) = &cfg.superuser
        && users::create(&db, name, pass, true).await?
    {
        tracing::info!(name, "created superuser");
    }
    let ctx = jobs::Context {
        db: Arc::new(db),
        storage: config::storage_from_env()?,
        secret: cfg.secret_key.as_str().into(),
        fetcher: Arc::new(Fetcher::new(&cfg.allowed_internal_hosts, &cfg.favicon_provider)?),
    };
    let jobs = Jobs::start(ctx.clone());
    let pending = jobs.enqueue_missing(&ctx.db, None).await?;
    if pending > 0 {
        tracing::info!(pending, "re-queued bookmarks missing favicons/previews");
    }
    let snapshots = requeue_snapshots(&ctx, &jobs).await?;
    if snapshots > 0 {
        tracing::info!(snapshots, "re-queued unfinished snapshots");
    }
    let app = web::router(AppState::new(ctx, jobs, &cfg.secret_key));
    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!("listening on http://{}", cfg.bind);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await?;
    Ok(())
}
