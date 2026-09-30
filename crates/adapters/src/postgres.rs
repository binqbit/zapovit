use application::{Database, Envelope, Error, Kind, Result, Transaction};
use async_trait::async_trait;
use domain::Id;
use serde_json::Value;
use sqlx::{PgPool, Postgres, Row, postgres::PgPoolOptions};
use std::time::Duration;

#[derive(Clone)]
pub struct PgDatabase {
    pub pool: PgPool,
}
impl PgDatabase {
    pub async fn connect(url: &str, max: u32) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(max)
            .acquire_timeout(Duration::from_secs(10))
            .connect(url)
            .await
            .map_err(|_| Error::Storage)?;
        Ok(Self { pool })
    }
    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("../../migrations")
            .run(&self.pool)
            .await
            .map_err(|_| Error::Storage)
    }
    pub async fn bind_bot(&self, bot_id: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO telegram_cursor(singleton,bot_id) VALUES(true,$1) ON CONFLICT DO NOTHING",
        )
        .bind(bot_id)
        .execute(&self.pool)
        .await
        .map_err(|_| Error::Storage)?;
        let existing: i64 =
            sqlx::query_scalar("SELECT bot_id FROM telegram_cursor WHERE singleton")
                .fetch_one(&self.pool)
                .await
                .map_err(|_| Error::Storage)?;
        if existing != bot_id {
            return Err(Error::Config);
        }
        Ok(())
    }
    /// Call under the poller leadership lock, before accepting any bot traffic.
    /// A bot binding survives deletion of user data, so an installation that
    /// has already run cannot silently replace a lost journal with empty history.
    pub async fn journal_bootstrap_allowed(&self) -> Result<bool> {
        sqlx::query_scalar(
            "SELECT NOT (
                EXISTS(SELECT 1 FROM telegram_cursor) OR
                EXISTS(SELECT 1 FROM telegram_inbox) OR
                EXISTS(SELECT 1 FROM accounts) OR
                EXISTS(SELECT 1 FROM profiles) OR
                EXISTS(SELECT 1 FROM plans) OR
                EXISTS(SELECT 1 FROM participants) OR
                EXISTS(SELECT 1 FROM invitations) OR
                EXISTS(SELECT 1 FROM drafts) OR
                EXISTS(SELECT 1 FROM secret_versions) OR
                EXISTS(SELECT 1 FROM secret_guardians) OR
                EXISTS(SELECT 1 FROM release_cases) OR
                EXISTS(SELECT 1 FROM guardian_submissions) OR
                EXISTS(SELECT 1 FROM file_objects) OR
                EXISTS(SELECT 1 FROM delivery_parts) OR
                EXISTS(SELECT 1 FROM recovery_claims) OR
                EXISTS(SELECT 1 FROM cancellation_requests) OR
                EXISTS(SELECT 1 FROM dialogs) OR
                EXISTS(SELECT 1 FROM actions) OR
                EXISTS(SELECT 1 FROM outbox) OR
                EXISTS(SELECT 1 FROM control_intents) OR
                EXISTS(SELECT 1 FROM handled_events) OR
                EXISTS(SELECT 1 FROM delivery_attempts) OR
                EXISTS(SELECT 1 FROM deletion_tombstones) OR
                EXISTS(SELECT 1 FROM rate_limits) OR
                EXISTS(SELECT 1 FROM audit_events)
            ) AND EXISTS(SELECT 1 FROM maintenance WHERE singleton AND NOT enabled)",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|_| Error::Storage)
    }
    /// A session-level lock is held by the poller connection for its entire lifetime.
    pub async fn poller_lock(&self) -> Result<sqlx::pool::PoolConnection<Postgres>> {
        let mut connection = self.pool.acquire().await.map_err(|_| Error::Storage)?;
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(734219882)")
            .fetch_one(&mut *connection)
            .await
            .map_err(|_| Error::Storage)?;
        if !acquired {
            return Err(Error::Config);
        }
        Ok(connection)
    }
    pub async fn cursor(&self) -> Result<i64> {
        sqlx::query_scalar("SELECT next_offset FROM telegram_cursor WHERE singleton")
            .fetch_one(&self.pool)
            .await
            .map_err(|_| Error::Storage)
    }
    pub async fn ingest(&self, bot: i64, updates: &[(i64, Envelope)]) -> Result<()> {
        self.ingest_prioritized(
            bot,
            &updates
                .iter()
                .map(|(id, envelope)| (*id, envelope.clone(), false))
                .collect::<Vec<_>>(),
        )
        .await
    }
    pub async fn ingest_prioritized(
        &self,
        bot: i64,
        updates: &[(i64, Envelope, bool)],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::Storage)?;
        for (id, envelope, priority) in updates {
            sqlx::query("INSERT INTO telegram_inbox(bot_id,update_id,envelope,priority) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING")
                .bind(bot).bind(id).bind(sqlx::types::Json(envelope)).bind(priority).execute(&mut *tx).await.map_err(|_|Error::Storage)?;
        }
        if let Some(next) = updates.iter().map(|(id, _, _)| id + 1).max() {
            sqlx::query("UPDATE telegram_cursor SET next_offset=$1,last_event_at=clock_timestamp(),hold_until=CASE WHEN last_poll_at < clock_timestamp()-interval '120 seconds' THEN GREATEST(hold_until,floor(extract(epoch from clock_timestamp()))::bigint+86400) ELSE hold_until END,last_poll_at=clock_timestamp() WHERE singleton AND bot_id=$2")
                .bind(next).bind(bot).execute(&mut *tx).await.map_err(|_|Error::Storage)?;
        } else {
            sqlx::query("UPDATE telegram_cursor SET hold_until=CASE WHEN last_poll_at < clock_timestamp()-interval '120 seconds' THEN GREATEST(hold_until,floor(extract(epoch from clock_timestamp()))::bigint+86400) ELSE hold_until END,last_poll_at=clock_timestamp() WHERE singleton AND bot_id=$1").bind(bot).execute(&mut *tx).await.map_err(|_|Error::Storage)?;
        }
        tx.commit().await.map_err(|_| Error::Storage)
    }
    pub async fn pending_updates(&self, bot: i64) -> Result<Vec<(i64, Envelope)>> {
        self.pending_updates_class(bot, None).await
    }
    pub async fn pending_updates_class(
        &self,
        bot: i64,
        priority: Option<bool>,
    ) -> Result<Vec<(i64, Envelope)>> {
        let rows=sqlx::query("SELECT update_id,envelope FROM telegram_inbox WHERE bot_id=$1 AND processed_at IS NULL AND ($2::boolean IS NULL OR priority=$2) ORDER BY priority DESC,update_id LIMIT 1")
            .bind(bot).bind(priority).fetch_all(&self.pool).await.map_err(|_|Error::Storage)?;
        rows.into_iter()
            .map(|r| {
                Ok((
                    r.try_get("update_id").map_err(|_| Error::Internal)?,
                    r.try_get::<sqlx::types::Json<Envelope>, _>("envelope")
                        .map_err(|_| Error::Internal)?
                        .0,
                ))
            })
            .collect()
    }
    pub async fn complete_update(&self, bot: i64, update: i64) -> Result<()> {
        sqlx::query("UPDATE telegram_inbox SET processed_at=clock_timestamp(),envelope=NULL WHERE bot_id=$1 AND update_id=$2")
            .bind(bot).bind(update).execute(&self.pool).await.map_err(|_|Error::Storage)?;
        Ok(())
    }
    pub async fn has_backlog(&self) -> Result<bool> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM telegram_inbox WHERE processed_at IS NULL)")
            .fetch_one(&self.pool)
            .await
            .map_err(|_| Error::Storage)
    }
    pub async fn healthy_polling(&self) -> Result<bool> {
        sqlx::query_scalar("SELECT COALESCE(last_poll_at > clock_timestamp()-interval '120 seconds',false) FROM telegram_cursor WHERE singleton")
            .fetch_optional(&self.pool).await.map(|v|v.unwrap_or(false)).map_err(|_|Error::Storage)
    }
    pub async fn scheduler_heartbeat(&self) -> Result<i64> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::Storage)?;
        let gap:i64=sqlx::query_scalar("SELECT COALESCE(floor(extract(epoch from clock_timestamp()-LEAST(last_scheduler_at,last_poll_at)))::bigint,0) FROM telegram_cursor WHERE singleton FOR UPDATE")
            .fetch_one(&mut *tx).await.map_err(|_|Error::Storage)?;
        sqlx::query("UPDATE telegram_cursor SET last_scheduler_at=clock_timestamp(),hold_until=CASE WHEN $1 > 120 THEN GREATEST(hold_until,floor(extract(epoch from clock_timestamp()))::bigint+86400) ELSE hold_until END WHERE singleton")
            .bind(gap).execute(&mut *tx).await.map_err(|_|Error::Storage)?;
        tx.commit().await.map_err(|_| Error::Storage)?;
        Ok(gap)
    }
    pub async fn ready_jobs(&self) -> Result<Vec<Id>> {
        sqlx::query_scalar("SELECT id FROM outbox WHERE (state IN ('queued','retryable_failed') AND due_at<=floor(extract(epoch from clock_timestamp()))::bigint) OR (state IN ('claimed','dispatching') AND (data->>'lease_until')::bigint<=floor(extract(epoch from clock_timestamp()))::bigint) ORDER BY (data->>'priority')::int,due_at,id LIMIT 100")
            .fetch_all(&self.pool).await.map_err(|_|Error::Storage)
    }
    pub async fn poll_offset(&self) -> Result<i64> {
        // Telegram may choose a random update ID after at least a week without updates.
        sqlx::query_scalar("SELECT CASE WHEN last_event_at < clock_timestamp()-interval '7 days' THEN 0 ELSE next_offset END FROM telegram_cursor WHERE singleton")
            .fetch_one(&self.pool).await.map_err(|_|Error::Storage)
    }
    pub async fn schema_current(&self) -> Result<()> {
        let expected = sqlx::migrate!("../../migrations");
        let rows =
            sqlx::query("SELECT version,success,checksum FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&self.pool)
                .await
                .map_err(|_| Error::Config)?;
        if rows.len() != expected.iter().len() {
            return Err(Error::Config);
        }
        for (row, migration) in rows.iter().zip(expected.iter()) {
            let version: i64 = row.try_get("version").map_err(|_| Error::Config)?;
            let success: bool = row.try_get("success").map_err(|_| Error::Config)?;
            let checksum: Vec<u8> = row.try_get("checksum").map_err(|_| Error::Config)?;
            if !success || version != migration.version || checksum != migration.checksum.as_ref() {
                return Err(Error::Config);
            }
        }
        Ok(())
    }
    pub async fn priority_owner(&self, telegram_id: i64) -> Result<bool> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts a JOIN profiles p ON p.data->>'owner_id'=a.id::text JOIN plans plan ON plan.data->>'profile_id'=p.id::text WHERE a.data->>'telegram_id'=$1 AND p.state<>'deleted' AND plan.state<>'deleted')")
            .bind(telegram_id.to_string()).fetch_one(&self.pool).await.map_err(|_|Error::Storage)
    }
    pub async fn priority_callback(&self, id: Id, telegram_id: i64) -> Result<bool> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM actions action JOIN accounts a ON action.data->>'actor_id'=a.id::text JOIN plans plan ON action.data->>'plan_id'=plan.id::text JOIN profiles p ON plan.data->>'profile_id'=p.id::text WHERE action.id=$1 AND a.data->>'telegram_id'=$2 AND action.data->>'name' IN ('stop','checkin') AND action.data->>'used'='false' AND (action.data->>'expires_at')::bigint>floor(extract(epoch from clock_timestamp()))::bigint AND p.data->>'owner_id'=a.id::text AND action.data->>'owner_epoch'=p.data->>'owner_epoch' AND p.state<>'deleted' AND plan.state<>'deleted')")
            .bind(id).bind(telegram_id.to_string()).fetch_one(&self.pool).await.map_err(|_|Error::Storage)
    }
    pub async fn metrics(&self) -> Result<String> {
        let row=sqlx::query("SELECT (SELECT count(*) FROM telegram_inbox WHERE processed_at IS NULL) AS inbox, (SELECT count(*) FROM outbox WHERE state IN ('queued','claimed','retryable_failed')) AS jobs, (SELECT count(*) FROM delivery_parts WHERE state='unknown') AS unknown, (SELECT count(*) FROM outbox WHERE state='permanent_failed') AS failed, (SELECT COALESCE(sum((data->>'size')::bigint),0)::bigint FROM file_objects WHERE state IN ('pending','gc')) AS pending_bytes, (SELECT COALESCE(floor(extract(epoch from clock_timestamp()-last_poll_at))::bigint,0) FROM telegram_cursor WHERE singleton) AS poll_age, (SELECT GREATEST(hold_until-floor(extract(epoch from clock_timestamp()))::bigint,0) FROM telegram_cursor WHERE singleton) AS hold_seconds").fetch_one(&self.pool).await.map_err(|_|Error::Storage)?;
        let mut output = String::new();
        for name in [
            "inbox",
            "jobs",
            "unknown",
            "failed",
            "pending_bytes",
            "poll_age",
            "hold_seconds",
        ] {
            let value: i64 = row
                .try_get::<Option<i64>, _>(name)
                .map_err(|_| Error::Internal)?
                .unwrap_or(0);
            output.push_str(&format!("zapovit_{name} {value}\n"));
        }
        output.push_str(&format!(
            "zapovit_db_connections {}\nzapovit_db_idle {}\n",
            self.pool.size(),
            self.pool.num_idle()
        ));
        Ok(output)
    }
}

