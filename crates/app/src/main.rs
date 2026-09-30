use adapters::{
    bot::BotUi,
    crypto::{ArgonHasher, CryptoAdapter},
    journal::FileJournal,
    postgres::PgDatabase,
    settings::{Settings, StorageCredentials, keyring, read_env_secret},
    storage::S3Storage,
    telegram::Telegram,
};
use application::{Crypto, Engine, Error, FileObject, Kind, Plan, Result, get, list, put};
use clap::{Parser, Subcommand};
use domain::{CaseState, DAY, Id};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::task::JoinSet;
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(
    name = "zapovit",
    version,
    about = "Telegram digital inheritance service"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Serve,
    Migrate,
    CheckConfig,
    VerifyJournal {
        #[arg(long)]
        directory: PathBuf,
    },
    Maintenance {
        #[arg(long)]
        enabled: bool,
    },
    ExportObjects {
        #[arg(long)]
        directory: PathBuf,
    },
    ImportObjects {
        #[arg(long)]
        directory: PathBuf,
    },
    GenerateEnv {
        #[arg(long, default_value = ".env")]
        output: PathBuf,
    },
    InitJournal {
        #[arg(long)]
        directory: PathBuf,
    },
    StorageInit {
        #[arg(long, default_value = "http://object-storage:3903")]
        endpoint: String,
        #[arg(long, default_value = "zapovit")]
        bucket: String,
        #[arg(long, default_value_t = 10_000_000_000)]
        capacity: u64,
    },
    Healthcheck {
        #[arg(long, default_value = "http://127.0.0.1:8080/ready")]
        url: String,
    },
}
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("zapovit=info,adapters=info")
        .json()
        .with_target(false)
        .init();
    if let Err(error) = run(Cli::parse()).await {
        tracing::error!(event="command_failed",code=%error);
        std::process::exit(1);
    }
}
async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::VerifyJournal { directory } => {
            let s = Settings::load()?;
            let crypto: Arc<dyn Crypto> = Arc::new(CryptoAdapter::new(
                keyring(&s.journal_keyring)?,
                keyring(&s.journal_keyring)?,
            ));
            FileJournal::open(directory, crypto)?;
            Ok(())
        }
        Command::Maintenance { enabled } => {
            adapters::backup::maintenance(&Settings::load()?, enabled).await
        }
        Command::ExportObjects { directory } => {
            adapters::backup::export_objects(&Settings::load()?, &directory).await
        }
        Command::ImportObjects { directory } => {
            adapters::backup::import_objects(&Settings::load()?, &directory).await
        }
        Command::GenerateEnv { output } => {
            adapters::settings::generate_env(&output)?;
            println!(
                "Environment file created. Set TELEGRAM_BOT_TOKEN to a separate test bot token."
            );
            Ok(())
        }
        Command::InitJournal { directory } => FileJournal::initialize(&directory),
        Command::StorageInit {
            endpoint,
            bucket,
            capacity,
        } => {
            let token = read_env_secret("GARAGE_ADMIN_TOKEN")?;
            let credentials = StorageCredentials {
                access_key_id: read_env_secret("S3_ACCESS_KEY_ID")?.to_string(),
                secret_access_key: read_env_secret("S3_SECRET_ACCESS_KEY")?.to_string(),
            };
            adapters::bootstrap::storage_init(&endpoint, &token, &bucket, &credentials, capacity)
                .await
        }
        Command::Healthcheck { url } => {
            let r = reqwest::Client::new()
                .get(url)
                .timeout(Duration::from_secs(3))
                .send()
                .await
                .map_err(|_| Error::Storage)?;
            if r.status().is_success() {
                Ok(())
            } else {
                Err(Error::Storage)
            }
        }
        Command::CheckConfig => {
            let s = Settings::load()?;
            s.admit_runtime()?;
            adapters::localization::validate()?;
            println!("Configuration valid for synthetic data. Production admission is blocked.");
            Ok(())
        }
        Command::Migrate => {
            let s = Settings::load()?;
            let db = PgDatabase::connect(&s.database_url, s.database_pool).await?;
            db.migrate().await
        }
        Command::Serve => serve(startup("settings", Settings::load())?).await,
    }
}
fn startup<T>(stage: &'static str, result: Result<T>) -> Result<T> {
    result.inspect_err(|error| {
        // Library error text may contain URLs or credentials. Log only the
        // static stage and the application's public error code.
        tracing::error!(event = "startup_failed", stage, code = %error);
    })
}
async fn serve(settings: Settings) -> Result<()> {
    startup("configuration", settings.admit_runtime())?;
    startup("localization", adapters::localization::validate())?;
    let crypto: Arc<dyn Crypto> = Arc::new(CryptoAdapter::new(
        keyring(&settings.kek_keyring)?,
        keyring(&settings.verifier_keyring)?,
    ));
    let journal_crypto: Arc<dyn Crypto> = Arc::new(CryptoAdapter::new(
        keyring(&settings.journal_keyring)?,
        keyring(&settings.journal_keyring)?,
    ));
    let credentials = settings.storage_credentials();
    let blobs = Arc::new(startup(
        "object_storage_configuration",
        S3Storage::new(
            &settings.s3_endpoint,
            &settings.s3_region,
            &settings.s3_bucket,
            &credentials.access_key_id,
            &credentials.secret_access_key,
        ),
    )?);
    let telegram = startup(
        "telegram_configuration",
        Telegram::new(&settings.telegram_api_base, settings.telegram_bot_token),
    )?;
    let db = Arc::new(startup(
        "database_connect",
        PgDatabase::connect(&settings.database_url, settings.database_pool).await,
    )?);
    startup("database_schema", db.schema_current().await)?;
    let leader = startup("database_leadership", db.poller_lock().await)?;
    let allow_initialize = startup("journal_bootstrap", db.journal_bootstrap_allowed().await)?;
    let journal = Arc::new(startup(
        "journal_open",
        FileJournal::open_or_initialize(settings.journal_dir, journal_crypto, allow_initialize),
    )?);
    let engine = Engine {
        db: db.clone(),
        crypto: crypto.clone(),
        blobs,
        recovery: Arc::new(ArgonHasher::new(65536, 3, 1, 2)?),
        journal,
    };
    startup("control_replay", engine.replay_controls().await)?;
    let (bot_id, username) = startup("telegram_get_me", telegram.me().await)?;
    startup("telegram_bot_binding", db.bind_bot(bot_id).await)?;
    let ui = BotUi {
        engine: engine.clone(),
        telegram: telegram.clone(),
        username,
    };
    let ready = Arc::new(AtomicBool::new(false));
    let stopping = Arc::new(AtomicBool::new(false));
    let mut tasks = JoinSet::<Result<()>>::new();
    let health_ready = ready.clone();
    let metrics_db = db.clone();
    let router = axum::Router::new()
        .route("/live", axum::routing::get(|| async { "ok" }))
        .route(
            "/ready",
            axum::routing::get(move || {
                let ready = health_ready.clone();
                async move {
                    if ready.load(Ordering::Acquire) {
                        axum::http::StatusCode::OK
                    } else {
                        axum::http::StatusCode::SERVICE_UNAVAILABLE
                    }
                }
            }),
        );
    let router = router.route(
        "/metrics",
        axum::routing::get(move || {
            let db = metrics_db.clone();
            async move {
                match db.metrics().await {
                    Ok(text) => (axum::http::StatusCode::OK, text),
                    Err(_) => (axum::http::StatusCode::SERVICE_UNAVAILABLE, String::new()),
                }
            }
        }),
    );
    let listener = startup(
        "health_listener",
        tokio::net::TcpListener::bind(&settings.health_bind)
            .await
            .map_err(|_| Error::Config),
    )?;
    tasks.spawn(async move {
        axum::serve(listener, router)
            .await
            .map_err(|_| Error::Storage)
    });
    let gap = db.scheduler_heartbeat().await?;
    if gap > 120 {
        hold_after_outage(&engine).await?;
    }
    if gap > DAY {
        invalidate_old_cases(&engine).await?;
    }
    let poll_db = db.clone();
    let poll_stop = stopping.clone();
    let leader_stop = stopping.clone();
    tasks.spawn(supervise_leader(leader, leader_stop));
    tasks.spawn(async move {
        while !poll_stop.load(Ordering::Acquire) {
            let offset = poll_db.poll_offset().await?;
            match telegram.poll(offset).await {
                Ok(updates) => {
                    let mut encrypted = Vec::new();
                    for update in updates {
                        let id = update["update_id"].as_i64().ok_or(Error::InvalidInput)?;
                        let context = Id::from_u128(((bot_id as u128) << 64) | id as u64 as u128);
                        let bytes = Zeroizing::new(
                            serde_json::to_vec(&update).map_err(|_| Error::Internal)?,
                        );
                        let command = update["message"]["text"]
                            .as_str()
                            .unwrap_or("")
                            .split_whitespace()
                            .next()
                            .unwrap_or("")
                            .split('@')
                            .next()
                            .unwrap_or("");
                        let mut priority = false;
                        if matches!(command, "/stop" | "/checkin")
                            && let Some(actor) = update["message"]["from"]["id"].as_i64()
                        {
                            priority = poll_db.priority_owner(actor).await?;
                        }
                        if let Some(id) = update["callback_query"]["data"]
                            .as_str()
                            .and_then(|id| Id::parse_str(id).ok())
                            && let Some(actor) = update["callback_query"]["from"]["id"].as_i64()
                        {
                            priority |= poll_db.priority_callback(id, actor).await?;
                        }
                        encrypted.push((
                            id,
                            crypto.wrap("telegram-inbox", context, &bytes)?,
                            priority,
                        ));
                    }
                    poll_db.ingest_prioritized(bot_id, &encrypted).await?;
                }
                Err(_) => {
                    tracing::warn!(event = "telegram_poll_failed");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
        }
        Ok(())
    });
    for priority in [true, false] {
        let inbox_db = db.clone();
        let inbox_ui = ui.clone();
        let inbox_stop = stopping.clone();
        tasks.spawn(async move {
            while !inbox_stop.load(Ordering::Acquire) {
                let updates = inbox_db
                    .pending_updates_class(bot_id, Some(priority))
                    .await?;
                for (id, envelope) in updates {
                    let context = Id::from_u128(((bot_id as u128) << 64) | id as u64 as u128);
                    let bytes =
                        inbox_ui
                            .engine
                            .crypto
                            .unwrap("telegram-inbox", context, &envelope)?;
                    let update = serde_json::from_slice(&bytes).map_err(|_| Error::Crypto)?;
                    match inbox_ui.handle(bot_id, &update).await {
                        Ok(()) => inbox_db.complete_update(bot_id, id).await?,
                        Err(e) => {
                            tracing::warn!(event="inbox_processing_failed",code=%e);
                            break;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            Ok(())
        });
    }
    let scheduler_db = db.clone();
    let scheduler_engine = engine.clone();
    let scheduler_stop = stopping.clone();
    let scheduler_ready = ready.clone();
    tasks.spawn(async move {
        while !scheduler_stop.load(Ordering::Acquire) {
            let result = schedule(&scheduler_db, &scheduler_engine).await;
            scheduler_ready.store(
                result.is_ok()
                    && scheduler_db.healthy_polling().await.unwrap_or(false)
                    && !scheduler_db.has_backlog().await.unwrap_or(true),
                Ordering::Release,
            );
            if let Err(e) = result {
                tracing::warn!(event="scheduler_failed",code=%e);
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        Ok(())
    });
    for _ in 0..settings.workers {
        let worker_db = db.clone();
        let worker_ui = ui.clone();
        let worker_stop = stopping.clone();
        tasks.spawn(async move {
            while !worker_stop.load(Ordering::Acquire) {
                for id in worker_db.ready_jobs().await? {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    if let Some(job) = worker_ui.engine.claim_job(id).await?
                        && let Err(error) = worker_ui.process_job(job).await
                    {
                        tracing::warn!(event="job_processing_failed",code=%error);
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Ok(())
        });
    }
    tracing::info!(
        event = "started",
        mode = "synthetic",
        workers = settings.workers
    );
    tokio::select! {
        _=shutdown_signal()=>{},
        result=tasks.join_next()=>{stopping.store(true,Ordering::Release);ready.store(false,Ordering::Release);tracing::error!(event="runtime_task_stopped");if let Some(Ok(Err(error)))=result{return Err(error);}return Err(Error::Internal);},
    }
    stopping.store(true, Ordering::Release);
    ready.store(false, Ordering::Release);
    let _ = tokio::time::timeout(Duration::from_secs(65), async {
        while tasks.len() > 1 {
            tasks.join_next().await;
        }
    })
    .await;
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    db.pool.close().await;
    Ok(())
}
async fn schedule(db: &PgDatabase, engine: &Engine) -> Result<()> {
    let gap = db.scheduler_heartbeat().await?;
    if gap > 120 {
        hold_after_outage(engine).await?;
    }
    if gap > DAY {
        invalidate_old_cases(engine).await?;
    }
    let mut tx = engine.db.begin().await?;
    let plans = list::<Plan>(&mut *tx, None).await?;
    tx.commit().await?;
    for plan in plans {
        engine.cleanup_plan(plan.id).await?;
        engine.tick_plan(plan.id).await?;
    }
    let mut tx = engine.db.begin().await?;
    let now = tx.now().await?;
    let objects = tx.due(Kind::FileObject, now, 100).await?;
    tx.commit().await?;
    for raw in objects {
        let object: FileObject = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
        engine.garbage_collect(object.id).await?;
    }
    Ok(())
}
async fn invalidate_old_cases(engine: &Engine) -> Result<()> {
    let mut tx = engine.db.begin().await?;
    let plans = list::<Plan>(&mut *tx, None).await?;
    tx.commit().await?;
    for p in plans {
        let mut tx = engine.db.begin().await?;
        tx.lock(Kind::Plan, p.id).await?;
        let now = tx.now().await?;
        for mut s in list::<application::Secret>(&mut *tx, Some(p.id)).await? {
            if let Some(id) = s.last_case {
                let mut c: application::CaseRecord = get(&mut *tx, id).await?;
                if c.case.state != CaseState::Complete {
                    c.case.cancel();
                    c.due_at = i64::MAX;
                    put(&mut *tx, Some(s.id), &c).await?;
                    for sub in list::<application::Submission>(&mut *tx, Some(id)).await? {
                        tx.remove(Kind::Submission, sub.id).await?;
                    }
                    s.last_case = None;
                    s.due_at = now + 7 * DAY;
                    put(&mut *tx, Some(p.id), &s).await?;
                }
            }
        }
        tx.commit().await?;
    }
    Ok(())
}
async fn hold_after_outage(engine: &Engine) -> Result<()> {
    let mut tx = engine.db.begin().await?;
    let plans = list::<Plan>(&mut *tx, None).await?;
    tx.commit().await?;
    for p in plans {
        let mut tx = engine.db.begin().await?;
        tx.lock(Kind::Plan, p.id).await?;
        let mut plan: Plan = get(&mut *tx, p.id).await?;
        let now = tx.now().await?;
        let delay = list::<application::Secret>(&mut *tx, Some(p.id))
            .await?
            .iter()
            .map(|s| s.policy.timing.release_delay_seconds)
            .max()
            .unwrap_or(plan.timing.release_delay_seconds);
        if plan.hold_until <= now {
            let profile: application::Profile = get(&mut *tx, plan.profile_id).await?;
            application::notice(&mut *tx, plan.id, profile.owner_id, "outage-hold", now).await?;
        }
        plan.hold_until = plan.hold_until.max(now + delay);
        put(&mut *tx, Some(plan.profile_id), &plan).await?;
        tx.commit().await?;
    }
    Ok(())
}
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}};
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

async fn supervise_leader(
    mut leader: sqlx::pool::PoolConnection<sqlx::Postgres>,
    stopping: Arc<AtomicBool>,
) -> Result<()> {
    while !stopping.load(Ordering::Acquire) {
        // Check the original session independently of a 30s Telegram poll.
        // Losing it is fatal; reconnecting would silently lose leadership.
        tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query("SELECT 1").execute(&mut *leader),
        )
        .await
        .map_err(|_| Error::Storage)?
        .map_err(|_| Error::Storage)?;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires explicitly configured local PostgreSQL"]
    async fn lost_leader_session_fails_without_reconnecting() {
        let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
        let parsed = reqwest::Url::parse(&url).unwrap();
        assert!(parsed.path().ends_with("_test"));
        let db = PgDatabase::connect(&url, 3).await.unwrap();
        // Hold a private test advisory lock, not the running bot's leadership key.
        let mut leader = db.pool.acquire().await.unwrap();
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(1_934_824_192_i64)
            .execute(&mut *leader)
            .await
            .unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *leader)
            .await
            .unwrap();
        let supervisor = tokio::spawn(supervise_leader(leader, Arc::new(AtomicBool::new(false))));
        // Terminate only the connection created above, never an unrelated backend.
        let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
            .bind(pid)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert!(terminated);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(4), supervisor)
                .await
                .unwrap()
                .unwrap(),
            Err(Error::Storage)
        ));
    }
}
