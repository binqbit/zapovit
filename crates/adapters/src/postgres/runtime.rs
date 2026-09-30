use super::*;
use std::collections::BTreeSet;

const ACTION_ROUTE_SQL: &str = "SELECT action.data->>'plan_id' AS plan,action.data->>'name' AS name,COALESCE(profile.data->>'owner_id'=$2 AND action.data->>'owner_epoch'=profile.data->>'owner_epoch',false) AS current_owner FROM actions action LEFT JOIN plans plan ON action.data->>'plan_id'=plan.id::text LEFT JOIN profiles profile ON plan.data->>'profile_id'=profile.id::text WHERE action.id=$1 AND action.data->>'actor_id'=$2 AND action.data->>'used'='false' AND (action.data->>'expires_at')::bigint>floor(extract(epoch from clock_timestamp()))::bigint AND (action.data->>'plan_id' IS NULL OR (plan.state<>'deleted' AND profile.state<>'deleted' AND (action.data->>'owner_epoch' IS NULL OR (action.data->>'owner_epoch'=profile.data->>'owner_epoch' AND profile.data->>'owner_id'=$2)) AND (action.data->>'name' IN ('stop','stop-secret','stop-confirmed','stop-secret-confirmed','stop-cancel','checkin','ack-grant','ack-recovery','ack-claim') OR action.data->>'epoch'=plan.data->>'epoch')))";

#[derive(Clone, Default)]
pub struct InboxRoute {
    pub actor_id: Option<Id>,
    pub plans: BTreeSet<Id>,
    pub protective: bool,
    pub protected_plans: BTreeSet<Id>,
    pub control_command: bool,
    pub priority: bool,
    pub verified_recovery: Option<(Id, Id)>,
    pub admission_rejected: bool,
    pub coalesce_key: Option<String>,
    pub sensitive_message: Option<(i64, i64, i64)>,
}

pub struct RoutedUpdate {
    pub update_id: i64,
    pub envelope: Envelope,
    pub route: InboxRoute,
}

pub struct IngestOutcome {
    /// Prefix accepted, deduplicated, coalesced or explicitly rejected. The suffix
    /// remains unaccepted and must be retried without advancing Telegram's offset.
    pub consumed: usize,
    pub rejected: Vec<Id>,
}

pub struct InboxClaim {
    pub update_id: i64,
    pub envelope: Envelope,
    pub lease_token: Id,
}

impl PgDatabase {
    /// Routing never trusts the text `/stop` by itself. Only private actors with
    /// a current owner binding, actor-bound stored callbacks, or an authenticated
    /// recovery proof may create a protective barrier. The Engine still authenticates
    /// every command; a routing match alone grants no business authority.
    pub async fn route_update(&self, update: &Value) -> Result<InboxRoute> {
        let (from, chat, text, callback) = if let Some(message) = update.get("message") {
            (
                &message["from"],
                &message["chat"],
                message["text"].as_str().unwrap_or(""),
                None,
            )
        } else if let Some(callback) = update.get("callback_query") {
            (
                &callback["from"],
                &callback["message"]["chat"],
                "",
                callback["data"].as_str(),
            )
        } else {
            return Ok(InboxRoute::default());
        };
        let Some(telegram_id) = from["id"].as_i64().filter(|id| *id > 0) else {
            return Ok(InboxRoute::default());
        };
        if chat["type"] != "private"
            || from["is_bot"] == true
            || chat["id"].as_i64() != Some(telegram_id)
        {
            return Ok(InboxRoute::default());
        }
        let actor = Id::from_u128(telegram_id as u128);
        let mut route = InboxRoute {
            actor_id: Some(actor),
            ..Default::default()
        };
        // All participant lanes are serialized, even when a message only changes
        // dialog state. Protective barriers themselves are much more narrowly scoped.
        let plans: Vec<Id> = sqlx::query_scalar("SELECT DISTINCT p.id FROM plans p JOIN profiles profile ON p.data->>'profile_id'=profile.id::text WHERE p.state<>'deleted' AND (profile.data->>'owner_id'=$1 OR EXISTS(SELECT 1 FROM participants participant WHERE participant.scope_id=p.id AND participant.data->>'account_id'=$1) OR EXISTS(SELECT 1 FROM recovery_claims claim WHERE claim.data->>'profile_id'=profile.id::text AND claim.data#>>'{target,id}'=$1))")
            .bind(actor.to_string()).fetch_all(&self.pool).await.map_err(|_|Error::Storage)?;
        route.plans.extend(plans);
        let command = text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .split('@')
            .next()
            .unwrap_or("");
        if (text.trim().starts_with("R1.")
            || text.trim().starts_with("Z1.")
            || (matches!(command, "/recover" | "/recoverstop")
                && text.split_whitespace().count() > 1))
            && let Some(message) = update["message"]["message_id"]
                .as_i64()
                .filter(|id| *id > 0)
        {
            route.sensitive_message = Some((
                telegram_id,
                message,
                update["message"]["date"].as_i64().unwrap_or(0),
            ));
        }
        route.control_command = matches!(command, "/stop" | "/checkin");
        if route.control_command && self.priority_owner(telegram_id).await? {
            route.priority = true;
            route.coalesce_key = Some(command.to_string());
        }
        if let Some(action_id) = callback.and_then(|value| Id::parse_str(value).ok()) {
            let row = sqlx::query(ACTION_ROUTE_SQL)
                .bind(action_id)
                .bind(actor.to_string())
                .fetch_optional(&self.pool)
                .await
                .map_err(|_| Error::Storage)?;
            if let Some(row) = row {
                let plan = row
                    .try_get::<Option<String>, _>("plan")
                    .map_err(|_| Error::Storage)?
                    .and_then(|value| Id::parse_str(&value).ok());
                let name: String = row.try_get("name").map_err(|_| Error::Storage)?;
                route.coalesce_key = Some(format!("callback:{action_id}"));
                if let Some(plan) = plan {
                    route.plans.insert(plan);
                    // Newly added mutating capabilities fail safe by default.
                    if !matches!(
                        name.as_str(),
                        "home"
                            | "plan-menu"
                            | "settings"
                            | "language"
                            | "recovery-options"
                            | "secrets"
                            | "participants"
                            | "guardians"
                            | "preview"
                            | "retry-delivery"
                            | "retry-confirmed"
                    ) && !name.starts_with("draft:")
                    {
                        route.protected_plans.insert(plan);
                    }
                }
                route.priority = self.priority_callback(action_id, telegram_id).await?;
            }
        }
        route.protective = !route.protected_plans.is_empty();
        Ok(route)
    }

