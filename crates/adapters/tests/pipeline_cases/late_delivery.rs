use super::*;

async fn dispatching_delivery(f: &Fixture) -> (Account, Id, Id, Id, Job) {
    let (owner, plan, people) = participants(f).await;
    let (secret, codes) = secret(f, &owner, plan, &people, false).await;
    let case = inactive(f, owner.id, plan, secret).await;
    for (person, code) in codes.iter().take(3) {
        f.engine.submit_code(person.id, case, code).await.unwrap();
    }
    wait_elapsed(f, case).await;
    f.engine.tick_plan(plan).await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let job = list::<Job>(&mut *tx, Some(plan))
        .await
        .unwrap()
        .into_iter()
        .find(|job| matches!(job.task, Task::Deliver { .. }))
        .unwrap();
    tx.commit().await.unwrap();
    let job = f.engine.claim_job(job.id).await.unwrap().unwrap();
    let (recipient, _, _) = f.engine.delivery_block(&job).await.unwrap();
    assert!(
        f.engine
            .authorize_dispatch(&job, recipient.chat_id)
            .await
            .unwrap()
    );
    (owner, plan, secret, case, job)
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn routed_retry_confirmation_can_progress_while_later_stop_fences_dispatch() {
    let f = fixture().await;
    let (owner, plan, secret, _, job) = dispatching_delivery(&f).await;
    f.engine
        .finish_job(job.id, job.lease_token, SendResult::Unknown)
        .await
        .unwrap();
    let mut tx = f.db.begin().await.unwrap();
    let part = list::<DeliveryPart>(&mut *tx, Some(secret))
        .await
        .unwrap()
        .remove(0);
    let recipient: Account = get(&mut *tx, part.recipient_id).await.unwrap();
    let current: Plan = get(&mut *tx, plan).await.unwrap();
    let now = tx.now().await.unwrap();
    let action = Action {
        id: Id::new_v4(),
        actor_id: recipient.id,
        plan_id: Some(plan),
        owner_epoch: None,
        epoch: Some(current.epoch),
        name: "retry-confirmed".into(),
        target: Some(part.id),
        expires_at: now + DAY,
        used: false,
    };
    put(&mut *tx, Some(plan), &action).await.unwrap();
    tx.commit().await.unwrap();
    let retry = serde_json::json!({"update_id":701,"callback_query":{"id":"synthetic-retry","data":action.id.to_string(),"from":{"id":recipient.telegram_id,"is_bot":false},"message":{"message_id":701,"chat":{"id":recipient.chat_id,"type":"private"}}}});
    let route = f.db.route_update(&retry).await.unwrap();
    assert!(route.plans.contains(&plan));
    assert!(
        !route.protective,
        "a delivery retry must not wait on its own inbox barrier"
    );
    let event = Id::from_u128((100_u128 << 64) | 701);
    let envelope = f
        .engine
        .crypto
        .wrap(
            "telegram-inbox",
            event,
            &serde_json::to_vec(&retry).unwrap(),
        )
        .unwrap();
    f.db.ingest_routed(
        100,
        &[adapters::postgres::RoutedUpdate {
            update_id: 701,
            envelope,
            route,
        }],
    )
    .await
    .unwrap();
    let claim = f.db.claim_update(100, false).await.unwrap().unwrap();
    assert_eq!(claim.update_id, 701);
    // This is the same use case called by BotUi while its inbox lease is active.
    f.engine
        .retry_delivery(recipient.id, part.id)
        .await
        .unwrap();
    assert!(f.db.finish_update(100, &claim).await.unwrap());
    let mut tx = f.db.begin().await.unwrap();
    let retry_job = list::<Job>(&mut *tx, Some(plan))
        .await
        .unwrap()
        .into_iter()
        .find(|j| {
            j.id != job.id && matches!(j.task, Task::Deliver { part_id, .. } if part_id == part.id)
        })
        .unwrap();
    tx.commit().await.unwrap();
    let retry_job = f.engine.claim_job(retry_job.id).await.unwrap().unwrap();
    let stop = serde_json::json!({"update_id":702,"message":{"message_id":702,"from":{"id":owner.telegram_id,"is_bot":false},"chat":{"id":owner.chat_id,"type":"private"},"text":"/stop"}});
    let route = f.db.route_update(&stop).await.unwrap();
    let event = Id::from_u128((100_u128 << 64) | 702);
    let envelope = f
        .engine
        .crypto
        .wrap("telegram-inbox", event, &serde_json::to_vec(&stop).unwrap())
        .unwrap();
    f.db.ingest_routed(
        100,
        &[adapters::postgres::RoutedUpdate {
            update_id: 702,
            envelope,
            route,
        }],
    )
    .await
    .unwrap();
    sqlx::query("DELETE FROM rate_limits WHERE key LIKE 'outbound:%'")
        .execute(&f.db.pool)
        .await
        .unwrap();
    assert!(
        !f.engine
            .authorize_dispatch(&retry_job, recipient.chat_id)
            .await
            .unwrap(),
        "a later accepted STOP still fences the explicit retry before dispatch"
    );
    let mut tx = f.db.begin().await.unwrap();
    assert_eq!(
        get::<Job>(&mut *tx, retry_job.id).await.unwrap().state,
        PartState::Claimed,
        "the refusal is the protective barrier, not outbound throttling"
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn late_delivery_result_cannot_lift_secret_stop_after_checkin() {
    for (result, expected) in [
        (SendResult::Unknown, PartState::Unknown),
        (SendResult::Permanent, PartState::PermanentFailed),
        (SendResult::Sent(700), PartState::Sent),
    ] {
        let f = fixture().await;
        let (owner, plan, secret, case, job) = dispatching_delivery(&f).await;
        f.engine
            .control(
                owner.id,
                plan,
                Id::new_v4(),
                Control::StopSecret { secret_id: secret },
            )
            .await
            .unwrap();
        f.engine
            .finish_job(job.id, job.lease_token, result)
            .await
            .unwrap();
        let mut tx = f.engine.db.begin().await.unwrap();
        let secret: Secret = get(&mut *tx, secret).await.unwrap();
        assert_eq!(secret.state, SecretState::Paused);
        assert!(secret.last_case.is_none());
        assert_eq!(
            get::<Plan>(&mut *tx, plan).await.unwrap().state,
            PlanState::Active
        );
        assert_eq!(
            get::<CaseRecord>(&mut *tx, case).await.unwrap().case.state,
            domain::CaseState::Cancelled
        );
        assert_eq!(get::<Job>(&mut *tx, job.id).await.unwrap().state, expected);
        assert_eq!(
            get::<Attempt>(&mut *tx, job.lease_token)
                .await
                .unwrap()
                .state,
            expected
        );
        let parts = list::<DeliveryPart>(&mut *tx, Some(secret.id))
            .await
            .unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].state, expected);
        tx.commit().await.unwrap();

        f.engine
            .control(owner.id, plan, Id::new_v4(), Control::CheckIn)
            .await
            .unwrap();
        let mut tx = f.engine.db.begin().await.unwrap();
        assert_eq!(
            get::<Secret>(&mut *tx, secret.id).await.unwrap().state,
            SecretState::Paused
        );
        // Check-in cancels unsent work, while preserving the late result's evidence.
        assert_eq!(
            get::<Attempt>(&mut *tx, job.lease_token)
                .await
                .unwrap()
                .state,
            expected
        );
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn historical_delivery_result_cannot_change_a_new_case() {
    for result in [SendResult::Unknown, SendResult::Sent(701)] {
        let f = fixture().await;
        let (owner, plan, secret, old_case, job) = dispatching_delivery(&f).await;
        f.engine
            .control(owner.id, plan, Id::new_v4(), Control::CheckIn)
            .await
            .unwrap();
        let new_case = inactive(&f, owner.id, plan, secret).await;
        assert_ne!(old_case, new_case);
        f.engine
            .finish_job(job.id, job.lease_token, result)
            .await
            .unwrap();
        let mut tx = f.engine.db.begin().await.unwrap();
        let secret: Secret = get(&mut *tx, secret).await.unwrap();
        assert_eq!(secret.state, SecretState::Armed);
        assert_eq!(secret.last_case, Some(new_case));
        assert_eq!(
            get::<CaseRecord>(&mut *tx, new_case)
                .await
                .unwrap()
                .case
                .state,
            domain::CaseState::Collecting
        );
        assert_eq!(
            get::<CaseRecord>(&mut *tx, old_case)
                .await
                .unwrap()
                .case
                .state,
            domain::CaseState::Cancelled
        );
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn superseded_delivery_failure_cannot_downgrade_successful_retry() {
    let f = fixture().await;
    let (_, plan, secret, case, old_job) = dispatching_delivery(&f).await;
    f.engine
        .finish_job(old_job.id, old_job.lease_token, SendResult::Unknown)
        .await
        .unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let part = list::<DeliveryPart>(&mut *tx, Some(secret))
        .await
        .unwrap()
        .remove(0);
    tx.commit().await.unwrap();
    f.engine
        .retry_delivery(part.recipient_id, part.id)
        .await
        .unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let job = list::<Job>(&mut *tx, Some(plan))
        .await
        .unwrap()
        .into_iter()
        .find(|job| job.id != old_job.id && matches!(job.task, Task::Deliver { .. }))
        .unwrap();
    tx.commit().await.unwrap();
    // Pacing is unrelated to this ordering: advance its fixture state without sleeping.
    sqlx::query("DELETE FROM rate_limits WHERE key LIKE 'outbound:%'")
        .execute(&f.db.pool)
        .await
        .unwrap();
    let job = f.engine.claim_job(job.id).await.unwrap().unwrap();
    let (recipient, _, _) = f.engine.delivery_block(&job).await.unwrap();
    assert!(
        f.engine
            .authorize_dispatch(&job, recipient.chat_id)
            .await
            .unwrap()
    );
    f.engine
        .finish_job(job.id, job.lease_token, SendResult::Sent(702))
        .await
        .unwrap();
    f.engine
        .finish_job(old_job.id, old_job.lease_token, SendResult::Permanent)
        .await
        .unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    assert_eq!(
        get::<Secret>(&mut *tx, secret).await.unwrap().state,
        SecretState::Delivered
    );
    assert_eq!(
        get::<CaseRecord>(&mut *tx, case).await.unwrap().case.state,
        domain::CaseState::Complete
    );
    assert_eq!(
        get::<DeliveryPart>(&mut *tx, part.id).await.unwrap().state,
        PartState::Sent
    );
    assert_eq!(
        get::<Attempt>(&mut *tx, old_job.lease_token)
            .await
            .unwrap()
            .state,
        PartState::PermanentFailed
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn late_delivery_result_cannot_recreate_deleted_secret() {
    let f = fixture().await;
    let (owner, plan, secret, case, job) = dispatching_delivery(&f).await;
    f.engine
        .control(
            owner.id,
            plan,
            Id::new_v4(),
            Control::DeleteSecret { secret_id: secret },
        )
        .await
        .unwrap();
    f.engine
        .finish_job(job.id, job.lease_token, SendResult::Sent(703))
        .await
        .unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    assert!(tx.get(Kind::Secret, secret).await.unwrap().is_none());
    assert!(tx.get(Kind::Case, case).await.unwrap().is_none());
    assert!(tx.get(Kind::Job, job.id).await.unwrap().is_none());
    assert!(
        tx.get(Kind::DeletionTombstone, secret)
            .await
            .unwrap()
            .is_some()
    );
}
