use super::*;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn unready_rearm_is_not_journaled_and_stale_snapshot_replay_stays_paused() {
    let f = fixture().await;
    let owner = f.engine.account(5001, 5001, "en").await.unwrap();
    let plan_id = f.engine.create_profile(owner.id).await.unwrap();
    let (snapshot_profile, snapshot_plan) = f.engine.own_plan(owner.id).await.unwrap();
    assert!(
        f.engine
            .control(owner.id, plan_id, Id::new_v4(), Control::Rearm)
            .await
            .is_err()
    );
    assert!(f.engine.journal.read().await.unwrap().is_empty());
    let (_, plan) = f.engine.own_plan(owner.id).await.unwrap();
    assert_eq!(plan.state, PlanState::Setup);
    assert!(plan.pending_control.is_none());

    f.engine
        .acknowledge_recovery(owner.id, plan_id, snapshot_profile.recovery_selector)
        .await
        .unwrap();
    // Saving recovery alone no longer activates an empty plan.
    assert!(
        f.engine
            .control(owner.id, plan_id, Id::new_v4(), Control::Rearm)
            .await
            .is_err()
    );
    assert!(f.engine.journal.read().await.unwrap().is_empty());
    let people = confirmed_people(&f, &owner, plan_id).await;
    secret(&f, &owner, plan_id, &people, false).await;
    let operation = Id::new_v4();
    f.engine
        .control(owner.id, plan_id, operation, Control::Rearm)
        .await
        .unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    put(&mut *tx, Some(owner.id), &snapshot_profile)
        .await
        .unwrap();
    put(&mut *tx, Some(snapshot_profile.id), &snapshot_plan)
        .await
        .unwrap();
    tx.remove(Kind::ControlIntent, operation).await.unwrap();
    tx.commit().await.unwrap();
    f.engine.replay_controls().await.unwrap();
    let (_, plan) = f.engine.own_plan(owner.id).await.unwrap();
    assert_eq!(plan.state, PlanState::Paused);
    assert!(plan.pending_control.is_none());
    f.engine.replay_controls().await.unwrap();
}