    /// A selector is public metadata, not recovery proof. Only a verified key
    /// may hold its target plan. Budgets commit before memory-hard verification;
    /// rejected admission never proceeds later as an ordinary recovery event.
    pub async fn authenticate_recovery_route(
        &self,
        update: &Value,
        route: &mut InboxRoute,
        engine: &application::Engine,
    ) -> Result<()> {
        let Some(actor) = route.actor_id else {
            return Ok(());
        };
        let text = update["message"]["text"].as_str().unwrap_or("").trim();
        let command = text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .split('@')
            .next()
            .unwrap_or("");
        let token = if matches!(command, "/recover" | "/recoverstop") {
            text.split_once(char::is_whitespace)
                .map(|(_, value)| value.trim())
                .unwrap_or("")
        } else {
            text
        };
        let Ok(selector) = crate::crypto::ArgonHasher::selector(token) else {
            return Ok(());
        };
        let mut tx = self.begin().await?;
        let Some(raw) = tx
            .find(Kind::Profile, "recovery_selector", &selector.to_string())
            .await?
            .into_iter()
            .find(|raw| raw["state"] == "active")
        else {
            return Ok(());
        };
        let profile: application::Profile =
            serde_json::from_value(raw).map_err(|_| Error::Internal)?;
        let allowed = tx.rate_limit("admission:recovery-routing", 20, 60).await?
            && tx
                .rate_limit(&format!("admission:recovery-routing:{actor}"), 5, 3600)
                .await?;
        tx.commit().await?;
        if !allowed {
            route.admission_rejected = true;
            return Ok(());
        }
        match engine
            .recovery
            .verify(token, selector, &profile.recovery_hash)
            .await
        {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(Error::RateLimited) => {
                route.admission_rejected = true;
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        route.verified_recovery = Some((profile.id, selector));
        Ok(())
    }

    /// Excess ordinary events are rejected before application effects. Their cursor
    /// is still advanced, so a public traffic flood cannot hide later STOP events.
    /// Returned existing actor IDs may receive bounded, non-sensitive busy feedback.
    pub async fn ingest_routed(&self, bot: i64, updates: &[RoutedUpdate]) -> Result<IngestOutcome> {
        self.ingest_batch(bot, updates, true).await
    }

    /// Retry the unaccepted encrypted suffix without pretending another Telegram
    /// poll completed. A sustained full reserve therefore triggers stale-poll hold.
    pub async fn retry_ingest_routed(
        &self,
        bot: i64,
        updates: &[RoutedUpdate],
    ) -> Result<IngestOutcome> {
        self.ingest_batch(bot, updates, false).await
    }

    async fn ingest_batch(
        &self,
        bot: i64,
        updates: &[RoutedUpdate],
        polled: bool,
    ) -> Result<IngestOutcome> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::Storage)?;
        // This lock is the dispatch watermark: all rows in a poll batch become
        // visible atomically before the cursor can be read by dispatch.
        sqlx::query("SELECT bot_id FROM telegram_cursor WHERE singleton AND bot_id=$1 FOR UPDATE")
            .bind(bot)
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| Error::Storage)?;
        let mut backlog: i64 = sqlx::query_scalar("SELECT count(*) FROM (SELECT 1 FROM telegram_inbox WHERE processed_at IS NULL LIMIT 100000) pending").fetch_one(&mut *tx).await.map_err(|_|Error::Storage)?;
        let mut rejected = BTreeSet::new();
        let mut consumed = 0;
        for update in updates {
            // Replayed provider rows already have durable ownership and do not
            // require new capacity, including quarantined accepted controls.
            let existing: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM telegram_inbox WHERE bot_id=$1 AND update_id=$2)",
            )
            .bind(bot)
            .bind(update.update_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| Error::Storage)?;
            if existing {
                consumed += 1;
                continue;
            }
            let mut route = update.route.clone();
            if route.admission_rejected {
                Self::rejected_cleanup(&mut tx, bot, update.update_id, &route).await?;
                if let Some(actor) = route.actor_id {
                    rejected.insert(actor);
                }
                consumed += 1;
                continue;
            }
            if let Some(action_id) = route
                .coalesce_key
                .as_deref()
                .and_then(|key| key.strip_prefix("callback:"))
                .and_then(|key| Id::parse_str(key).ok())
            {
                let row = sqlx::query(ACTION_ROUTE_SQL)
                    .bind(action_id)
                    .bind(route.actor_id.ok_or(Error::Internal)?.to_string())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|_| Error::Storage)?;
                if let Some(row) = row {
                    let name: String = row.try_get("name").map_err(|_| Error::Storage)?;
                    route.priority = matches!(
                        name.as_str(),
                        "stop"
                            | "stop-secret"
                            | "stop-confirmed"
                            | "stop-secret-confirmed"
                            | "stop-cancel"
                            | "checkin"
                    ) && row
                        .try_get::<bool, _>("current_owner")
                        .map_err(|_| Error::Storage)?;
                } else {
                    route.priority = false;
                    route.protective = false;
                    route.protected_plans.clear();
                    route.coalesce_key = None;
                }
            }
            if let Some((profile, selector)) = route.verified_recovery {
                // A concurrent rotation invalidates the old proof. Recheck inside
                // the watermark before any scope can become a dispatch barrier.
                let plans:Vec<Id>=sqlx::query_scalar("SELECT p.id FROM plans p JOIN profiles profile ON p.data->>'profile_id'=profile.id::text WHERE profile.id=$1 AND profile.data->>'recovery_selector'=$2 AND profile.state='active' AND p.state<>'deleted'").bind(profile).bind(selector.to_string()).fetch_all(&mut *tx).await.map_err(|_|Error::Storage)?;
                route.protected_plans.extend(plans.iter().copied());
                route.plans.extend(plans);
                route.protective = !route.protected_plans.is_empty();
            }
            if route.control_command
                && let Some(actor) = route.actor_id
            {
                // Resolve again inside the ingress watermark transaction. Earlier
                // recovery commands in this same batch may establish a new owner.
                let owned: Vec<Id> = sqlx::query_scalar("SELECT p.id FROM plans p JOIN profiles profile ON p.data->>'profile_id'=profile.id::text WHERE p.state<>'deleted' AND (profile.data->>'owner_id'=$1 OR EXISTS(SELECT 1 FROM recovery_claims claim WHERE claim.data->>'profile_id'=profile.id::text AND claim.data#>>'{target,id}'=$1)) UNION SELECT s.plan_id FROM telegram_inbox_scopes s JOIN telegram_inbox i USING(bot_id,update_id) JOIN profiles proof ON proof.id=i.recovery_profile AND proof.data->>'recovery_selector'=i.recovery_selector::text AND proof.state='active' WHERE i.actor_id=$2 AND i.processed_at IS NULL AND s.protective")
                    .bind(actor.to_string()).bind(actor).fetch_all(&mut *tx).await.map_err(|_|Error::Storage)?;
                route.priority=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM profiles profile JOIN plans plan ON plan.data->>'profile_id'=profile.id::text WHERE profile.data->>'owner_id'=$1 AND profile.state='active' AND plan.state<>'deleted')").bind(actor.to_string()).fetch_one(&mut *tx).await.map_err(|_|Error::Storage)?;
                if !route.priority {
                    route.coalesce_key = None;
                }
                route.protected_plans.clear();
                route.protected_plans.extend(owned.iter().copied());
                route.plans.extend(owned);
                route.protective = !route.protected_plans.is_empty();
            }
            if let (Some(actor), Some(key)) = (route.actor_id, route.coalesce_key.as_deref()) {
                // Coalesce only an identical unclaimed tail event. Any intervening
                // actor OR plan event prevents coalescing; the accepted operation
                // and its existing receipt/feedback identity remain unchanged.
                let duplicate:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM telegram_inbox tail WHERE tail.bot_id=$1 AND tail.actor_id=$2 AND tail.processed_at IS NULL AND tail.quarantined_at IS NULL AND tail.attempts=0 AND tail.coalesce_key=$3 AND NOT EXISTS(SELECT 1 FROM telegram_inbox later WHERE later.bot_id=tail.bot_id AND later.arrival>tail.arrival AND (later.actor_id=$2 OR EXISTS(SELECT 1 FROM telegram_inbox_scopes scope WHERE scope.bot_id=later.bot_id AND scope.update_id=later.update_id AND scope.plan_id=ANY($4)))))").bind(bot).bind(actor).bind(key).bind(route.plans.iter().copied().collect::<Vec<_>>()).fetch_one(&mut *tx).await.map_err(|_|Error::Storage)?;
                if duplicate {
                    consumed += 1;
                    continue;
                }
            }
            if backlog >= 50_000 && !route.protective && !route.priority {
                Self::rejected_cleanup(&mut tx, bot, update.update_id, &route).await?;
                if let Some(actor) = route.actor_id {
                    rejected.insert(actor);
                }
                consumed += 1;
                continue;
            }
            if backlog >= 100_000 {
                // Ordinary input was rejected above. Only a scoped protective
                // event can exhaust this reserve. Do not acknowledge, discard or
                // advance past it: commit the safe prefix and retry the suffix.
                break;
            }
            let inserted = sqlx::query("INSERT INTO telegram_inbox(bot_id,update_id,envelope,priority,actor_id,routed,protective,coalesce_key,recovery_profile,recovery_selector,processed_at) VALUES($1,$2,$3,$4,$5,true,$6,$7,$8,$9,CASE WHEN $5::uuid IS NULL THEN clock_timestamp() ELSE NULL END) ON CONFLICT DO NOTHING")
                .bind(bot).bind(update.update_id).bind(if route.actor_id.is_some(){Some(sqlx::types::Json(&update.envelope))}else{None}).bind(route.priority).bind(route.actor_id).bind(route.protective).bind(route.coalesce_key).bind(route.verified_recovery.map(|proof|proof.0)).bind(route.verified_recovery.map(|proof|proof.1)).execute(&mut *tx).await.map_err(|_|Error::Storage)?.rows_affected();
            consumed += 1;
            if inserted == 0 {
                continue;
            }
            if route.actor_id.is_some() {
                backlog += 1;
            }
            for plan in &route.plans {
                sqlx::query("INSERT INTO telegram_inbox_scopes(bot_id,update_id,plan_id,protective) VALUES($1,$2,$3,$4)").bind(bot).bind(update.update_id).bind(plan).bind(route.protected_plans.contains(plan)).execute(&mut *tx).await.map_err(|_|Error::Storage)?;
            }
        }
        let next = updates[..consumed]
            .iter()
            .filter_map(|u| u.update_id.checked_add(1))
            .max();
        sqlx::query("UPDATE telegram_cursor SET next_offset=COALESCE($1,next_offset),last_event_at=CASE WHEN $1::bigint IS NOT NULL THEN clock_timestamp() ELSE last_event_at END,hold_until=CASE WHEN last_poll_at<clock_timestamp()-interval '120 seconds' THEN GREATEST(hold_until,floor(extract(epoch from clock_timestamp()))::bigint+86400) ELSE hold_until END,last_poll_at=CASE WHEN $3 THEN clock_timestamp() ELSE last_poll_at END WHERE singleton AND bot_id=$2")
            .bind(next).bind(bot).bind(polled).execute(&mut *tx).await.map_err(|_|Error::Storage)?;
        tx.commit().await.map_err(|_| Error::Storage)?;
        Ok(IngestOutcome {
            consumed,
            rejected: rejected.into_iter().collect(),
        })
    }

    async fn rejected_cleanup(
        tx: &mut sqlx::Transaction<'_, Postgres>,
        bot: i64,
        update: i64,
        route: &InboxRoute,
    ) -> Result<()> {
        let (Some(actor), Some((chat, message, sent_at))) =
            (route.actor_id, route.sensitive_message)
        else {
            return Ok(());
        };
        // A rejection never retains credential ciphertext. Cleanup contains only
        // private chat/message identifiers and commits with the cursor decision.
        let now: i64 =
            sqlx::query_scalar("SELECT floor(extract(epoch from clock_timestamp()))::bigint")
                .fetch_one(&mut **tx)
                .await
                .map_err(|_| Error::Storage)?;
        for (key, capacity) in [
            (format!("cleanup:rejected:{actor}"), 5),
            ("cleanup:rejected:global".into(), 20),
        ] {
            let used:i64=sqlx::query_scalar("INSERT INTO rate_limits(key,window_start,used) VALUES($1,$2,1) ON CONFLICT(key) DO UPDATE SET window_start=$2,used=CASE WHEN rate_limits.window_start=$2 THEN rate_limits.used+1 ELSE 1 END RETURNING used").bind(key).bind(now/60*60).fetch_one(&mut **tx).await.map_err(|_|Error::Storage)?;
            if used > capacity {
                return Ok(());
            }
        }
        let id = Id::from_u128(((bot as u128) << 64) | update as u64 as u128);
        let job = application::Job {
            id,
            plan_id: None,
            task: application::Task::CleanupMessage {
                chat_id: chat,
                message_id: message,
                sent_at: if sent_at > 0 { sent_at } else { now },
                account_id: actor,
            },
            state: domain::PartState::Queued,
            due_at: now,
            expires_at: now + 86400,
            lease_until: 0,
            lease_token: Id::nil(),
            attempts: 0,
            message_id: None,
            priority: 0,
        };
        sqlx::query("INSERT INTO outbox(id,data) VALUES($1,$2) ON CONFLICT DO NOTHING")
            .bind(id)
            .bind(serde_json::to_value(job).map_err(|_| Error::Internal)?)
            .execute(&mut **tx)
            .await
            .map_err(|_| Error::Storage)?;
        Ok(())
    }

    pub async fn claim_update(&self, bot: i64, priority: bool) -> Result<Option<InboxClaim>> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::Storage)?;
        // Claim selection is short and serialized, processing itself is not.
        // Different workers cannot concurrently reserve overlapping actor/plan lanes.
        sqlx::query("SELECT pg_advisory_xact_lock(734219883)")
            .execute(&mut *tx)
            .await
            .map_err(|_| Error::Storage)?;
        let row=sqlx::query("SELECT i.update_id,i.envelope FROM telegram_inbox i WHERE i.bot_id=$1 AND i.priority=$2 AND i.processed_at IS NULL AND i.quarantined_at IS NULL AND i.next_attempt_at<=clock_timestamp() AND (i.lease_until IS NULL OR i.lease_until<=clock_timestamp()) AND NOT EXISTS(SELECT 1 FROM telegram_inbox earlier WHERE earlier.bot_id=i.bot_id AND earlier.processed_at IS NULL AND earlier.quarantined_at IS NULL AND earlier.update_id<>i.update_id AND ((earlier.lease_until>clock_timestamp()) OR (earlier.arrival<i.arrival AND (earlier.priority OR NOT i.priority))) AND (earlier.actor_id=i.actor_id OR EXISTS(SELECT 1 FROM telegram_inbox_scopes a JOIN telegram_inbox_scopes b ON a.plan_id=b.plan_id WHERE a.bot_id=earlier.bot_id AND a.update_id=earlier.update_id AND b.bot_id=i.bot_id AND b.update_id=i.update_id))) ORDER BY i.next_attempt_at,i.arrival LIMIT 1 FOR UPDATE OF i SKIP LOCKED")
            .bind(bot).bind(priority).fetch_optional(&mut *tx).await.map_err(|_|Error::Storage)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let update_id: i64 = row.try_get("update_id").map_err(|_| Error::Storage)?;
        let raw: Value = row.try_get("envelope").map_err(|_| Error::Storage)?;
        let envelope = match serde_json::from_value(raw) {
            Ok(envelope) => envelope,
            Err(_) => {
                sqlx::query("UPDATE telegram_inbox SET quarantined_at=clock_timestamp(),last_error='crypto_failure' WHERE bot_id=$1 AND update_id=$2").bind(bot).bind(update_id).execute(&mut *tx).await.map_err(|_|Error::Storage)?;
                tx.commit().await.map_err(|_| Error::Storage)?;
                return Ok(None);
            }
        };
        let lease_token = Id::new_v4();
        sqlx::query("UPDATE telegram_inbox SET attempts=attempts+1,lease_token=$3,lease_until=clock_timestamp()+interval '90 seconds' WHERE bot_id=$1 AND update_id=$2").bind(bot).bind(update_id).bind(lease_token).execute(&mut *tx).await.map_err(|_|Error::Storage)?;
        tx.commit().await.map_err(|_| Error::Storage)?;
        Ok(Some(InboxClaim {
            update_id,
            envelope,
            lease_token,
        }))
    }

    pub async fn finish_update(&self, bot: i64, claim: &InboxClaim) -> Result<bool> {
        Ok(sqlx::query("UPDATE telegram_inbox SET processed_at=clock_timestamp(),envelope=NULL,lease_until=NULL,lease_token=NULL WHERE bot_id=$1 AND update_id=$2 AND lease_token=$3 AND lease_until>clock_timestamp() AND processed_at IS NULL")
            .bind(bot).bind(claim.update_id).bind(claim.lease_token).execute(&self.pool).await.map_err(|_|Error::Storage)?.rows_affected()==1)
    }

    /// Five attempts with bounded exponential backoff. Quarantined protective
    /// events retain their plan barrier; ordinary malformed input cannot hold others.
    pub async fn fail_update(&self, bot: i64, claim: &InboxClaim, error: &Error) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::Storage)?;
        let row=sqlx::query("UPDATE telegram_inbox SET last_error=$4,quarantined_at=CASE WHEN attempts>=5 OR $5 THEN clock_timestamp() ELSE NULL END,next_attempt_at=clock_timestamp()+make_interval(secs=>LEAST(60,power(2,LEAST(attempts,6)))::double precision),lease_until=NULL,lease_token=NULL WHERE bot_id=$1 AND update_id=$2 AND lease_token=$3 RETURNING protective,routed,EXISTS(SELECT 1 FROM telegram_inbox_scopes WHERE bot_id=$1 AND update_id=$2) AS scoped")
            .bind(bot).bind(claim.update_id).bind(claim.lease_token).bind(error.to_string()).bind(matches!(error,Error::Crypto)).fetch_optional(&mut *tx).await.map_err(|_|Error::Storage)?;
        if let Some(row) = row {
            let protective: bool = row.try_get("protective").map_err(|_| Error::Storage)?;
            let routed: bool = row.try_get("routed").map_err(|_| Error::Storage)?;
            let scoped: bool = row.try_get("scoped").map_err(|_| Error::Storage)?;
            if matches!(error, Error::Crypto) && (!routed || (protective && !scoped)) {
                sqlx::query("UPDATE runtime_health SET integrity_failure=true WHERE singleton")
                    .execute(&mut *tx)
                    .await
                    .map_err(|_| Error::Storage)?;
            }
        }
        tx.commit().await.map_err(|_| Error::Storage)
    }

    pub async fn retry_quarantined_update(&self, bot: i64, update: i64) -> Result<()> {
        let changed=sqlx::query("UPDATE telegram_inbox SET quarantined_at=NULL,attempts=0,next_attempt_at=clock_timestamp(),lease_until=NULL,lease_token=NULL WHERE bot_id=$1 AND update_id=$2 AND processed_at IS NULL AND quarantined_at IS NOT NULL").bind(bot).bind(update).execute(&self.pool).await.map_err(|_|Error::Storage)?.rows_affected();
        if changed != 1 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// Readiness is evaluated on every HTTP probe; no cached successful-cycle bool.
    pub async fn runtime_ready(&self) -> Result<bool> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM telegram_cursor WHERE singleton AND last_poll_at>clock_timestamp()-interval '120 seconds' AND last_scheduler_at>clock_timestamp()-interval '120 seconds') AND NOT EXISTS(SELECT 1 FROM runtime_health WHERE integrity_failure) AND NOT EXISTS(SELECT 1 FROM maintenance WHERE enabled)").fetch_one(&self.pool).await.map_err(|_|Error::Storage)
    }

    pub async fn due_plans(&self) -> Result<Vec<Id>> {
        sqlx::query_scalar("SELECT plan_id FROM runtime_plan_schedule WHERE next_at<=clock_timestamp() ORDER BY next_at,plan_id LIMIT 25").fetch_all(&self.pool).await.map_err(|_|Error::Storage)
    }
    pub async fn plan_scheduled(&self, plan: Id, error: Option<&Error>) -> Result<()> {
        sqlx::query("UPDATE runtime_plan_schedule SET next_at=clock_timestamp()+make_interval(secs=>CASE WHEN $2::text IS NULL THEN 5 ELSE 30 END),last_error=$2,failures=CASE WHEN $2::text IS NULL THEN 0 ELSE failures+1 END WHERE plan_id=$1").bind(plan).bind(error.map(ToString::to_string)).execute(&self.pool).await.map_err(|_|Error::Storage)?;
        Ok(())
    }
    pub async fn retain_runtime_history(&self) -> Result<()> {
        // Control intents, journal, tombstones and delivery receipts are excluded:
        // backup anti-resurrection guarantees depend on their explicit lifecycle.
        sqlx::query("DELETE FROM telegram_inbox WHERE (bot_id,update_id) IN (SELECT bot_id,update_id FROM telegram_inbox WHERE processed_at<clock_timestamp()-interval '7 days' OR (quarantined_at<clock_timestamp()-interval '7 days' AND NOT protective) ORDER BY COALESCE(processed_at,quarantined_at) LIMIT 1000)").execute(&self.pool).await.map_err(|_|Error::Storage)?;
        sqlx::query("DELETE FROM actions WHERE id IN (SELECT id FROM actions WHERE (data->>'expires_at')::bigint<floor(extract(epoch from clock_timestamp()))::bigint ORDER BY (data->>'expires_at')::bigint LIMIT 1000)").execute(&self.pool).await.map_err(|_|Error::Storage)?;
        sqlx::query("DELETE FROM rate_limits WHERE key IN (SELECT key FROM rate_limits WHERE window_start<floor(extract(epoch from clock_timestamp()))::bigint-604800 ORDER BY window_start LIMIT 1000)").execute(&self.pool).await.map_err(|_|Error::Storage)?;
        sqlx::query("DELETE FROM operation_receipts WHERE id IN (SELECT id FROM operation_receipts WHERE due_at<floor(extract(epoch from clock_timestamp()))::bigint ORDER BY due_at LIMIT 1000)").execute(&self.pool).await.map_err(|_|Error::Storage)?;
        sqlx::query("DELETE FROM deletion_requests WHERE id IN (SELECT id FROM deletion_requests WHERE (data->>'expires_at')::bigint<floor(extract(epoch from clock_timestamp()))::bigint-86400 ORDER BY (data->>'expires_at')::bigint LIMIT 1000)").execute(&self.pool).await.map_err(|_|Error::Storage)?;
        // Non-content notifications have no anti-resurrection authority. Keep a
        // week for support; content delivery jobs/attempts retain their lifecycle.
        sqlx::query("DELETE FROM outbox WHERE id IN (SELECT id FROM outbox WHERE data#>>'{task,kind}' IN ('notice','cleanup_message') AND (data->>'expires_at')::bigint<floor(extract(epoch from clock_timestamp()))::bigint-604800 AND state NOT IN ('claimed','dispatching') ORDER BY (data->>'expires_at')::bigint LIMIT 1000)").execute(&self.pool).await.map_err(|_|Error::Storage)?;
        Ok(())
    }
    pub(super) async fn runtime_metrics(&self) -> Result<String> {
        let row=sqlx::query("SELECT (SELECT count(*) FROM telegram_inbox WHERE quarantined_at IS NOT NULL AND processed_at IS NULL) AS inbox_quarantined,(SELECT count(*) FROM telegram_inbox WHERE protective AND processed_at IS NULL) AS control_backlog,(SELECT COALESCE(floor(extract(epoch from clock_timestamp()-min(received_at)))::bigint,0) FROM telegram_inbox WHERE processed_at IS NULL AND quarantined_at IS NULL) AS inbox_oldest_seconds,(SELECT COALESCE(floor(extract(epoch from clock_timestamp()-last_scheduler_at))::bigint,0) FROM telegram_cursor WHERE singleton) AS scheduler_age,(SELECT count(*) FROM runtime_plan_schedule WHERE last_error IS NOT NULL) AS scheduler_failed_plans,(SELECT COALESCE(sum(amount),0)::bigint FROM resource_reservations WHERE resource='blob_bytes') AS reserved_blob_bytes,(SELECT count(*) FROM telegram_inbox WHERE lease_until>clock_timestamp() AND processed_at IS NULL) AS inbox_leases,(SELECT CASE WHEN integrity_failure THEN 1 ELSE 0 END::bigint FROM runtime_health WHERE singleton) AS integrity_hold,(SELECT COALESCE(GREATEST(floor(extract(epoch from clock_timestamp()-min(next_at)))::bigint,0),0) FROM runtime_plan_schedule) AS scheduler_lag_seconds,(SELECT CASE WHEN last_backup_at IS NULL THEN -1 ELSE GREATEST(floor(extract(epoch from clock_timestamp()-last_backup_at))::bigint,0) END FROM maintenance WHERE singleton) AS backup_age_seconds").fetch_one(&self.pool).await.map_err(|_|Error::Storage)?;
        let mut text = String::new();
        for name in [
            "inbox_quarantined",
            "control_backlog",
            "inbox_oldest_seconds",
            "scheduler_age",
            "scheduler_failed_plans",
            "reserved_blob_bytes",
            "inbox_leases",
            "integrity_hold",
            "scheduler_lag_seconds",
            "backup_age_seconds",
        ] {
            let value = row
                .try_get::<Option<i64>, _>(name)
                .map_err(|_| Error::Storage)?
                .unwrap_or(0);
            text.push_str(&format!("zapovit_{name} {value}\n"));
        }
        Ok(text)
    }
}

