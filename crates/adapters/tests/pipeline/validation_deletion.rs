use super::*;
use sqlx::Row;

const RECORDS: &[Kind] = &[
    Kind::Account,
    Kind::Profile,
    Kind::Plan,
    Kind::Participant,
    Kind::Invitation,
    Kind::Draft,
    Kind::Secret,
    Kind::GuardianGrant,
    Kind::Case,
    Kind::Submission,
    Kind::FileObject,
    Kind::DeliveryPart,
    Kind::Claim,
    Kind::Cancellation,
    Kind::Dialog,
    Kind::Action,
    Kind::Job,
    Kind::ControlIntent,
    Kind::HandledEvent,
    Kind::Attempt,
];

async fn snapshot(f: &Fixture) -> Vec<(Kind, Id, Option<Id>, serde_json::Value)> {
    let mut result = Vec::new();
    for kind in RECORDS {
        let query = format!("SELECT id,scope_id,data FROM {}", kind.table());
        for row in sqlx::query(sqlx::AssertSqlSafe(query))
            .fetch_all(&f.db.pool)
            .await
            .unwrap()
        {
            result.push((*kind, row.get("id"), row.get("scope_id"), row.get("data")));
        }
    }
    result
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn profile_purge_removes_contact_metadata_replays_and_preserves_foreign_plan() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let (secret, _) = secret(&f, &owner, plan, &people, true).await;
    let (profile, _) = f.engine.own_plan(owner.id).await.unwrap();
    let claim = f
        .engine
        .rotate_recovery(owner.id, plan, Id::new_v4())
        .await
        .unwrap();
    f.engine
        .acknowledge_claim(owner.id, claim, Id::new_v4())
        .await
        .unwrap();

    let other = f.engine.account(6001, 6001, "uk").await.unwrap();
    let other_plan = f.engine.create_profile(other.id).await.unwrap();
    let invitation = f.engine.invite(other.id, other_plan).await.unwrap();
    f.engine.accept_invite(owner.id, invitation).await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let participant = list::<Participant>(&mut *tx, Some(other_plan))
        .await
        .unwrap()
        .remove(0);
    tx.commit().await.unwrap();
    f.engine
        .confirm_participant(other.id, other_plan, participant.id)
        .await
        .unwrap();
    let before = snapshot(&f).await;
    f.engine
        .control(owner.id, plan, Id::new_v4(), Control::DeleteProfile)
        .await
        .unwrap();
    f.engine.replay_controls().await.unwrap();

    let mut tx = f.engine.db.begin().await.unwrap();
    assert!(tx.get(Kind::Profile, profile.id).await.unwrap().is_none());
    assert!(tx.get(Kind::Plan, plan).await.unwrap().is_none());
    assert!(tx.get(Kind::Secret, secret).await.unwrap().is_none());
    assert!(
        list::<ControlIntent>(&mut *tx, Some(plan))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        list::<CaseRecord>(&mut *tx, Some(secret))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        list::<DeliveryPart>(&mut *tx, Some(secret))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        get::<Account>(&mut *tx, owner.id)
            .await
            .unwrap()
            .telegram_id,
        owner.telegram_id
    );
    assert!(
        get::<Participant>(&mut *tx, participant.id)
            .await
            .unwrap()
            .confirmed
    );
    let other_profile_id = get::<Plan>(&mut *tx, other_plan).await.unwrap().profile_id;
    assert_eq!(
        get::<Profile>(&mut *tx, other_profile_id)
            .await
            .unwrap()
            .owner_id,
        other.id
    );
    for tomb in tx.list(Kind::DeletionTombstone, None).await.unwrap() {
        assert_eq!(tomb.as_object().unwrap().len(), 3);
        assert!(tomb.get("owner_id").is_none());
        assert!(tomb.get("policy").is_none());
    }
    tx.commit().await.unwrap();

    // A separate fresh schema represents the full SQL snapshot before deletion.
    let restored = fixture().await;
    let mut tx = restored.engine.db.begin().await.unwrap();
    for (kind, id, scope, data) in before {
        tx.put(kind, id, scope, data).await.unwrap();
    }
    tx.commit().await.unwrap();
    let mut replay = restored.engine.clone();
    replay.crypto = f.engine.crypto.clone();
    replay.journal = f.engine.journal.clone();
    replay.blobs = f.engine.blobs.clone();
    replay.replay_controls().await.unwrap();
    replay.replay_controls().await.unwrap();
    let mut tx = replay.db.begin().await.unwrap();
    assert!(tx.get(Kind::Profile, profile.id).await.unwrap().is_none());
    assert!(tx.get(Kind::Secret, secret).await.unwrap().is_none());
    assert!(tx.get(Kind::Plan, other_plan).await.unwrap().is_some());
    assert!(
        tx.get(Kind::Participant, participant.id)
            .await
            .unwrap()
            .is_some()
    );
    tx.commit().await.unwrap();

    // Cleanup remains possible after parent removal, then releases its remaining chat/object data.
    let mut tx = f.engine.db.begin().await.unwrap();
    let objects = list::<FileObject>(&mut *tx, Some(plan)).await.unwrap();
    assert!(!objects.is_empty());
    let jobs = list::<Job>(&mut *tx, Some(plan)).await.unwrap();
    assert!(!jobs.is_empty());
    for mut object in objects.clone() {
        assert_eq!(object.state, "gc");
        object.created_at = tx.now().await.unwrap() - 3 * DAY;
        object.due_at = tx.now().await.unwrap() - 1;
        put(&mut *tx, Some(plan), &object).await.unwrap();
    }
    tx.commit().await.unwrap();
    for job in jobs {
        assert!(matches!(
            job.task,
            Task::CleanupMessage { .. } | Task::DeleteObject { .. }
        ));
        let claimed = f.engine.claim_job(job.id).await.unwrap().unwrap();
        f.engine
            .finish_job(job.id, claimed.lease_token, SendResult::Sent(0))
            .await
            .unwrap();
        assert!(f.engine.claim_job(job.id).await.unwrap().is_none());
        f.engine
            .finish_job(job.id, claimed.lease_token, SendResult::Unknown)
            .await
            .unwrap();
    }
    for object in objects {
        f.engine.garbage_collect(object.id).await.unwrap();
        f.engine.garbage_collect(object.id).await.unwrap();
        assert!(!f.engine.blobs.exists(&object.key).await.unwrap());
    }
    f.engine.cleanup_plan(plan).await.unwrap();
    f.engine.tick_plan(plan).await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    assert!(
        list::<FileObject>(&mut *tx, Some(plan))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(list::<Job>(&mut *tx, Some(plan)).await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn secret_purge_keeps_sibling_policy_and_database_rejects_resurrection() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let (first, _) = secret(&f, &owner, plan, &people, false).await;
    let (second, _) = secret(&f, &owner, plan, &people, false).await;
    let mut tx = f.engine.db.begin().await.unwrap();
    let old: Secret = get(&mut *tx, first).await.unwrap();
    let sibling = tx.get(Kind::Secret, second).await.unwrap().unwrap();
    tx.commit().await.unwrap();
    f.engine
        .control(
            owner.id,
            plan,
            Id::new_v4(),
            Control::DeleteSecret { secret_id: first },
        )
        .await
        .unwrap();
    f.engine.replay_controls().await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    assert!(tx.get(Kind::Secret, first).await.unwrap().is_none());
    assert_eq!(
        tx.get(Kind::Secret, second).await.unwrap().unwrap(),
        sibling
    );
    assert!(put(&mut *tx, Some(plan), &old).await.is_err());
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn database_allows_only_one_live_case_per_secret() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let (secret, _) = secret(&f, &owner, plan, &people, false).await;
    let first = inactive(&f, owner.id, plan, secret).await;
    let mut tx = f.engine.db.begin().await.unwrap();
    let mut duplicate: CaseRecord = get(&mut *tx, first).await.unwrap();
    duplicate.id = Id::new_v4();
    duplicate.case.id = duplicate.id;
    assert!(put(&mut *tx, Some(secret), &duplicate).await.is_err());
    drop(tx);
    let mut tx = f.engine.db.begin().await.unwrap();
    let mut expired: CaseRecord = get(&mut *tx, first).await.unwrap();
    expired.case.state = domain::CaseState::NeedsAttention;
    put(&mut *tx, Some(secret), &expired).await.unwrap();
    put(&mut *tx, Some(secret), &duplicate).await.unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn deleting_plan_retains_profile_credential_and_new_plan_has_fresh_identity() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let (secret, _) = secret(&f, &owner, plan, &people, false).await;
    let (profile, _) = f.engine.own_plan(owner.id).await.unwrap();
    f.engine
        .control(owner.id, plan, Id::new_v4(), Control::DeletePlan)
        .await
        .unwrap();
    f.engine.replay_controls().await.unwrap();
    let replacement = f.engine.create_profile(owner.id).await.unwrap();
    assert_ne!(replacement, plan);
    assert_eq!(
        f.engine.create_profile(owner.id).await.unwrap(),
        replacement
    );
    let (retained, fresh) = f.engine.own_plan(owner.id).await.unwrap();
    assert_eq!(retained.id, profile.id);
    assert_eq!(retained.recovery_selector, profile.recovery_selector);
    assert_eq!(retained.recovery_hash, profile.recovery_hash);
    assert!(retained.recovery_saved);
    assert_eq!(fresh.state, PlanState::Setup);
    let mut tx = f.engine.db.begin().await.unwrap();
    assert!(tx.get(Kind::Plan, plan).await.unwrap().is_none());
    assert!(tx.get(Kind::Secret, secret).await.unwrap().is_none());
    assert!(
        list::<Secret>(&mut *tx, Some(replacement))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        list::<Participant>(&mut *tx, Some(replacement))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        list::<Job>(&mut *tx, Some(replacement))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn replay_discards_presave_snapshot_drafts_and_preserves_sealed_sibling() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let (sibling, _) = secret(&f, &owner, plan, &people, true).await;
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    f.engine
        .append_file(
            owner.id,
            draft,
            "draft.bin".into(),
            "synthetic".into(),
            Zeroizing::new(vec![1, 2, 3]),
            (owner.chat_id, 902),
        )
        .await
        .unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let sealed: Secret = get(&mut *tx, sibling).await.unwrap();
    let sealed_before = serde_json::to_value(&sealed).unwrap();
    tx.commit().await.unwrap();
    f.engine
        .draft_policy(owner.id, draft, sealed.policy)
        .await
        .unwrap();
    let before = snapshot(&f).await;
    let saved = f.engine.save(owner.id, draft).await.unwrap();

    for delete_later in [false, true] {
        if delete_later {
            f.engine
                .control(
                    owner.id,
                    plan,
                    Id::new_v4(),
                    Control::DeleteSecret { secret_id: saved },
                )
                .await
                .unwrap();
        }
        let restored = fixture().await;
        let mut tx = restored.engine.db.begin().await.unwrap();
        for (kind, id, scope, data) in before.clone() {
            tx.put(kind, id, scope, data).await.unwrap();
        }
        tx.commit().await.unwrap();
        let mut replay = restored.engine.clone();
        replay.crypto = f.engine.crypto.clone();
        replay.journal = f.engine.journal.clone();
        replay.blobs = f.engine.blobs.clone();
        replay.replay_controls().await.unwrap();
        assert!(replay.draft_blocks(owner.id, draft).await.is_err());
        let mut tx = replay.db.begin().await.unwrap();
        assert!(tx.get(Kind::Draft, draft).await.unwrap().is_none());
        assert_eq!(
            tx.get(Kind::Secret, sibling).await.unwrap().unwrap(),
            sealed_before
        );
        let objects = list::<FileObject>(&mut *tx, Some(plan)).await.unwrap();
        assert!(
            objects
                .iter()
                .filter(|o| o.draft_id == draft)
                .all(|o| o.state == "gc")
        );
        assert!(
            objects
                .iter()
                .any(|o| o.operation_id == sibling && o.state == "sealed"),
            "sealed sibling object preserved after delete={delete_later}: {:?}",
            objects
                .iter()
                .map(|o| (o.operation_id, o.draft_id, o.state.as_str()))
                .collect::<Vec<_>>()
        );
        let jobs = list::<Job>(&mut *tx, Some(plan)).await.unwrap();
        assert!(jobs.iter().any(|j| matches!(
            j.task,
            Task::CleanupMessage {
                message_id: 902,
                ..
            }
        )));
        if !delete_later {
            assert!(
                jobs.iter()
                    .any(|j| matches!(&j.task, Task::Notice { key, .. } if key == "draft-reset"))
            );
        } else {
            assert!(
                tx.get(Kind::DeletionTombstone, saved)
                    .await
                    .unwrap()
                    .is_some()
            );
        }
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn original_recovery_token_transfers_retained_profile_without_reviving_deleted_plan() {
    let f = fixture().await;
    let (owner, old_plan, people) = participants(&f).await;
    let (secret, _) = secret(&f, &owner, old_plan, &people, false).await;
    // Rotate once to obtain an acknowledged credential without retaining it in storage.
    let rotation = f
        .engine
        .rotate_recovery(owner.id, old_plan, Id::new_v4())
        .await
        .unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let claim: Claim = get(&mut *tx, rotation).await.unwrap();
    let token = f
        .engine
        .crypto
        .unwrap("claim", claim.id, &claim.delivery)
        .unwrap();
    tx.commit().await.unwrap();
    f.engine
        .acknowledge_claim(owner.id, rotation, Id::new_v4())
        .await
        .unwrap();
    let (before, _) = f.engine.own_plan(owner.id).await.unwrap();
    f.engine
        .control(owner.id, old_plan, Id::new_v4(), Control::DeletePlan)
        .await
        .unwrap();

    let target = f.engine.account(7001, 7001, "uk").await.unwrap();
    let recovery = f
        .engine
        .recover(
            target.id,
            before.recovery_selector,
            std::str::from_utf8(&token).unwrap(),
            false,
            Id::new_v4(),
        )
        .await
        .unwrap()
        .unwrap();
    let (pending, fresh_plan) = f.engine.own_plan(owner.id).await.unwrap();
    assert_eq!(pending.id, before.id);
    assert_eq!(
        pending.owner_id, owner.id,
        "new-token acknowledgement must precede transfer"
    );
    assert_ne!(fresh_plan.id, old_plan);
    assert_eq!(fresh_plan.state, PlanState::Paused);
    f.engine
        .acknowledge_claim(target.id, recovery, Id::new_v4())
        .await
        .unwrap();
    let (transferred, plan) = f.engine.own_plan(target.id).await.unwrap();
    assert_eq!(transferred.id, before.id);
    assert_eq!(transferred.owner_epoch, before.owner_epoch + 1);
    assert_eq!(plan.id, fresh_plan.id);
    assert_eq!(plan.state, PlanState::Paused);
    assert!(f.engine.own_plan(owner.id).await.is_err());
    let mut tx = f.engine.db.begin().await.unwrap();
    assert!(tx.get(Kind::Plan, old_plan).await.unwrap().is_none());
    assert!(tx.get(Kind::Secret, secret).await.unwrap().is_none());
    assert!(
        tx.get(Kind::DeletionTombstone, old_plan)
            .await
            .unwrap()
            .is_some()
    );
    for kind in [
        Kind::Secret,
        Kind::Participant,
        Kind::Invitation,
        Kind::Draft,
    ] {
        assert!(tx.list(kind, Some(plan.id)).await.unwrap().is_empty());
    }
    tx.commit().await.unwrap();
    assert!(
        f.engine
            .recover(
                owner.id,
                before.recovery_selector,
                std::str::from_utf8(&token).unwrap(),
                false,
                Id::new_v4()
            )
            .await
            .is_err()
    );
}