async fn prepared_delivery(f: &Fixture) -> (Account, Id, Id, Id, Job) {
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
        .find(|j| matches!(j.task, Task::Deliver { .. }))
        .unwrap();
    tx.commit().await.unwrap();
    (owner, plan, secret, case, job)
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn expired_historical_delivery_cannot_lift_secret_stop_or_poison_new_case() {
    let f = fixture().await;
    let (owner, plan, secret, case, _) = prepared_delivery(&f).await;
    let mut tx = f.engine.db.begin().await.unwrap();
    let mut c: CaseRecord = get(&mut *tx, case).await.unwrap();
    c.started_delivery = Some(tx.now().await.unwrap() - 8 * DAY);
    put(&mut *tx, Some(secret), &c).await.unwrap();
    tx.commit().await.unwrap();

    f.engine
        .control(
            owner.id,
            plan,
            Id::new_v4(),
            Control::StopSecret { secret_id: secret },
        )
        .await
        .unwrap();
    f.engine.cleanup_plan(plan).await.unwrap();
    f.engine
        .control(owner.id, plan, Id::new_v4(), Control::CheckIn)
        .await
        .unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    assert_eq!(
        get::<Secret>(&mut *tx, secret).await.unwrap().state,
        SecretState::Paused
    );
    assert_eq!(
        get::<CaseRecord>(&mut *tx, case).await.unwrap().case.state,
        domain::CaseState::Cancelled
    );
    tx.commit().await.unwrap();

    let replacement = inactive(&f, owner.id, plan, secret).await;
    assert_ne!(case, replacement);
    f.engine.cleanup_plan(plan).await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let s: Secret = get(&mut *tx, secret).await.unwrap();
    assert_eq!(s.state, SecretState::Armed);
    assert_eq!(s.last_case, Some(replacement));
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn cancellation_request_refreshes_invalidated_epoch_and_secret_snapshot() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    secret(&f, &owner, plan, &people, false).await;
    let first = f
        .engine
        .request_cancellation(people[0].id, plan, None)
        .await
        .unwrap();
    f.engine
        .control(owner.id, plan, Id::new_v4(), Control::CheckIn)
        .await
        .unwrap();
    let second = f
        .engine
        .request_cancellation(people[0].id, plan, None)
        .await
        .unwrap();
    assert_ne!(first, second);
    assert!(f.engine.vote_cancel(people[0].id, first).await.is_err());
    secret(&f, &owner, plan, &people, false).await;
    let third = f
        .engine
        .request_cancellation(people[0].id, plan, None)
        .await
        .unwrap();
    assert_ne!(second, third);
    assert!(f.engine.vote_cancel(people[0].id, second).await.is_err());
    for (i, person) in people.iter().enumerate() {
        assert_eq!(
            f.engine.vote_cancel(person.id, third).await.unwrap(),
            i == people.len() - 1
        );
    }
    let mut tx = f.engine.db.begin().await.unwrap();
    assert_eq!(
        get::<Plan>(&mut *tx, plan).await.unwrap().state,
        PlanState::Paused
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn worker_loss_cannot_downgrade_confirmed_delivery_to_unknown() {
    let f = fixture().await;
    let (_, _, secret, _, job) = prepared_delivery(&f).await;
    let job = f.engine.claim_job(job.id).await.unwrap().unwrap();
    let (account, _, _) = f.engine.delivery_block(&job).await.unwrap();
    assert!(
        f.engine
            .authorize_dispatch(&job, account.chat_id)
            .await
            .unwrap()
    );
    f.engine
        .finish_job(job.id, job.lease_token, SendResult::Unknown)
        .await
        .unwrap();
    f.engine
        .finish_job(job.id, job.lease_token, SendResult::Sent(401))
        .await
        .unwrap();
    // The lease reaper may have observed Dispatching immediately before that commit.
    f.engine
        .finish_job(job.id, job.lease_token, SendResult::Unknown)
        .await
        .unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let finished: Job = get(&mut *tx, job.id).await.unwrap();
    assert_eq!(finished.state, PartState::Sent);
    assert_eq!(finished.message_id, Some(401));
    let attempt: Attempt = get(&mut *tx, job.lease_token).await.unwrap();
    assert_eq!(attempt.state, PartState::Sent);
    assert_eq!(attempt.message_id, Some(401));
    let parts = list::<DeliveryPart>(&mut *tx, Some(secret)).await.unwrap();
    assert!(parts.iter().all(|p| p.state == PartState::Sent));
    assert_eq!(
        get::<Secret>(&mut *tx, secret).await.unwrap().state,
        SecretState::Delivered
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn recovery_begin_replay_without_claim_keeps_stop_and_allows_authenticated_retry() {
    let f = fixture().await;
    let owner = f.engine.account(4001, 4001, "en").await.unwrap();
    let plan_id = f.engine.create_profile(owner.id).await.unwrap();
    let (snapshot_profile, snapshot_plan) = f.engine.own_plan(owner.id).await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let original = list::<Job>(&mut *tx, Some(plan_id))
        .await
        .unwrap()
        .into_iter()
        .find(|j| matches!(j.task, Task::RecoveryCode { .. }))
        .unwrap();
    tx.commit().await.unwrap();
    let Task::RecoveryCode {
        selector, envelope, ..
    } = original.task
    else {
        unreachable!()
    };
    let token = f
        .engine
        .crypto
        .unwrap("recovery", selector, &envelope)
        .unwrap();
    let operation = Id::new_v4();
    let claim = f
        .engine
        .rotate_recovery(owner.id, plan_id, operation)
        .await
        .unwrap();

    // Restore the SQL records from before the journaled RecoveryBegin. The new claim
    // and its delivery envelope were never part of that snapshot.
    let mut tx = f.engine.db.begin().await.unwrap();
    put(&mut *tx, Some(owner.id), &snapshot_profile)
        .await
        .unwrap();
    put(&mut *tx, Some(snapshot_profile.id), &snapshot_plan)
        .await
        .unwrap();
    tx.remove(Kind::Claim, claim).await.unwrap();
    tx.remove(Kind::ControlIntent, operation).await.unwrap();
    tx.commit().await.unwrap();
    f.engine.replay_controls().await.unwrap();
    let (profile, plan) = f.engine.own_plan(owner.id).await.unwrap();
    assert_eq!(plan.state, PlanState::Paused);
    assert!(profile.pending_claim.is_none());
    let target = f.engine.account(4002, 4002, "uk").await.unwrap();
    assert!(
        f.engine
            .recover(
                target.id,
                selector,
                std::str::from_utf8(&token).unwrap(),
                false,
                Id::new_v4()
            )
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn final_dispatch_revalidates_recovery_rotation_and_claim_acknowledgement() {
    let f = fixture().await;
    let owner = f.engine.account(3001, 3001, "en").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let original = list::<Job>(&mut *tx, Some(plan))
        .await
        .unwrap()
        .into_iter()
        .find(|j| matches!(j.task, Task::RecoveryCode { .. }))
        .unwrap();
    tx.commit().await.unwrap();
    let original = f.engine.claim_job(original.id).await.unwrap().unwrap();
    let claim = f
        .engine
        .rotate_recovery(owner.id, plan, Id::new_v4())
        .await
        .unwrap();
    // A worker prepared the old token before rotation, then waited for Telegram pacing.
    assert!(
        !f.engine
            .authorize_dispatch(&original, owner.chat_id)
            .await
            .unwrap()
    );
    let mut tx = f.engine.db.begin().await.unwrap();
    let claim_job = list::<Job>(&mut *tx, Some(plan))
        .await
        .unwrap()
        .into_iter()
        .find(|j| matches!(j.task, Task::ClaimCode { claim_id } if claim_id == claim))
        .unwrap();
    tx.commit().await.unwrap();
    let claim_job = f.engine.claim_job(claim_job.id).await.unwrap().unwrap();
    f.engine
        .acknowledge_claim(owner.id, claim, Id::new_v4())
        .await
        .unwrap();
    assert!(
        !f.engine
            .authorize_dispatch(&claim_job, owner.chat_id)
            .await
            .unwrap()
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn final_dispatch_revalidates_guardian_issuance_and_completed_quorum() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    f.engine
        .append_block(
            owner.id,
            draft,
            Block::Text {
                text: "synthetic".into(),
            },
            (owner.chat_id, 800),
        )
        .await
        .unwrap();
    f.engine
        .draft_policy(
            owner.id,
            draft,
            Policy {
                guardians: [people[0].id].into(),
                recipients: [people[0].id].into(),
                threshold: 1,
                timing: Timing::default(),
            },
        )
        .await
        .unwrap();
    let secret_id = f.engine.save(owner.id, draft).await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let grant = list::<GuardianGrant>(&mut *tx, Some(secret_id))
        .await
        .unwrap()
        .remove(0);
    let job = list::<Job>(&mut *tx, Some(plan))
        .await
        .unwrap()
        .into_iter()
        .find(|j| matches!(j.task, Task::GuardianCode { grant_id } if grant_id == grant.id))
        .unwrap();
    tx.commit().await.unwrap();
    let job = f.engine.claim_job(job.id).await.unwrap().unwrap();
    f.engine
        .control(
            owner.id,
            plan,
            Id::new_v4(),
            Control::StopSecret { secret_id },
        )
        .await
        .unwrap();
    assert!(
        !f.engine
            .authorize_dispatch(&job, people[0].chat_id)
            .await
            .unwrap()
    );

    let (secret_id, codes) = secret(&f, &owner, plan, &people, false).await;
    let case = inactive(&f, owner.id, plan, secret_id).await;
    let mut tx = f.engine.db.begin().await.unwrap();
    let request = list::<Job>(&mut *tx, Some(plan))
        .await
        .unwrap()
        .into_iter()
        .find(|j| matches!(j.task, Task::GuardianRequest { case_id, .. } if case_id == case))
        .unwrap();
    tx.commit().await.unwrap();
    let request = f.engine.claim_job(request.id).await.unwrap().unwrap();
    let recipient = match request.task {
        Task::GuardianRequest { account_id, .. } => account_id,
        _ => unreachable!(),
    };
    for (person, code) in codes.iter().take(3) {
        f.engine.submit_code(person.id, case, code).await.unwrap();
    }
    let chat_id = people.iter().find(|p| p.id == recipient).unwrap().chat_id;
    assert!(
        !f.engine
            .authorize_dispatch(&request, chat_id)
            .await
            .unwrap()
    );
}
