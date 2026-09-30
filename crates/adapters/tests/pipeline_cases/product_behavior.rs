use super::*;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn telegram_display_names_are_encrypted_and_role_scoped() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    assert_eq!(f.engine.account_display_name(&owner).unwrap(), None);
    f.engine
        .update_account_display(
            owner.id,
            Some("Марія"),
            Some("Коваль"),
            Some("mariia_owner"),
        )
        .await
        .unwrap();
    f.engine
        .update_account_display(
            people[0].id,
            Some("Олена"),
            Some("Петренко"),
            Some("olena_person"),
        )
        .await
        .unwrap();
    let contact = f
        .engine
        .contacts(owner.id, plan)
        .await
        .unwrap()
        .into_iter()
        .find(|person| person.account_id == people[0].id)
        .unwrap();
    assert_eq!(contact.display_name.as_deref(), Some("Олена Петренко"));
    assert_eq!(contact.username.as_deref(), Some("olena_person"));
    assert_eq!(contact.telegram_id, people[0].telegram_id);
    f.engine
        .set_label(owner.id, plan, contact.id, "Private nickname")
        .await
        .unwrap();
    let contact = f
        .engine
        .contacts(owner.id, plan)
        .await
        .unwrap()
        .into_iter()
        .find(|person| person.account_id == people[0].id)
        .unwrap();
    assert_eq!(contact.label.as_deref(), Some("Private nickname"));
    assert_eq!(contact.display_name.as_deref(), Some("Олена Петренко"));
    assert!(f.engine.contacts(people[0].id, plan).await.is_err());
    assert!(
        f.engine
            .owner_label(people[0].id, plan, contact.id)
            .await
            .is_err()
    );

    let invite = f.engine.invite(owner.id, plan).await.unwrap();
    let preview = f.engine.invitation(people[0].id, invite).await.unwrap();
    assert_eq!(preview.owner_display_name.as_deref(), Some("Марія Коваль"));
    assert_eq!(preview.owner_telegram_id, owner.telegram_id);
    assert!(
        f.engine
            .invitations(owner.id, plan)
            .await
            .unwrap()
            .iter()
            .all(|invitation| invitation.owner_display_name.as_deref() == Some("Марія Коваль"))
    );
    secret(&f, &owner, plan, &people, false).await;
    let projection = f.engine.overview(people[0].id).await.unwrap();
    assert_eq!(
        projection.guardian[0].owner_display_name.as_deref(),
        Some("Марія Коваль")
    );
    assert_eq!(
        projection.receiving[0].owner_display_name.as_deref(),
        Some("Марія Коваль")
    );
    assert_eq!(projection.guardian[0].owner_telegram_id, owner.telegram_id);
    assert_eq!(projection.receiving[0].owner_telegram_id, owner.telegram_id);

    let mut tx = f.db.begin().await.unwrap();
    let raw = tx.get(Kind::Account, people[0].id).await.unwrap().unwrap();
    let account: Account = serde_json::from_value(raw.clone()).unwrap();
    for plaintext in ["Олена", "Петренко", "olena_person"] {
        assert!(
            !raw.to_string().contains(plaintext),
            "display metadata must be ciphertext at rest"
        );
    }
    tx.commit().await.unwrap();
    let mut copied = account.clone();
    copied.id = people[1].id;
    assert!(
        matches!(f.engine.account_display_name(&copied), Err(Error::Crypto)),
        "account identity must authenticate the encrypted metadata"
    );
    f.engine
        .update_account_display(
            people[0].id,
            Some("Олена"),
            Some("Петренко"),
            Some("olena_person"),
        )
        .await
        .unwrap();
    f.engine
        .update_account_display(people[0].id, None, None, Some("incomplete_update"))
        .await
        .unwrap();
    let mut tx = f.db.begin().await.unwrap();
    assert_eq!(
        tx.get(Kind::Account, people[0].id).await.unwrap().unwrap(),
        raw,
        "identical or incomplete Telegram updates must preserve the encrypted snapshot"
    );
    tx.commit().await.unwrap();

    f.engine
        .update_account_display(people[0].id, Some("Олена"), None, None)
        .await
        .unwrap();
    let contact = f
        .engine
        .contacts(owner.id, plan)
        .await
        .unwrap()
        .into_iter()
        .find(|person| person.account_id == people[0].id)
        .unwrap();
    assert_eq!(contact.display_name.as_deref(), Some("Олена"));
    assert_eq!(
        contact.username, None,
        "a complete snapshot clears a removed username"
    );
    assert_eq!(contact.label.as_deref(), Some("Private nickname"));
    f.engine
        .update_account_display(people[1].id, Some(""), None, Some("username_only"))
        .await
        .unwrap();
    let contact = f
        .engine
        .contacts(owner.id, plan)
        .await
        .unwrap()
        .into_iter()
        .find(|person| person.account_id == people[1].id)
        .unwrap();
    assert_eq!(contact.display_name.as_deref(), Some("@username_only"));
    assert_eq!(contact.telegram_id, people[1].telegram_id);
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn private_labels_preserve_sealed_policy_and_are_owner_scoped() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let (secret_id, _) = secret(&f, &owner, plan, &people, false).await;
    let mut tx = f.db.begin().await.unwrap();
    let before: Secret = get(&mut *tx, secret_id).await.unwrap();
    tx.commit().await.unwrap();
    f.engine
        .set_label(owner.id, plan, secret_id, "Private family notes")
        .await
        .unwrap();
    assert_eq!(
        f.engine
            .owner_label(owner.id, plan, secret_id)
            .await
            .unwrap()
            .as_deref(),
        Some("Private family notes")
    );
    assert!(
        f.engine
            .owner_label(people[0].id, plan, secret_id)
            .await
            .is_err()
    );
    assert!(
        f.engine
            .set_label(people[0].id, plan, secret_id, "changed")
            .await
            .is_err()
    );
    assert!(
        f.engine
            .set_label(owner.id, plan, secret_id, "line\nbreak")
            .await
            .is_err()
    );
    let mut tx = f.db.begin().await.unwrap();
    let after: Secret = get(&mut *tx, secret_id).await.unwrap();
    assert_eq!(
        serde_json::to_value(&before.payload).unwrap(),
        serde_json::to_value(&after.payload).unwrap()
    );
    assert_eq!(before.policy, after.policy);
    let metadata = tx
        .get(Kind::PrivateMetadata, secret_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!metadata.to_string().contains("Private family notes"));
    tx.commit().await.unwrap();
    let owned = f.engine.overview(owner.id).await.unwrap().own.unwrap();
    assert_eq!(
        owned.secrets[0].label.as_deref(),
        Some("Private family notes")
    );
    let participant_view = f.engine.overview(people[0].id).await.unwrap();
    assert!(participant_view.own.is_none());
    assert_eq!(participant_view.guardian.len(), 1);
    assert_eq!(participant_view.receiving.len(), 1);
    f.engine
        .set_utc_offset_minutes(owner.id, 345)
        .await
        .unwrap();
    assert_eq!(f.engine.utc_offset_minutes(owner.id).await.unwrap(), 345);
    assert!(
        f.engine
            .set_utc_offset_minutes(owner.id, 346)
            .await
            .is_err()
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn invitation_retry_reject_revoke_decline_and_archive_keep_authority() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let newcomer = f.engine.account(3001, 3001, "en").await.unwrap();
    let operation = Id::new_v4();
    let invite = f
        .engine
        .invite_with_id(owner.id, plan, operation)
        .await
        .unwrap();
    assert_eq!(
        f.engine
            .invite_with_id(owner.id, plan, operation)
            .await
            .unwrap(),
        invite
    );
    f.engine.accept_invite(newcomer.id, invite).await.unwrap();
    let mut tx = f.db.begin().await.unwrap();
    let notices = list::<Job>(&mut *tx, Some(plan)).await.unwrap().len();
    tx.commit().await.unwrap();
    f.engine.accept_invite(newcomer.id, invite).await.unwrap();
    let mut tx = f.db.begin().await.unwrap();
    assert_eq!(
        list::<Job>(&mut *tx, Some(plan)).await.unwrap().len(),
        notices
    );
    let joined = list::<Participant>(&mut *tx, Some(plan))
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.account_id == newcomer.id)
        .unwrap();
    tx.commit().await.unwrap();
    f.engine
        .reject_participant(owner.id, plan, joined.id)
        .await
        .unwrap();
    f.engine
        .reject_participant(owner.id, plan, joined.id)
        .await
        .unwrap();
    assert!(f.engine.accept_invite(newcomer.id, invite).await.is_err());
    let revoked = f.engine.invite(owner.id, plan).await.unwrap();
    f.engine.revoke_invitation(owner.id, revoked).await.unwrap();
    assert_eq!(
        f.engine
            .invitation(newcomer.id, revoked)
            .await
            .unwrap()
            .status,
        InvitationStatus::Revoked
    );
    assert!(f.engine.accept_invite(newcomer.id, revoked).await.is_err());
    let declined = f.engine.invite(owner.id, plan).await.unwrap();
    f.engine
        .decline_invitation(newcomer.id, declined)
        .await
        .unwrap();
    assert_eq!(
        f.engine
            .invitation(newcomer.id, declined)
            .await
            .unwrap()
            .status,
        InvitationStatus::Declined
    );
    assert!(f.engine.accept_invite(newcomer.id, declined).await.is_err());
    let (sealed, _) = secret(&f, &owner, plan, &people, false).await;
    let contact = f
        .engine
        .contacts(owner.id, plan)
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.account_id == people[0].id)
        .unwrap();
    assert_eq!(contact.dependent_secrets, vec![sealed]);
    assert!(
        f.engine
            .archive_participant(owner.id, plan, contact.id)
            .await
            .is_err()
    );
    let fresh_invite = f.engine.invite(owner.id, plan).await.unwrap();
    f.engine
        .accept_invite(newcomer.id, fresh_invite)
        .await
        .unwrap();
    let contact = f
        .engine
        .contacts(owner.id, plan)
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.account_id == newcomer.id)
        .unwrap();
    f.engine
        .confirm_participant(owner.id, plan, contact.id)
        .await
        .unwrap();
    f.engine
        .archive_participant(owner.id, plan, contact.id)
        .await
        .unwrap();
    assert!(
        f.engine
            .contacts(owner.id, plan)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.id == contact.id)
            .unwrap()
            .archived
    );
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    assert!(
        f.engine
            .draft_policy(
                owner.id,
                draft,
                Policy {
                    guardians: [newcomer.id].into(),
                    recipients: [people[0].id].into(),
                    threshold: 1,
                    timing: Timing::default()
                }
            )
            .await
            .is_err()
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn draft_editing_fences_stale_revisions_and_collects_removed_files() {
    let f = fixture().await;
    let (owner, plan, _) = participants(&f).await;
    let draft_id = f.engine.new_draft(owner.id, plan).await.unwrap();
    for (index, text) in ["first", "second"].into_iter().enumerate() {
        f.engine
            .append_block(
                owner.id,
                draft_id,
                Block::Text { text: text.into() },
                (owner.chat_id, 700 + index as i64),
            )
            .await
            .unwrap();
    }
    f.engine
        .append_file(
            owner.id,
            draft_id,
            "test.txt".into(),
            String::new(),
            Zeroizing::new(b"synthetic".to_vec()),
            (owner.chat_id, 702),
        )
        .await
        .unwrap();
    let mut tx = f.db.begin().await.unwrap();
    let draft: Draft = get(&mut *tx, draft_id).await.unwrap();
    let object = list::<FileObject>(&mut *tx, Some(plan))
        .await
        .unwrap()
        .remove(0);
    tx.commit().await.unwrap();
    f.engine
        .move_draft_block(owner.id, draft_id, draft.revision, 0, 1)
        .await
        .unwrap();
    assert!(
        f.engine
            .move_draft_block(owner.id, draft_id, draft.revision, 0, 1)
            .await
            .is_err()
    );
    let blocks = f.engine.draft_blocks(owner.id, draft_id).await.unwrap();
    assert!(matches!(&blocks[0], Block::Text { text } if text == "second"));
    f.engine
        .remove_draft_block(owner.id, draft_id, draft.revision + 1, 2)
        .await
        .unwrap();
    let mut tx = f.db.begin().await.unwrap();
    assert_eq!(
        get::<FileObject>(&mut *tx, object.id).await.unwrap().state,
        "gc"
    );
    tx.commit().await.unwrap();
    f.engine
        .cancel_draft(owner.id, draft_id, draft.revision + 2)
        .await
        .unwrap();
    f.engine
        .cancel_draft(owner.id, draft_id, draft.revision + 2)
        .await
        .unwrap();
    let mut tx = f.db.begin().await.unwrap();
    assert!(tx.get(Kind::Draft, draft_id).await.unwrap().is_none());
    assert_eq!(
        get::<OperationReceipt>(&mut *tx, draft_id)
            .await
            .unwrap()
            .operation,
        OperationKind::DraftCancelled
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn readiness_and_scope_resume_keep_individual_secret_stop() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    assert!(
        f.engine
            .control(owner.id, plan, Id::new_v4(), Control::Rearm)
            .await
            .is_err()
    );
    let before = f.engine.overview(owner.id).await.unwrap().own.unwrap();
    assert_eq!(before.next_action, NextAction::CreateSecret);
    assert!(!before.can_resume);
    let (first, _) = secret(&f, &owner, plan, &people, false).await;
    let (second, _) = secret(&f, &owner, plan, &people, false).await;
    let ready = f.engine.overview(owner.id).await.unwrap().own.unwrap();
    assert_eq!(ready.ready_secrets, 2);
    assert_eq!(ready.next_action, NextAction::ResumePlan);
    assert_eq!(
        ready.secrets[0]
            .guardians
            .iter()
            .filter(|g| g.ready)
            .count(),
        5
    );
    assert_eq!(ready.secrets[0].threshold, 3);
    f.engine
        .control(owner.id, plan, Id::new_v4(), Control::Rearm)
        .await
        .unwrap();
    f.engine
        .control(
            owner.id,
            plan,
            Id::new_v4(),
            Control::StopSecret { secret_id: first },
        )
        .await
        .unwrap();
    f.engine
        .control(owner.id, plan, Id::new_v4(), Control::Stop)
        .await
        .unwrap();
    f.engine
        .control(owner.id, plan, Id::new_v4(), Control::Rearm)
        .await
        .unwrap();
    let mut tx = f.db.begin().await.unwrap();
    assert_eq!(
        get::<Secret>(&mut *tx, first).await.unwrap().state,
        SecretState::Paused
    );
    assert_eq!(
        get::<Secret>(&mut *tx, second).await.unwrap().state,
        SecretState::Armed
    );
    tx.commit().await.unwrap();
    f.engine
        .control(
            owner.id,
            plan,
            Id::new_v4(),
            Control::RearmSecret { secret_id: first },
        )
        .await
        .unwrap();
    let mut tx = f.db.begin().await.unwrap();
    assert_eq!(
        get::<Secret>(&mut *tx, first).await.unwrap().state,
        SecretState::Armed
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn deletion_confirmation_expires_paused_and_retries_return_receipt() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let (secret, _) = secret(&f, &owner, plan, &people, false).await;
    let request_id = Id::new_v4();
    let request = f
        .engine
        .prepare_deletion(owner.id, plan, Some(secret), request_id)
        .await
        .unwrap();
    assert_eq!(
        f.engine
            .prepare_deletion(owner.id, plan, Some(secret), request_id)
            .await
            .unwrap()
            .expires_at,
        request.expires_at
    );
    let mut tx = f.db.begin().await.unwrap();
    let mut expired = request;
    expired.expires_at = tx.now().await.unwrap() - 1;
    put(&mut *tx, Some(plan), &expired).await.unwrap();
    tx.commit().await.unwrap();
    assert!(
        f.engine
            .confirm_deletion(owner.id, request_id, Id::new_v4())
            .await
            .is_err()
    );
    let mut tx = f.db.begin().await.unwrap();
    assert_eq!(
        get::<Secret>(&mut *tx, secret).await.unwrap().state,
        SecretState::Paused
    );
    tx.commit().await.unwrap();
    let request = f
        .engine
        .prepare_deletion(owner.id, plan, Some(secret), Id::new_v4())
        .await
        .unwrap();
    assert!(
        f.engine
            .confirm_deletion(people[0].id, request.id, Id::new_v4())
            .await
            .is_err()
    );
    let operation = Id::new_v4();
    let receipt = f
        .engine
        .confirm_deletion(owner.id, request.id, operation)
        .await
        .unwrap();
    assert_eq!(receipt.status, ReceiptStatus::Completed);
    assert_eq!(receipt.operation, OperationKind::DeleteSecret);
    assert_eq!(
        f.engine
            .confirm_deletion(owner.id, request.id, operation)
            .await
            .unwrap()
            .id,
        operation
    );
    let mut tx = f.db.begin().await.unwrap();
    assert!(tx.get(Kind::Secret, secret).await.unwrap().is_none());
    assert!(
        tx.get(Kind::DeletionTombstone, secret)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn deletion_receipt_keeps_cleanup_pending_until_objects_are_confirmed_gone() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let (secret, _) = secret(&f, &owner, plan, &people, true).await;
    let mut tx = f.db.begin().await.unwrap();
    let mut delivered: Secret = get(&mut *tx, secret).await.unwrap();
    delivered.state = SecretState::Delivered;
    put(&mut *tx, Some(plan), &delivered).await.unwrap();
    tx.commit().await.unwrap();
    let request = f
        .engine
        .prepare_deletion(owner.id, plan, Some(secret), Id::new_v4())
        .await
        .unwrap();
    let operation = Id::new_v4();
    let receipt = f
        .engine
        .confirm_deletion(owner.id, request.id, operation)
        .await
        .unwrap();
    assert_eq!(receipt.status, ReceiptStatus::CleanupPending);
    assert_eq!(
        receipt.pending_objects.len(),
        2,
        "draft and sealed copies both require GC"
    );
    let mut tx = f.db.begin().await.unwrap();
    let now = tx.now().await.unwrap();
    let objects = list::<FileObject>(&mut *tx, Some(plan)).await.unwrap();
    for object in &objects {
        let mut old = object.clone();
        old.created_at = now - 3 * DAY;
        put(&mut *tx, Some(plan), &old).await.unwrap();
    }
    tx.commit().await.unwrap();
    for object in &objects {
        f.engine.garbage_collect(object.id).await.unwrap();
    }
    assert_eq!(
        f.engine
            .operation_receipt(owner.id, operation)
            .await
            .unwrap()
            .status,
        ReceiptStatus::Completed
    );
    assert!(f.blobs.0.lock().await.is_empty());
}

struct JournalFailure;
#[async_trait]
impl ControlJournal for JournalFailure {
    async fn append(&self, _: &ControlIntent) -> Result<u64> {
        Err(Error::Storage)
    }
    async fn read(&self) -> Result<Vec<ControlIntent>> {
        Ok(vec![])
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn deletion_retries_durable_intent_after_journal_unavailable() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let (secret, _) = secret(&f, &owner, plan, &people, false).await;
    let request = f
        .engine
        .prepare_deletion(owner.id, plan, Some(secret), Id::new_v4())
        .await
        .unwrap();
    let operation = Id::new_v4();
    let mut unavailable = f.engine.clone();
    unavailable.journal = Arc::new(JournalFailure);
    assert!(
        unavailable
            .confirm_deletion(owner.id, request.id, operation)
            .await
            .is_err()
    );
    let mut tx = f.db.begin().await.unwrap();
    assert_eq!(
        get::<DeletionRequest>(&mut *tx, request.id)
            .await
            .unwrap()
            .completed_operation,
        Some(operation)
    );
    assert_eq!(
        get::<Secret>(&mut *tx, secret)
            .await
            .unwrap()
            .pending_control,
        Some(operation)
    );
    tx.commit().await.unwrap();
    assert_eq!(
        f.engine
            .confirm_deletion(owner.id, request.id, operation)
            .await
            .unwrap()
            .id,
        operation
    );
    assert_eq!(
        f.engine
            .confirm_deletion(owner.id, request.id, operation)
            .await
            .unwrap()
            .status,
        ReceiptStatus::Completed
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn provisioning_readiness_and_private_draft_label_transfer() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    f.engine
        .set_label(owner.id, plan, draft, "Owner only")
        .await
        .unwrap();
    f.engine
        .append_block(
            owner.id,
            draft,
            Block::Text {
                text: "Synthetic example".into(),
            },
            (owner.chat_id, 801),
        )
        .await
        .unwrap();
    f.engine
        .draft_policy(
            owner.id,
            draft,
            Policy {
                guardians: people.iter().map(|a| a.id).collect(),
                recipients: [people[0].id].into(),
                threshold: 3,
                timing: Timing::default(),
            },
        )
        .await
        .unwrap();
    let sealed = f.engine.save(owner.id, draft).await.unwrap();
    assert_eq!(
        f.engine
            .owner_label(owner.id, plan, sealed)
            .await
            .unwrap()
            .as_deref(),
        Some("Owner only")
    );
    assert!(
        f.engine
            .owner_label(owner.id, plan, draft)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        f.engine
            .set_label(owner.id, plan, draft, "stale")
            .await
            .is_err()
    );
    let overview = f.engine.overview(owner.id).await.unwrap().own.unwrap();
    assert_eq!(overview.next_action, NextAction::AwaitCodes);
    assert_eq!(overview.ready_secrets, 0);
    assert_eq!(
        overview.secrets[0]
            .guardians
            .iter()
            .filter(|g| g.ready)
            .count(),
        0
    );
    assert!(overview.secrets[0].provisioning_expires_at.is_some());
    assert!(!overview.can_resume);
    let mut tx = f.db.begin().await.unwrap();
    let mut secret: Secret = get(&mut *tx, sealed).await.unwrap();
    secret.created_at = tx.now().await.unwrap() - DAY - 1;
    put(&mut *tx, Some(plan), &secret).await.unwrap();
    tx.commit().await.unwrap();
    f.engine.cleanup_plan(plan).await.unwrap();
    let failed = f.engine.overview(owner.id).await.unwrap().own.unwrap();
    assert_eq!(failed.next_action, NextAction::CreateSecret);
    assert_eq!(failed.secrets[0].state, SecretState::SetupFailed);
    assert!(!failed.secrets[0].can_resume);
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn recovery_rotation_and_claim_acknowledgement_are_idempotent() {
    let f = fixture().await;
    let (owner, plan, _) = participants(&f).await;
    let operation = Id::new_v4();
    let claim = f
        .engine
        .rotate_recovery(owner.id, plan, operation)
        .await
        .unwrap();
    assert_eq!(
        f.engine
            .rotate_recovery(owner.id, plan, operation)
            .await
            .unwrap(),
        claim
    );
    let acknowledge = Id::new_v4();
    f.engine
        .acknowledge_claim(owner.id, claim, acknowledge)
        .await
        .unwrap();
    f.engine
        .acknowledge_claim(owner.id, claim, acknowledge)
        .await
        .unwrap();
    let (profile, _) = f.engine.own_plan(owner.id).await.unwrap();
    assert!(profile.pending_claim.is_none());
    assert_eq!(
        f.engine
            .operation_receipt(owner.id, acknowledge)
            .await
            .unwrap()
            .operation,
        OperationKind::RecoveryCompleted
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn unknown_recovery_selectors_preserve_global_argon_budget() {
    let f = fixture().await;
    let owner = f.engine.account(9101, 9101, "en").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    let mut tx = f.db.begin().await.unwrap();
    let job = list::<Job>(&mut *tx, Some(plan))
        .await
        .unwrap()
        .into_iter()
        .find(|job| matches!(job.task, Task::RecoveryCode { .. }))
        .unwrap();
    tx.commit().await.unwrap();
    let Task::RecoveryCode {
        selector, envelope, ..
    } = job.task
    else {
        unreachable!()
    };
    let token = f
        .engine
        .crypto
        .unwrap("recovery", selector, &envelope)
        .unwrap();
    let token = std::str::from_utf8(&token).unwrap();
    let unknown = Id::new_v4();
    let unknown_token = format!("R1.{}.{}", unknown.simple(), "A".repeat(43));
    assert!(matches!(
        f.engine
            .account_for_recovery(9102, 9102, "en", unknown, &unknown_token)
            .await,
        Err(Error::InvalidCode)
    ));
    assert!(matches!(
        f.engine
            .recover(owner.id, unknown, &unknown_token, true, Id::new_v4())
            .await,
        Err(Error::InvalidCode)
    ));
    assert!(matches!(
        f.engine
            .recover(Id::from_u128(9103), selector, token, true, Id::new_v4())
            .await,
        Err(Error::NotFound)
    ));
    let used: Option<i64> =
        sqlx::query_scalar("SELECT used FROM rate_limits WHERE key='admission:argon-recovery'")
            .fetch_optional(&f.db.pool)
            .await
            .unwrap();
    assert_eq!(
        used, None,
        "cheap rejection must not consume the shared Argon budget"
    );
    let target = f
        .engine
        .account_for_recovery(9102, 9102, "en", selector, token)
        .await
        .unwrap();
    let used: i64 =
        sqlx::query_scalar("SELECT used FROM rate_limits WHERE key='admission:argon-recovery'")
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    assert_eq!(used, 1, "valid account recovery reserves Argon work");
    // Isolate the second assertion from a possible wall-clock minute rollover.
    sqlx::query("DELETE FROM rate_limits WHERE key='admission:argon-recovery'")
        .execute(&f.db.pool)
        .await
        .unwrap();
    f.engine
        .recover(target.id, selector, token, true, Id::new_v4())
        .await
        .unwrap();
    let used: i64 =
        sqlx::query_scalar("SELECT used FROM rate_limits WHERE key='admission:argon-recovery'")
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    assert_eq!(used, 1, "valid recovery execution also reserves Argon work");
}