impl PgTransaction {
    pub(super) async fn admit_recovery(
        &mut self,
        account: &application::Account,
        profile_id: Id,
        selector: Id,
    ) -> Result<()> {
        if account.telegram_id <= 0
            || account.chat_id != account.telegram_id
            || account.id != Id::from_u128(account.telegram_id as u128)
        {
            return Err(Error::InvalidInput);
        }
        self.lock(Kind::Profile, profile_id).await?;
        let profile = self
            .get(Kind::Profile, profile_id)
            .await?
            .ok_or(Error::NotFound)?;
        if profile["recovery_selector"].as_str() != Some(&selector.to_string())
            || profile["state"] == "deleted"
        {
            return Err(domain::RuleError::AccessDenied.into());
        }
        if self.get(Kind::Account, account.id).await?.is_some() {
            return Ok(());
        }
        sqlx::query("SELECT pg_advisory_xact_lock(734219886)")
            .execute(&mut *self.tx)
            .await
            .map_err(|_| Error::Storage)?;
        // A rotated/deleted credential no longer needs a reserved signup slot.
        sqlx::query("DELETE FROM recovery_account_grants g WHERE NOT EXISTS(SELECT 1 FROM profiles p WHERE p.id=g.profile_id AND p.data->>'recovery_selector'=g.selector::text AND p.state<>'deleted')").execute(&mut *self.tx).await.map_err(|_|Error::Storage)?;
        let granted: Option<Id> =
            sqlx::query_scalar("SELECT account_id FROM recovery_account_grants WHERE selector=$1")
                .bind(selector)
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(|_| Error::Storage)?;
        if granted.is_some_and(|actor| actor != account.id) {
            return Err(Error::RateLimited);
        }
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM recovery_account_grants")
            .fetch_one(&mut *self.tx)
            .await
            .map_err(|_| Error::Storage)?;
        if granted.is_none() && count >= 1000 {
            return Err(Error::RateLimited);
        }
        sqlx::query("INSERT INTO recovery_account_grants(selector,profile_id,account_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(selector).bind(profile_id).bind(account.id).execute(&mut *self.tx).await.map_err(|_|Error::Storage)?;
        let value = serde_json::to_value(account).map_err(|_| Error::Internal)?;
        sqlx::query("INSERT INTO accounts(id,data) VALUES($1,$2) ON CONFLICT DO NOTHING")
            .bind(account.id)
            .bind(value)
            .execute(&mut *self.tx)
            .await
            .map_err(|_| Error::Storage)?;
        Ok(())
    }

    pub(super) async fn reserve_payload(&mut self, kind: Kind, id: Id, amount: i64) -> Result<()> {
        let resource = format!("payload/{}", kind.table());
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,19))")
            .bind(&resource)
            .execute(&mut *self.tx)
            .await
            .map_err(|_| Error::Storage)?;
        let current: Option<i64> = sqlx::query_scalar(
            "SELECT amount FROM resource_reservations WHERE resource=$1 AND reservation_id=$2",
        )
        .bind(&resource)
        .bind(id)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(|_| Error::Storage)?;
        if current == Some(amount) {
            return Ok(());
        }
        let total: i64 = sqlx::query_scalar(
            "SELECT COALESCE(sum(amount),0)::bigint FROM resource_reservations WHERE resource=$1",
        )
        .bind(&resource)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(|_| Error::Storage)?;
        // 512MiB for each encrypted content class, independent from the S3 budget.
        if amount > current.unwrap_or(0)
            && total - current.unwrap_or(0) + amount > 512 * 1024 * 1024
        {
            return Err(Error::RateLimited);
        }
        sqlx::query("INSERT INTO resource_reservations(resource,reservation_id,amount) VALUES($1,$2,$3) ON CONFLICT(resource,reservation_id) DO UPDATE SET amount=EXCLUDED.amount").bind(resource).bind(id).bind(amount).execute(&mut *self.tx).await.map_err(|_|Error::Storage)?;
        Ok(())
    }

    pub(super) async fn runtime_status(
        &mut self,
        plan_id: Option<Id>,
    ) -> Result<OperationalStatus> {
        // Ingress takes UPDATE first; this SHARE lock is held until dispatch's
        // Dispatching transition commits. Never weaken it to an unlocked snapshot.
        let writes_ready = sqlx::query_scalar::<_, bool>(
            "SELECT NOT enabled FROM maintenance WHERE singleton FOR SHARE",
        )
        .fetch_one(&mut *self.tx)
        .await
        .map_err(|_| Error::Storage)?;
        let cursor=sqlx::query("SELECT last_poll_at>clock_timestamp()-interval '120 seconds' AS poll_ok,last_scheduler_at>clock_timestamp()-interval '120 seconds' AS scheduler_ok,hold_until FROM telegram_cursor WHERE singleton FOR SHARE").fetch_optional(&mut *self.tx).await.map_err(|_|Error::Storage)?;
        let mut reasons = Vec::new();
        if !writes_ready {
            reasons.push(OperationalBlocker::Maintenance);
        }
        let mut hold_until = None;
        let now = self.now().await?;
        if let Some(cursor) = cursor {
            if !cursor
                .try_get::<Option<bool>, _>("poll_ok")
                .map_err(|_| Error::Storage)?
                .unwrap_or(false)
            {
                reasons.push(OperationalBlocker::PollStale);
            }
            if !cursor
                .try_get::<Option<bool>, _>("scheduler_ok")
                .map_err(|_| Error::Storage)?
                .unwrap_or(false)
            {
                reasons.push(OperationalBlocker::SchedulerStale);
            }
            let hold: i64 = cursor.try_get("hold_until").map_err(|_| Error::Storage)?;
            if hold > now {
                hold_until = Some(hold);
            }
        } else {
            reasons.extend([
                OperationalBlocker::PollStale,
                OperationalBlocker::SchedulerStale,
            ]);
        }
        if let Some(plan_id) = plan_id
            && let Some(raw) = self.get(Kind::Plan, plan_id).await?
        {
            if let Some(hold) = raw["hold_until"].as_i64().filter(|hold| *hold > now) {
                hold_until = Some(hold_until.unwrap_or(0).max(hold));
            }
            if raw["pending_control"].as_str().is_some() {
                reasons.push(OperationalBlocker::ControlBacklog);
            }
        }
        if hold_until.is_some() {
            reasons.push(OperationalBlocker::Hold);
        }
        let barrier=sqlx::query("SELECT EXISTS(SELECT 1 FROM telegram_inbox i WHERE i.processed_at IS NULL AND i.protective AND (NOT i.routed OR EXISTS(SELECT 1 FROM telegram_inbox_scopes scope WHERE scope.bot_id=i.bot_id AND scope.update_id=i.update_id AND scope.plan_id=$1 AND scope.protective)) AND i.quarantined_at IS NULL) AS pending, EXISTS(SELECT 1 FROM telegram_inbox i WHERE i.processed_at IS NULL AND i.protective AND (NOT i.routed OR EXISTS(SELECT 1 FROM telegram_inbox_scopes scope WHERE scope.bot_id=i.bot_id AND scope.update_id=i.update_id AND scope.plan_id=$1 AND scope.protective)) AND i.quarantined_at IS NOT NULL) AS quarantined, (SELECT integrity_failure FROM runtime_health WHERE singleton) AS integrity")
            .bind(plan_id).fetch_one(&mut *self.tx).await.map_err(|_|Error::Storage)?;
        if barrier
            .try_get::<bool, _>("pending")
            .map_err(|_| Error::Storage)?
        {
            reasons.push(OperationalBlocker::ControlBacklog);
        }
        if barrier
            .try_get::<bool, _>("quarantined")
            .map_err(|_| Error::Storage)?
        {
            reasons.push(OperationalBlocker::QuarantinedControl);
        }
        if barrier
            .try_get::<bool, _>("integrity")
            .map_err(|_| Error::Storage)?
        {
            reasons.push(OperationalBlocker::IntegrityFailure);
        }
        Ok(OperationalStatus {
            ready: reasons.is_empty(),
            writes_ready,
            hold_until,
            reasons,
        })
    }

    pub(super) async fn reserve(
        &mut self,
        resource: &str,
        id: Id,
        amount: i64,
        capacity: i64,
    ) -> Result<bool> {
        if amount < 0 || capacity < 1 || resource.len() > 64 {
            return Err(Error::InvalidInput);
        }
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,19))")
            .bind(resource)
            .execute(&mut *self.tx)
            .await
            .map_err(|_| Error::Storage)?;
        let current: Option<i64> = sqlx::query_scalar(
            "SELECT amount FROM resource_reservations WHERE resource=$1 AND reservation_id=$2",
        )
        .bind(resource)
        .bind(id)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(|_| Error::Storage)?;
        if let Some(current) = current {
            return Ok(current == amount);
        }
        let used: i64 = sqlx::query_scalar(
            "SELECT COALESCE(sum(amount),0)::bigint FROM resource_reservations WHERE resource=$1",
        )
        .bind(resource)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(|_| Error::Storage)?;
        if used
            .checked_add(amount)
            .is_none_or(|total| total > capacity)
        {
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO resource_reservations(resource,reservation_id,amount) VALUES($1,$2,$3)",
        )
        .bind(resource)
        .bind(id)
        .bind(amount)
        .execute(&mut *self.tx)
        .await
        .map_err(|_| Error::Storage)?;
        Ok(true)
    }
}
