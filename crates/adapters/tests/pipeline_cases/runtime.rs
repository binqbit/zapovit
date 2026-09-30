use super::*;
use adapters::postgres::RoutedUpdate;
use serde_json::{Value, json};
use std::time::Duration;

async fn routed(f: &Fixture, id: i64, user: i64, text: &str) -> RoutedUpdate {
    let update = json!({"update_id":id,"message":{"message_id":id,"from":{"id":user,"is_bot":false},"chat":{"id":user,"type":"private"},"text":text}});
    let mut route = f.db.route_update(&update).await.unwrap();
    f.db.authenticate_recovery_route(&update, &mut route, &f.engine)
        .await
        .unwrap();
    let event = Id::from_u128((100_u128 << 64) | id as u128);
    RoutedUpdate {
        update_id: id,
        envelope: f
            .engine
            .crypto
            .wrap(
                "telegram-inbox",
                event,
                &serde_json::to_vec(&update).unwrap(),
            )
            .unwrap(),
        route,
    }
}
async fn status(f: &Fixture, plan: Id) -> OperationalStatus {
    let mut tx = f.db.begin().await.unwrap();
    tx.operational_status(Some(plan)).await.unwrap()
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_ordinary_traffic_and_untrusted_stop_do_not_block_other_plans() {
    let f = fixture().await;
    let owner = f.engine.account(8101, 8101, "en").await.unwrap();
    let other = f.engine.account(8102, 8102, "en").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    let unrelated = f.engine.create_profile(other.id).await.unwrap();
    let ordinary = routed(&f, 100, 8101, "/start").await;
    let outsider = routed(&f, 101, 9999, "/stop").await;
    f.db.ingest_routed(100, &[ordinary, outsider])
        .await
        .unwrap();
    assert!(status(&f, plan).await.ready);
    assert!(status(&f, unrelated).await.ready);
    let stop = routed(&f, 102, 8101, "/stop").await;
    f.db.ingest_routed(100, &[stop]).await.unwrap();
    assert!(
        status(&f, plan)
            .await
            .reasons
            .contains(&OperationalBlocker::ControlBacklog)
    );
    assert!(status(&f, unrelated).await.ready);
    // An authenticated owner's unresolved control is retained, not discarded to
    // unblock the queue; after five failures only its own plan remains held.
    for _ in 0..5 {
        let claim = f.db.claim_update(100, true).await.unwrap().unwrap();
        assert_eq!(claim.update_id, 102);
        f.db.fail_update(100, &claim, &Error::Storage)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE telegram_inbox SET next_attempt_at=clock_timestamp() WHERE update_id=102",
        )
        .execute(&f.db.pool)
        .await
        .unwrap();
    }
    assert!(f.db.claim_update(100, true).await.unwrap().is_none());
    assert!(
        status(&f, plan)
            .await
            .reasons
            .contains(&OperationalBlocker::QuarantinedControl)
    );
    assert!(status(&f, unrelated).await.ready);
    let retained:bool=sqlx::query_scalar("SELECT processed_at IS NULL AND envelope IS NOT NULL FROM telegram_inbox WHERE update_id=102").fetch_one(&f.db.pool).await.unwrap();
    assert!(retained);
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_claims_preserve_actor_order_and_let_independent_lanes_progress() {
    let f = fixture().await;
    let a = routed(&f, 200, 8201, "/start").await;
    let b = routed(&f, 201, 8201, "/settings").await;
    let c = routed(&f, 202, 8202, "/start").await;
    f.db.ingest_routed(100, &[a, b, c]).await.unwrap();
    let first = f.db.claim_update(100, false).await.unwrap().unwrap();
    assert_eq!(first.update_id, 200);
    let other = f.db.claim_update(100, false).await.unwrap().unwrap();
    assert_eq!(other.update_id, 202);
    assert!(f.db.claim_update(100, false).await.unwrap().is_none());
    assert!(f.db.finish_update(100, &first).await.unwrap());
    assert_eq!(
        f.db.claim_update(100, false)
            .await
            .unwrap()
            .unwrap()
            .update_id,
        201
    );
    assert!(
        !f.db.finish_update(100, &first).await.unwrap(),
        "completed lease cannot complete another update"
    );
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_plan_lanes_serialize_different_participants() {
    let f = fixture().await;
    let (_, _, people) = participants(&f).await;
    let a = routed(&f, 210, people[0].telegram_id, "/guardians").await;
    let b = routed(&f, 211, people[1].telegram_id, "/guardians").await;
    f.db.ingest_routed(100, &[a, b]).await.unwrap();
    let first = f.db.claim_update(100, false).await.unwrap().unwrap();
    assert!(f.db.claim_update(100, false).await.unwrap().is_none());
    f.db.finish_update(100, &first).await.unwrap();
    assert_eq!(
        f.db.claim_update(100, false)
            .await
            .unwrap()
            .unwrap()
            .update_id,
        211
    );
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_dispatch_watermark_serializes_ingress_commit() {
    let f = fixture().await;
    let owner = f.engine.account(8301, 8301, "en").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    let stop = routed(&f, 300, 8301, "/stop").await;
    let mut dispatch = f.db.begin().await.unwrap();
    assert!(dispatch.operational_status(Some(plan)).await.unwrap().ready);
    let db = f.db.clone();
    let ingress = tokio::spawn(async move { db.ingest_routed(100, &[stop]).await });
    tokio::time::timeout(Duration::from_secs(1),async {
        loop {
            let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE cardinality(pg_blocking_pids(pid))>0 AND query LIKE 'SELECT bot_id FROM telegram_cursor%')").fetch_one(&f.db.pool).await.unwrap();
            if blocked {break;}
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.expect("ingress waits for dispatch watermark transaction");
    assert!(!ingress.is_finished());
    dispatch.commit().await.unwrap();
    ingress.await.unwrap().unwrap();
    assert!(
        !status(&f, plan).await.ready,
        "the next dispatch observes the accepted STOP"
    );
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_storage_reservations_are_atomic_idempotent_and_released_explicitly() {
    let f = fixture().await;
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let db = f.db.clone();
        tasks.push(tokio::spawn(async move {
            let id = Id::new_v4();
            let mut tx = db.begin().await.unwrap();
            let allowed = tx
                .reserve_resource("test-bytes", id, 80, 100)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            (id, allowed)
        }));
    }
    let a = tasks.remove(0).await.unwrap();
    let b = tasks.remove(0).await.unwrap();
    assert_ne!(a.1, b.1);
    let winner = if a.1 { a.0 } else { b.0 };
    let mut tx = f.db.begin().await.unwrap();
    assert!(
        tx.reserve_resource("test-bytes", winner, 80, 100)
            .await
            .unwrap()
    );
    assert!(
        !tx.reserve_resource("test-bytes", winner, 81, 100)
            .await
            .unwrap()
    );
    tx.release_resource("test-bytes", winner).await.unwrap();
    assert!(
        tx.reserve_resource("test-bytes", Id::new_v4(), 100, 100)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_keyset_scan_and_cleanup_support_more_than_ten_thousand_records() {
    let f = fixture().await;
    sqlx::query("INSERT INTO actions(id,data) SELECT md5(n::text)::uuid,jsonb_build_object('id',md5(n::text)::uuid,'actor_id','00000000-0000-0000-0000-000000000001','plan_id',NULL,'owner_epoch',NULL,'epoch',NULL,'name','home','target',NULL,'expires_at',0,'used',true) FROM generate_series(1,10050) n").execute(&f.db.pool).await.unwrap();
    let mut after = None;
    let mut count = 0;
    loop {
        let mut tx = f.db.begin().await.unwrap();
        let rows = tx.page(Kind::Action, None, after, 1000).await.unwrap();
        tx.commit().await.unwrap();
        if rows.is_empty() {
            break;
        }
        count += rows.len();
        after = Some(Id::parse_str(rows.last().unwrap()["id"].as_str().unwrap()).unwrap());
    }
    assert_eq!(count, 10050);
    f.db.retain_runtime_history().await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM actions")
        .fetch_one(&f.db.pool)
        .await
        .unwrap();
    assert_eq!(left, 9050, "retention work is bounded per pass");
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_readiness_checks_fresh_heartbeats_and_malformed_input_has_no_scope() {
    let f = fixture().await;
    assert!(f.db.runtime_ready().await.unwrap());
    for update in [
        json!({"message":{"from":{"id":123},"chat":{"id":123,"type":"group"},"text":"/stop"}}),
        json!({"message":{"from":{"id":123},"chat":{"id":999,"type":"private"},"text":"/stop"}}),
        Value::Null,
    ] {
        let route = f.db.route_update(&update).await.unwrap();
        assert!(route.actor_id.is_none());
        assert!(route.plans.is_empty());
        assert!(!route.protective);
    }
    sqlx::query(
        "UPDATE telegram_cursor SET last_scheduler_at=clock_timestamp()-interval '121 seconds'",
    )
    .execute(&f.db.pool)
    .await
    .unwrap();
    assert!(!f.db.runtime_ready().await.unwrap());
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_arrival_order_survives_telegram_update_id_reset() {
    let f = fixture().await;
    let first = routed(&f, 8000, 8401, "/start").await;
    f.db.ingest_routed(100, &[first]).await.unwrap();
    let reset = routed(&f, 8, 8401, "/settings").await;
    f.db.ingest_routed(100, &[reset]).await.unwrap();
    let claimed = f.db.claim_update(100, false).await.unwrap().unwrap();
    assert_eq!(claimed.update_id, 8000);
    f.db.finish_update(100, &claimed).await.unwrap();
    assert_eq!(
        f.db.claim_update(100, false)
            .await
            .unwrap()
            .unwrap()
            .update_id,
        8
    );
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_full_ordinary_inbox_accepts_stop_and_advances_past_rejected_traffic() {
    let f = fixture().await;
    let owner = f.engine.account(8501, 8501, "en").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    sqlx::query("INSERT INTO telegram_inbox(bot_id,update_id,envelope,actor_id,routed,protective) SELECT 100,n,'{}',md5(n::text)::uuid,true,false FROM generate_series(1,50000) n").execute(&f.db.pool).await.unwrap();
    let ordinary = routed(&f, 60001, 8502, "/start").await;
    let stop = routed(&f, 60002, 8501, "/stop").await;
    let rejected = f.db.ingest_routed(100, &[ordinary, stop]).await.unwrap();
    assert_eq!(rejected.consumed, 2);
    assert_eq!(rejected.rejected, vec![Id::from_u128(8502)]);
    assert_eq!(f.db.cursor().await.unwrap(), 60003);
    let claim = f.db.claim_update(100, true).await.unwrap().unwrap();
    assert_eq!(claim.update_id, 60002);
    assert!(!status(&f, plan).await.ready);
    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM telegram_inbox WHERE update_id=60001")
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    assert_eq!(
        stored, 0,
        "rejected traffic has no application side effects or retained ciphertext"
    );
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_recovery_requires_proof_but_has_reserved_account_admission() {
    let f = fixture().await;
    let owner = f.engine.account(8601, 8601, "en").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    let mut tx = f.db.begin().await.unwrap();
    let job = list::<Job>(&mut *tx, Some(plan))
        .await
        .unwrap()
        .into_iter()
        .find(|j| matches!(j.task, Task::RecoveryCode { .. }))
        .unwrap();
    tx.commit().await.unwrap();
    let Task::RecoveryCode {
        envelope, selector, ..
    } = job.task
    else {
        panic!()
    };
    let token = f
        .engine
        .crypto
        .unwrap("recovery", selector, &envelope)
        .unwrap();
    let token = std::str::from_utf8(&token).unwrap();
    sqlx::query("INSERT INTO accounts(id,data) SELECT md5(n::text)::uuid,jsonb_build_object('id',md5(n::text)::uuid,'telegram_id',n+1000000,'chat_id',n+1000000,'locale','en') FROM generate_series(1,99999) n").execute(&f.db.pool).await.unwrap();
    assert!(matches!(
        f.engine.account(8602, 8602, "en").await,
        Err(Error::RateLimited)
    ));
    assert!(
        f.engine
            .account_for_recovery(8602, 8602, "en", selector, "invalid-key")
            .await
            .is_err()
    );
    let mut tx = f.db.begin().await.unwrap();
    assert!(
        tx.get(Kind::Account, Id::from_u128(8602))
            .await
            .unwrap()
            .is_none()
    );
    tx.commit().await.unwrap();
    let recovered = f
        .engine
        .account_for_recovery(8602, 8602, "en", selector, token)
        .await
        .unwrap();
    assert_eq!(recovered.id, Id::from_u128(8602));
    assert!(
        matches!(
            f.engine
                .account_for_recovery(8603, 8603, "en", selector, token)
                .await,
            Err(Error::RateLimited)
        ),
        "one credential cannot admit unlimited identities"
    );
    let pending = routed(&f, 70000, 8602, &format!("/recoverstop {token}")).await;
    assert!(pending.route.verified_recovery.is_some());
    f.db.ingest_routed(100, &[pending]).await.unwrap();
    assert!(
        !status(&f, plan).await.ready,
        "single-message credential recovery establishes its plan barrier"
    );
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_full_normal_outbox_preserves_reserved_protective_feedback() {
    let f = fixture().await;
    sqlx::query("INSERT INTO outbox(id,data) SELECT md5(n::text)::uuid,jsonb_build_object('id',md5(n::text)::uuid,'state','queued','priority',5,'task',jsonb_build_object('kind','notice')) FROM generate_series(1,100000) n").execute(&f.db.pool).await.unwrap();
    let task = Task::Notice {
        account_id: Id::from_u128(1),
        key: "stopped".into(),
        buttons: vec![],
    };
    let mut tx = f.db.begin().await.unwrap();
    assert!(matches!(
        enqueue(&mut *tx, None, task.clone(), 0, 60, 5).await,
        Err(Error::RateLimited)
    ));
    drop(tx);
    let mut tx = f.db.begin().await.unwrap();
    let id = enqueue(&mut *tx, None, task, 0, 60, 0).await.unwrap();
    enqueue(
        &mut *tx,
        None,
        Task::CleanupMessage {
            chat_id: 1,
            message_id: 2,
            sent_at: 0,
            account_id: Id::from_u128(1),
        },
        0,
        60,
        1,
    )
    .await
    .expect("deletion cleanup uses reserved capacity");
    enqueue(
        &mut *tx,
        None,
        Task::DeleteObject {
            object_id: Id::new_v4(),
        },
        0,
        60,
        1,
    )
    .await
    .expect("physical cleanup uses reserved capacity");
    tx.commit().await.unwrap();
    let mut tx = f.db.begin().await.unwrap();
    assert_eq!(get::<Job>(&mut *tx, id).await.unwrap().priority, 0);
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_payload_budget_rejects_growth_and_releases_expired_drafts() {
    let f = fixture().await;
    let (owner, plan, _) = participants(&f).await;
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    let mut tx = f.db.begin().await.unwrap();
    let current: Draft = get(&mut *tx, draft).await.unwrap();
    let size = serde_json::to_vec(&current.payload).unwrap().len() as i64;
    assert!(
        tx.reserve_resource(
            "payload/drafts",
            Id::new_v4(),
            512 * 1024 * 1024 - size,
            512 * 1024 * 1024
        )
        .await
        .unwrap()
    );
    tx.commit().await.unwrap();
    assert!(matches!(
        f.engine
            .append_block(
                owner.id,
                draft,
                Block::Text {
                    text: "a larger encrypted message".into()
                },
                (owner.chat_id, 42)
            )
            .await,
        Err(Error::RateLimited)
    ));
    assert!(
        f.engine
            .draft_blocks(owner.id, draft)
            .await
            .unwrap()
            .is_empty()
    );
    sqlx::query("UPDATE drafts SET data=jsonb_set(data,'{expires_at}','0') WHERE id=$1")
        .bind(draft)
        .execute(&f.db.pool)
        .await
        .unwrap();
    f.engine.cleanup_plan(plan).await.unwrap();
    let reserved:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM resource_reservations WHERE resource='payload/drafts' AND reservation_id=$1)").bind(draft).fetch_one(&f.db.pool).await.unwrap();
    assert!(
        !reserved,
        "deletion releases capacity without requiring more admission"
    );
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_expired_worker_cannot_complete_reclaimed_event() {
    let f = fixture().await;
    let event = routed(&f, 80001, 8701, "/start").await;
    f.db.ingest_routed(100, &[event]).await.unwrap();
    let old = f.db.claim_update(100, false).await.unwrap().unwrap();
    sqlx::query("UPDATE telegram_inbox SET lease_until=clock_timestamp()-interval '1 second' WHERE update_id=80001").execute(&f.db.pool).await.unwrap();
    let current = f.db.claim_update(100, false).await.unwrap().unwrap();
    assert_ne!(old.lease_token, current.lease_token);
    assert!(!f.db.finish_update(100, &old).await.unwrap());
    f.db.fail_update(100, &old, &Error::Crypto).await.unwrap();
    assert!(f.db.finish_update(100, &current).await.unwrap());
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_scheduler_pages_past_failed_plans_and_removes_deleted_plans() {
    let f = fixture().await;
    sqlx::query("INSERT INTO runtime_plan_schedule(plan_id) SELECT md5(n::text)::uuid FROM generate_series(1,10050) n").execute(&f.db.pool).await.unwrap();
    let first = f.db.due_plans().await.unwrap();
    assert_eq!(first.len(), 25);
    for id in &first {
        f.db.plan_scheduled(*id, Some(&Error::Storage))
            .await
            .unwrap();
    }
    let next = f.db.due_plans().await.unwrap();
    assert_eq!(next.len(), 25);
    assert!(
        next.iter().all(|id| !first.contains(id)),
        "failed plans do not occupy the first scheduler page repeatedly"
    );
    let owner = f.engine.account(8801, 8801, "en").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    let mut tx = f.db.begin().await.unwrap();
    tx.remove(Kind::Plan, plan).await.unwrap();
    tx.commit().await.unwrap();
    let scheduled: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runtime_plan_schedule WHERE plan_id=$1)")
            .bind(plan)
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    assert!(!scheduled);
    let metrics = f.db.metrics().await.unwrap();
    assert!(metrics.contains("zapovit_scheduler_lag_seconds "));
    assert!(metrics.contains("zapovit_backup_age_seconds -1"));
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_dispatch_waits_for_and_observes_ingress_transaction() {
    let f = fixture().await;
    let owner = f.engine.account(8901, 8901, "en").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    let mut ingress = f.db.pool.begin().await.unwrap();
    sqlx::query("SELECT bot_id FROM telegram_cursor WHERE singleton FOR UPDATE")
        .execute(&mut *ingress)
        .await
        .unwrap();
    let db = f.db.clone();
    let dispatch = tokio::spawn(async move {
        let mut tx = db.begin().await.unwrap();
        tx.operational_status(Some(plan)).await.unwrap()
    });
    tokio::time::timeout(Duration::from_secs(1),async {
        loop {
            let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE cardinality(pg_blocking_pids(pid))>0 AND query LIKE 'SELECT last_poll_at%')").fetch_one(&f.db.pool).await.unwrap();
            if blocked { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.expect("dispatch waits for the accepting ingress transaction");
    sqlx::query("INSERT INTO telegram_inbox(bot_id,update_id,envelope,actor_id,routed,protective) VALUES(100,90000,'{}',$1,true,true)").bind(owner.id).execute(&mut *ingress).await.unwrap();
    sqlx::query("INSERT INTO telegram_inbox_scopes(bot_id,update_id,plan_id,protective) VALUES(100,90000,$1,true)").bind(plan).execute(&mut *ingress).await.unwrap();
    ingress.commit().await.unwrap();
    let status = dispatch.await.unwrap();
    assert!(!status.ready);
    assert!(status.reasons.contains(&OperationalBlocker::ControlBacklog));
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_duplicate_owner_stops_coalesce_only_without_intervening_events() {
    let f = fixture().await;
    let owner = f.engine.account(9001, 9001, "en").await.unwrap();
    f.engine.create_profile(owner.id).await.unwrap();
    let mut repeated = Vec::new();
    for id in 91000..91020 {
        repeated.push(routed(&f, id, 9001, "/stop").await);
    }
    f.db.ingest_routed(100, &repeated).await.unwrap();
    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM telegram_inbox WHERE processed_at IS NULL")
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    assert_eq!(
        pending, 1,
        "identical unclaimed tail controls share the first accepted operation"
    );
    let checkin = routed(&f, 91020, 9001, "/checkin").await;
    let stop = routed(&f, 91021, 9001, "/stop").await;
    f.db.ingest_routed(100, &[checkin, stop]).await.unwrap();
    let first = f.db.claim_update(100, true).await.unwrap().unwrap();
    assert_eq!(first.update_id, 91000);
    f.db.finish_update(100, &first).await.unwrap();
    let second = f.db.claim_update(100, true).await.unwrap().unwrap();
    assert_eq!(
        second.update_id, 91020,
        "distinct protective controls keep their order"
    );
    f.db.finish_update(100, &second).await.unwrap();
    assert_eq!(
        f.db.claim_update(100, true)
            .await
            .unwrap()
            .unwrap()
            .update_id,
        91021
    );
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_unverified_recovery_selector_cannot_hold_target_plan() {
    let f = fixture().await;
    let owner = f.engine.account(9101, 9101, "en").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    let (profile, _) = f.engine.own_plan(owner.id).await.unwrap();
    let invalid = format!(
        "/recoverstop R1.{}.{}",
        profile.recovery_selector,
        URL_SAFE_NO_PAD.encode([0_u8; 32])
    );
    let event = routed(&f, 92000, 9102, &invalid).await;
    assert!(event.route.verified_recovery.is_none());
    assert!(!event.route.protective);
    f.db.ingest_routed(100, &[event]).await.unwrap();
    assert!(status(&f, plan).await.ready);
    let claim = f.db.claim_update(100, false).await.unwrap().unwrap();
    f.db.fail_update(100, &claim, &Error::Crypto).await.unwrap();
    assert!(
        status(&f, plan).await.ready,
        "even quarantine of an unverified attempt has no target authority"
    );
    sqlx::query("UPDATE rate_limits SET used=20 WHERE key='admission:recovery-routing'")
        .execute(&f.db.pool)
        .await
        .unwrap();
    let rejected = routed(&f, 92001, 9102, &invalid).await;
    assert!(rejected.route.admission_rejected);
    assert_eq!(
        f.db.ingest_routed(100, &[rejected]).await.unwrap().rejected,
        vec![Id::from_u128(9102)]
    );
    let mut tx = f.db.begin().await.unwrap();
    let cleanup = get::<Job>(&mut *tx, Id::from_u128((100_u128 << 64) | 92001))
        .await
        .unwrap();
    assert!(matches!(
        cleanup.task,
        Task::CleanupMessage {
            chat_id: 9102,
            message_id: 92001,
            ..
        }
    ));
    assert!(
        tx.get(Kind::Account, Id::from_u128(9102))
            .await
            .unwrap()
            .is_none(),
        "cleanup does not admit unknown accounts"
    );
    tx.commit().await.unwrap();
    let retained: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM telegram_inbox WHERE update_id=92001)")
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    assert!(
        !retained,
        "rejected credentials retain only bounded cleanup identifiers"
    );
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_old_owner_callback_cannot_hold_transferred_plan() {
    let f = fixture().await;
    let owner = f.engine.account(9301, 9301, "en").await.unwrap();
    let replacement = f.engine.account(9302, 9302, "en").await.unwrap();
    let plan_id = f.engine.create_profile(owner.id).await.unwrap();
    let (mut profile, plan) = f.engine.own_plan(owner.id).await.unwrap();
    let mut tx = f.db.begin().await.unwrap();
    let action = Action {
        id: Id::new_v4(),
        actor_id: owner.id,
        plan_id: Some(plan_id),
        owner_epoch: Some(profile.owner_epoch),
        epoch: Some(plan.epoch),
        name: "stop".into(),
        target: None,
        expires_at: tx.now().await.unwrap() + 600,
        used: false,
    };
    put(&mut *tx, Some(plan_id), &action).await.unwrap();
    profile.owner_id = replacement.id;
    profile.owner_epoch += 1;
    put(&mut *tx, Some(replacement.id), &profile).await.unwrap();
    tx.commit().await.unwrap();
    let update = json!({"update_id":93000,"callback_query":{"id":"old-owner-callback","from":{"id":9301,"is_bot":false},"data":action.id.to_string(),"message":{"message_id":1,"chat":{"id":9301,"type":"private"}}}});
    let route = f.db.route_update(&update).await.unwrap();
    assert!(!route.protective);
    assert!(!route.priority);
    let envelope = f
        .engine
        .crypto
        .wrap(
            "telegram-inbox",
            Id::from_u128((100_u128 << 64) | 93000),
            &serde_json::to_vec(&update).unwrap(),
        )
        .unwrap();
    f.db.ingest_routed(
        100,
        &[RoutedUpdate {
            update_id: 93000,
            envelope,
            route,
        }],
    )
    .await
    .unwrap();
    assert!(status(&f, plan_id).await.ready);
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_full_navigation_budget_keeps_recovery_acknowledgement_headroom() {
    let f = fixture().await;
    sqlx::query("INSERT INTO actions(id,data) SELECT md5(n::text)::uuid,jsonb_build_object('id',md5(n::text)::uuid,'name','home') FROM generate_series(1,250000) n").execute(&f.db.pool).await.unwrap();
    let mut action = Action {
        id: Id::new_v4(),
        actor_id: Id::from_u128(1),
        plan_id: None,
        owner_epoch: None,
        epoch: None,
        name: "home".into(),
        target: None,
        expires_at: i64::MAX,
        used: false,
    };
    let mut tx = f.db.begin().await.unwrap();
    assert!(matches!(
        put(&mut *tx, None, &action).await,
        Err(Error::RateLimited)
    ));
    drop(tx);
    action.name = "ack-claim".into();
    let mut tx = f.db.begin().await.unwrap();
    put(&mut *tx, None, &action).await.unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_full_protective_reserve_commits_only_prefix_and_retries_without_loss() {
    let f = fixture().await;
    let owner = f.engine.account(9501, 9501, "en").await.unwrap();
    let replacement = f.engine.account(9502, 9502, "en").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    sqlx::query("INSERT INTO telegram_inbox(bot_id,update_id,envelope,actor_id,routed,protective,quarantined_at) SELECT 100,n,'{}',md5(n::text)::uuid,true,true,CASE WHEN n=100000 THEN clock_timestamp() ELSE NULL END FROM generate_series(1,100000) n").execute(&f.db.pool).await.unwrap();
    let batch = [
        routed(&f, 200001, 9503, "/start").await,
        routed(&f, 200002, 9504, "/stop").await,
        routed(&f, 200003, 9501, "/stop").await,
        routed(&f, 200004, 9505, "/start").await,
    ];
    let first = f.db.ingest_routed(100, &batch).await.unwrap();
    assert_eq!(
        first.consumed, 2,
        "unknown input rejects without consuming the protected reserve"
    );
    assert_eq!(
        first.rejected,
        vec![Id::from_u128(9503), Id::from_u128(9504)]
    );
    assert_eq!(
        f.db.cursor().await.unwrap(),
        200003,
        "offset never advances past the first unaccepted control"
    );
    let suffix_stored: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM telegram_inbox WHERE update_id>=200003)")
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    assert!(!suffix_stored);
    sqlx::query("UPDATE telegram_cursor SET last_poll_at=clock_timestamp()-interval '121 seconds'")
        .execute(&f.db.pool)
        .await
        .unwrap();
    let still_full = f.db.retry_ingest_routed(100, &batch[2..]).await.unwrap();
    assert_eq!(still_full.consumed, 0);
    assert!(
        still_full.rejected.is_empty(),
        "a deferred control is never reported as rejected"
    );
    assert_eq!(f.db.cursor().await.unwrap(), 200003);
    assert!(
        !f.db.runtime_ready().await.unwrap(),
        "local retries cannot fake a fresh Telegram poll"
    );
    assert!(
        status(&f, plan)
            .await
            .reasons
            .contains(&OperationalBlocker::Hold)
    );
    let retained:bool=sqlx::query_scalar("SELECT quarantined_at IS NOT NULL AND envelope IS NOT NULL AND processed_at IS NULL FROM telegram_inbox WHERE update_id=100000").fetch_one(&f.db.pool).await.unwrap();
    assert!(
        retained,
        "quarantined controls count toward capacity and are never evicted"
    );
    sqlx::query(
        "UPDATE telegram_inbox SET processed_at=clock_timestamp(),envelope=NULL WHERE update_id=1",
    )
    .execute(&f.db.pool)
    .await
    .unwrap();
    let resumed = f.db.retry_ingest_routed(100, &batch[2..]).await.unwrap();
    assert_eq!(resumed.consumed, 2);
    assert_eq!(resumed.rejected, vec![Id::from_u128(9505)]);
    assert_eq!(f.db.cursor().await.unwrap(), 200005);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM telegram_inbox WHERE processed_at IS NULL")
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    assert_eq!(count, 100000);
    let stored: Value =
        sqlx::query_scalar("SELECT envelope FROM telegram_inbox WHERE update_id=200003")
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    assert_eq!(
        stored,
        serde_json::to_value(&batch[2].envelope).unwrap(),
        "the original encrypted operation is retained unchanged"
    );
    let duplicate = routed(&f, 200005, 9501, "/stop").await;
    assert_eq!(
        f.db.retry_ingest_routed(100, &[duplicate])
            .await
            .unwrap()
            .consumed,
        1,
        "tail coalescing precedes reserve rejection"
    );
    assert_eq!(f.db.cursor().await.unwrap(), 200006);
    let cached_stop = routed(&f, 200006, 9501, "/stop").await;
    let (mut profile, record) = f.engine.own_plan(owner.id).await.unwrap();
    let mut tx = f.db.begin().await.unwrap();
    let action = Action {
        id: Id::new_v4(),
        actor_id: owner.id,
        plan_id: Some(plan),
        owner_epoch: Some(profile.owner_epoch),
        epoch: Some(record.epoch),
        name: "stop".into(),
        target: None,
        expires_at: tx.now().await.unwrap() + 600,
        used: false,
    };
    put(&mut *tx, Some(plan), &action).await.unwrap();
    tx.commit().await.unwrap();
    let update = json!({"update_id":200007,"callback_query":{"id":"cached-stop","from":{"id":9501,"is_bot":false},"data":action.id.to_string(),"message":{"message_id":1,"chat":{"id":9501,"type":"private"}}}});
    let route = f.db.route_update(&update).await.unwrap();
    assert!(route.priority && route.protective);
    let envelope = f
        .engine
        .crypto
        .wrap(
            "telegram-inbox",
            Id::from_u128((100_u128 << 64) | 200007),
            &serde_json::to_vec(&update).unwrap(),
        )
        .unwrap();
    profile.owner_id = replacement.id;
    profile.owner_epoch += 1;
    let mut tx = f.db.begin().await.unwrap();
    put(&mut *tx, Some(replacement.id), &profile).await.unwrap();
    tx.commit().await.unwrap();
    let obsolete =
        f.db.retry_ingest_routed(
            100,
            &[
                cached_stop,
                RoutedUpdate {
                    update_id: 200007,
                    envelope,
                    route,
                },
            ],
        )
        .await
        .unwrap();
    assert_eq!(obsolete.consumed, 2);
    assert_eq!(
        obsolete.rejected,
        vec![owner.id],
        "cached former-owner routes cannot exhaust the reserve"
    );
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn runtime_deferred_recovery_rechecks_selector_without_spending_another_argon_token() {
    let f = fixture().await;
    let owner = f.engine.account(9601, 9601, "en").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    let (mut profile, _) = f.engine.own_plan(owner.id).await.unwrap();
    let mut tx = f.db.begin().await.unwrap();
    let job = list::<Job>(&mut *tx, Some(plan))
        .await
        .unwrap()
        .into_iter()
        .find(|job| matches!(job.task, Task::RecoveryCode { .. }))
        .unwrap();
    tx.commit().await.unwrap();
    let Task::RecoveryCode {
        envelope, selector, ..
    } = job.task
    else {
        panic!()
    };
    let token = f
        .engine
        .crypto
        .unwrap("recovery", selector, &envelope)
        .unwrap();
    let input = format!("/recoverstop {}", std::str::from_utf8(&token).unwrap());
    let deferred = [routed(&f, 210001, 9602, &input).await];
    assert!(deferred[0].route.verified_recovery.is_some());
    sqlx::query("INSERT INTO telegram_inbox(bot_id,update_id,envelope,actor_id,routed,protective) SELECT 100,n,'{}',md5(n::text)::uuid,true,true FROM generate_series(1,100000) n").execute(&f.db.pool).await.unwrap();
    assert_eq!(
        f.db.ingest_routed(100, &deferred).await.unwrap().consumed,
        0
    );
    let offset = f.db.cursor().await.unwrap();
    assert_eq!(
        f.db.retry_ingest_routed(100, &deferred)
            .await
            .unwrap()
            .consumed,
        0
    );
    assert_eq!(
        f.db.cursor().await.unwrap(),
        offset,
        "without a safe prefix there is no cursor advance"
    );
    let attempts: i64 =
        sqlx::query_scalar("SELECT used FROM rate_limits WHERE key='admission:recovery-routing'")
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    assert_eq!(attempts, 1);
    profile.recovery_selector = Id::new_v4();
    let mut tx = f.db.begin().await.unwrap();
    put(&mut *tx, Some(owner.id), &profile).await.unwrap();
    tx.commit().await.unwrap();
    let rotated = f.db.retry_ingest_routed(100, &deferred).await.unwrap();
    assert_eq!(rotated.consumed, 1);
    assert_eq!(
        rotated.rejected,
        vec![Id::from_u128(9602)],
        "an invalidated proof no longer occupies protected capacity"
    );
    let scoped: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM telegram_inbox_scopes WHERE update_id=210001)",
    )
    .fetch_one(&f.db.pool)
    .await
    .unwrap();
    assert!(!scoped);
}