struct PgTransaction {
    tx: sqlx::Transaction<'static, Postgres>,
}
#[async_trait]
impl Database for PgDatabase {
    async fn begin(&self) -> Result<Box<dyn Transaction>> {
        Ok(Box::new(PgTransaction {
            tx: self.pool.begin().await.map_err(|_| Error::Storage)?,
        }))
    }
}
#[async_trait]
// Dynamic SQL interpolates only Kind::table(), a closed enum of static identifiers.
// Every value (including JSON field selectors) is bound as a parameter.
impl Transaction for PgTransaction {
    async fn writes_ready(&mut self) -> Result<bool> {
        sqlx::query_scalar("SELECT NOT enabled FROM maintenance WHERE singleton FOR SHARE")
            .fetch_one(&mut *self.tx)
            .await
            .map_err(|_| Error::Storage)
    }
    async fn operational_ready(&mut self) -> Result<bool> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM telegram_cursor WHERE last_poll_at > clock_timestamp() - interval '120 seconds' AND last_scheduler_at > clock_timestamp() - interval '120 seconds' AND hold_until <= floor(extract(epoch from clock_timestamp()))::bigint) AND NOT EXISTS(SELECT 1 FROM telegram_inbox WHERE processed_at IS NULL) AND NOT EXISTS(SELECT 1 FROM maintenance WHERE enabled)")
            .fetch_one(&mut *self.tx).await.map_err(|_|Error::Storage)
    }
    async fn now(&mut self) -> Result<i64> {
        sqlx::query_scalar("SELECT floor(extract(epoch from clock_timestamp()))::bigint")
            .fetch_one(&mut *self.tx)
            .await
            .map_err(|_| Error::Storage)
    }
    async fn lock(&mut self, kind: Kind, id: Id) -> Result<()> {
        // The advisory lock also serializes creation when the row does not exist yet.
        // Telegram-derived account UUIDs share their high 64 bits. Hash the full
        // typed identity so independent accounts do not serialize on one global lock.
        let key = format!("{}/{id}", kind.table());
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(key)
            .execute(&mut *self.tx)
            .await
            .map_err(|_| Error::Storage)?;
        let query = format!("SELECT id FROM {} WHERE id=$1 FOR UPDATE", kind.table());
        sqlx::query(sqlx::AssertSqlSafe(query.as_str()))
            .bind(id)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(|_| Error::Storage)?;
        Ok(())
    }
    async fn get(&mut self, kind: Kind, id: Id) -> Result<Option<Value>> {
        let q = format!("SELECT data FROM {} WHERE id=$1", kind.table());
        sqlx::query_scalar(sqlx::AssertSqlSafe(q.as_str()))
            .bind(id)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(|_| Error::Storage)
    }
    async fn list(&mut self, kind: Kind, scope: Option<Id>) -> Result<Vec<Value>> {
        let q = format!(
            "SELECT data FROM {} WHERE ($1::uuid IS NULL OR scope_id=$1) ORDER BY id LIMIT 10001",
            kind.table()
        );
        let rows: Vec<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(q.as_str()))
            .bind(scope)
            .fetch_all(&mut *self.tx)
            .await
            .map_err(|_| Error::Storage)?;
        if rows.len() > 10_000 {
            return Err(Error::Storage);
        }
        Ok(rows)
    }
    async fn find(&mut self, kind: Kind, field: &str, value: &str) -> Result<Vec<Value>> {
        let q = format!(
            "SELECT data FROM {} WHERE data->>$1=$2 ORDER BY id LIMIT 10001",
            kind.table()
        );
        let rows: Vec<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(q.as_str()))
            .bind(field)
            .bind(value)
            .fetch_all(&mut *self.tx)
            .await
            .map_err(|_| Error::Storage)?;
        if rows.len() > 10_000 {
            return Err(Error::Storage);
        }
        Ok(rows)
    }
    async fn due(&mut self, kind: Kind, now: i64, limit: i64) -> Result<Vec<Value>> {
        let q = format!(
            "SELECT data FROM {} WHERE due_at<=$1 ORDER BY due_at,id LIMIT $2",
            kind.table()
        );
        sqlx::query_scalar(sqlx::AssertSqlSafe(q.as_str()))
            .bind(now)
            .bind(limit.clamp(1, 1000))
            .fetch_all(&mut *self.tx)
            .await
            .map_err(|_| Error::Storage)
    }
    async fn put(&mut self, kind: Kind, id: Id, scope: Option<Id>, value: Value) -> Result<()> {
        let q = format!(
            "INSERT INTO {}(id,scope_id,data) VALUES($1,$2,$3) ON CONFLICT(id) DO UPDATE SET scope_id=EXCLUDED.scope_id,data=EXCLUDED.data,updated_at=clock_timestamp()",
            kind.table()
        );
        sqlx::query(sqlx::AssertSqlSafe(q.as_str()))
            .bind(id)
            .bind(scope)
            .bind(value)
            .execute(&mut *self.tx)
            .await
            .map_err(|_| Error::Storage)?;
        Ok(())
    }
    async fn remove(&mut self, kind: Kind, id: Id) -> Result<()> {
        let q = format!("DELETE FROM {} WHERE id=$1", kind.table());
        sqlx::query(sqlx::AssertSqlSafe(q.as_str()))
            .bind(id)
            .execute(&mut *self.tx)
            .await
            .map_err(|_| Error::Storage)?;
        Ok(())
    }
    async fn rate_limit(&mut self, key: &str, capacity: i64, period: i64) -> Result<bool> {
        if capacity < 1 || period < 1 {
            return Err(Error::Internal);
        }
        let now = self.now().await?;
        let window = now / period * period;
        let count:i64=sqlx::query_scalar("INSERT INTO rate_limits(key,window_start,used) VALUES($1,$2,1) ON CONFLICT(key) DO UPDATE SET window_start=$2,used=CASE WHEN rate_limits.window_start=$2 THEN rate_limits.used+1 ELSE 1 END RETURNING used")
            .bind(key).bind(window).fetch_one(&mut *self.tx).await.map_err(|_|Error::Storage)?;
        Ok(count <= capacity)
    }
    async fn commit(self: Box<Self>) -> Result<()> {
        self.tx.commit().await.map_err(|_| Error::Storage)
    }
}
