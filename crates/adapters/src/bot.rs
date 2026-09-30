//! Telegram conversation adapter. Business authority is rechecked by Engine commands.
use crate::{
    crypto::ArgonHasher,
    localization::tr,
    telegram::{DeleteResult, Telegram, entity},
};
use application::*;
use domain::{Block, CaseState, DAY, Id, PartState, Policy, RuleError, SecretState, Timing};
use serde_json::Value;
use teloxide_core::types::MessageEntityKind;
use zeroize::Zeroizing;

mod drafts;
mod menus;
mod setup;
mod workspace;
use menus::Menu;

#[derive(Clone)]
pub struct BotUi {
    pub engine: Engine,
    pub telegram: Telegram,
    pub username: String,
}
impl BotUi {
    async fn say(
        &self,
        account: &Account,
        key: &str,
        buttons: Vec<(String, String)>,
    ) -> Result<i64> {
        match self
            .telegram
            .send_text(
                account.chat_id,
                &tr(&account.locale, key),
                vec![],
                buttons,
                None,
            )
            .await
        {
            SendResult::Sent(id) => Ok(id),
            SendResult::RetryAfter(_) | SendResult::Permanent | SendResult::Unknown => {
                Err(Error::MessageUnavailable)
            }
        }
    }
    pub async fn button(
        &self,
        account: &Account,
        name: &str,
        target: Option<Id>,
        plan_id: Option<Id>,
        owner: bool,
        label: &str,
    ) -> Result<(String, String)> {
        let mut tx = self.engine.db.begin().await?;
        let now = tx.now().await?;
        let (owner_epoch, epoch) = if let Some(id) = plan_id {
            let p: Plan = get(&mut *tx, id).await?;
            let profile: Profile = get(&mut *tx, p.profile_id).await?;
            if owner && profile.owner_id != account.id {
                return Err(RuleError::AccessDenied.into());
            }
            (owner.then_some(profile.owner_epoch), Some(p.epoch))
        } else {
            (None, None)
        };
        let a = Action {
            id: Id::new_v4(),
            actor_id: account.id,
            plan_id,
            owner_epoch,
            epoch,
            name: name.into(),
            target,
            expires_at: now
                + if matches!(
                    name,
                    "delete-confirmed" | "stop-confirmed" | "stop-secret-confirmed" | "stop-cancel"
                ) {
                    300
                } else {
                    DAY
                },
            used: false,
        };
        put(&mut *tx, plan_id, &a).await?;
        tx.commit().await?;
        Ok((label.into(), a.id.simple().to_string()))
    }
    async fn b(
        &self,
        a: &Account,
        name: &str,
        target: Option<Id>,
        plan: Option<Id>,
        owner: bool,
    ) -> Result<(String, String)> {
        self.button(a, name, target, plan, owner, &tr(&a.locale, name))
            .await
    }
    async fn dialog(
        &self,
        account: &Account,
        step: &str,
        plan: Option<Id>,
        draft: Option<Id>,
    ) -> Result<Dialog> {
        let mut tx = self.engine.db.begin().await?;
        let now = tx.now().await?;
        let owner_epoch = if let Some(id) = plan {
            let plan: Plan = get(&mut *tx, id).await?;
            let p: Profile = get(&mut *tx, plan.profile_id).await?;
            Some(p.owner_epoch)
        } else {
            None
        };
        let d = Dialog {
            id: account.id,
            plan_id: plan,
            owner_epoch,
            step: step.into(),
            draft_id: draft,
            expires_at: now + 900,
            selected: Default::default(),
            reply_to: None,
            case_id: None,
            guardians: Default::default(),
            recipients: Default::default(),
            threshold: 0,
            timing: None,
        };
        if d.draft_id.is_some() {
            put(
                &mut *tx,
                plan,
                &DraftSession {
                    id: d.id,
                    dialog: d.clone(),
                },
            )
            .await?;
        } else {
            put(&mut *tx, plan, &d).await?;
        }
        tx.commit().await?;
        Ok(d)
    }
    async fn store_dialog(&self, d: &Dialog) -> Result<()> {
        let mut tx = self.engine.db.begin().await?;
        if d.draft_id.is_some() {
            put(
                &mut *tx,
                d.plan_id,
                &DraftSession {
                    id: d.id,
                    dialog: d.clone(),
                },
            )
            .await?;
        } else {
            put(&mut *tx, d.plan_id, d).await?;
        }
        tx.commit().await
    }
    async fn load_dialog(&self, a: &Account) -> Result<Dialog> {
        let mut tx = self.engine.db.begin().await?;
        let d: Dialog = if let Some(raw) = tx.get(Kind::Dialog, a.id).await? {
            serde_json::from_value(raw).map_err(|_| Error::Internal)?
        } else {
            get::<DraftSession>(&mut *tx, a.id).await?.dialog
        };
        tx.commit().await?;
        self.validate_dialog(d).await
    }
    async fn load_draft_dialog(&self, a: &Account) -> Result<Dialog> {
        let mut tx = self.engine.db.begin().await?;
        let session: DraftSession = get(&mut *tx, a.id).await?;
        tx.commit().await?;
        self.validate_dialog(session.dialog).await
    }
    async fn validate_dialog(&self, d: Dialog) -> Result<Dialog> {
        let mut tx = self.engine.db.begin().await?;
        let now = tx.now().await?;
        if let Some(draft_id) = d.draft_id {
            let draft: Draft = get(&mut *tx, draft_id).await?;
            if draft.saved_secret.is_some()
                || draft.expires_at <= now
                || draft.created_at + 3600 <= now
            {
                return Err(RuleError::Expired.into());
            }
        } else if d.expires_at <= now {
            return Err(RuleError::Expired.into());
        }
        if let Some(id) = d.plan_id {
            let p: Plan = get(&mut *tx, id).await?;
            let profile: Profile = get(&mut *tx, p.profile_id).await?;
            if d.owner_epoch != Some(profile.owner_epoch) {
                return Err(RuleError::StaleAction.into());
            }
        }
        Ok(d)
    }
    async fn home(&self, a: &Account) -> Result<()> {
        self.menu(a, Menu::Home, None).await
    }
    pub async fn handle(&self, bot_id: i64, update: &Value) -> Result<()> {
        let update_id = update["update_id"].as_i64().ok_or(Error::InvalidInput)?;
        let event = Id::from_u128(((bot_id as u128) << 64) | update_id as u64 as u128);
        let message = update.get("message");
        let callback = update.get("callback_query");
        let (from, chat) = if let Some(m) = message {
            (&m["from"], &m["chat"])
        } else if let Some(c) = callback {
            (&c["from"], &c["message"]["chat"])
        } else {
            return Ok(());
        };
        if chat["type"] != "private" || from["is_bot"] == true {
            return Ok(());
        }
        let telegram_id = from["id"].as_i64().ok_or(Error::InvalidInput)?;
        let chat_id = chat["id"].as_i64().ok_or(Error::InvalidInput)?;
        let language = from["language_code"].as_str().unwrap_or("en");
        let a = match self.engine.account(telegram_id, chat_id, language).await {
            Ok(account) => account,
            Err(Error::RateLimited | Error::Rule(RuleError::QuotaExceeded)) => {
                let submission = message
                    .and_then(|m| m["text"].as_str())
                    .and_then(recovery_submission);
                let recovery = if let Some((_, key)) = submission {
                    match ArgonHasher::selector(key) {
                        Ok(selector) => {
                            self.engine
                                .account_for_recovery(telegram_id, chat_id, language, selector, key)
                                .await
                        }
                        Err(error) => Err(error),
                    }
                } else {
                    Err(Error::Rule(RuleError::QuotaExceeded))
                };
                match recovery {
                    Ok(account) => account,
                    Err(error)
                        if !matches!(error, Error::Storage | Error::Internal | Error::Crypto) =>
                    {
                        if submission.is_some()
                            && let Some(message_id) = message.and_then(|m| m["message_id"].as_i64())
                        {
                            let _ = self.telegram.delete(chat_id, message_id).await;
                        }
                        if self
                            .engine
                            .limit("admission-feedback", 20, 60)
                            .await
                            .is_ok()
                        {
                            let key = if submission.is_some() {
                                "recovery-admission-failed"
                            } else {
                                "admission-full"
                            };
                            let _ = self
                                .telegram
                                .send_text(chat_id, &tr(language, key), vec![], vec![], None)
                                .await;
                        }
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        let mut tx = self.engine.db.begin().await?;
        if tx.get(Kind::HandledEvent, event).await?.is_some() {
            return Ok(());
        }
        tx.commit().await?;
        // Telegram names are presentation metadata; an unavailable refresh must
        // never prevent a protective control from reaching the Engine.
        if let Some(first_name) = from["first_name"].as_str() {
            let _ = self
                .engine
                .update_account_display(
                    a.id,
                    Some(first_name),
                    from["last_name"].as_str(),
                    from["username"].as_str(),
                )
                .await;
        }
        let result = if let Some(c) = callback {
            if let Some(id) = c["id"].as_str() {
                let (_, result) = tokio::join!(
                    self.telegram.answer_callback(id),
                    self.callback(&a, c, event)
                );
                result
            } else {
                self.callback(&a, c, event).await
            }
        } else {
            self.message(&a, message.ok_or(Error::InvalidInput)?, event)
                .await
        };
        if let Err(error) = result {
            if matches!(error, Error::Storage | Error::Internal | Error::Crypto) {
                return Err(error);
            }
            // A blocked recipient or an uncertain UI reply must not poison the
            // shared inbox. Domain/storage failures above remain retryable.
            let feedback_allowed = if matches!(error, Error::RateLimited) {
                match self
                    .engine
                    .limit(&format!("ui-limit-notice:{}", a.id), 1, 60)
                    .await
                {
                    Ok(()) => true,
                    Err(Error::RateLimited) => false,
                    Err(error) => return Err(error),
                }
            } else {
                true
            };
            if feedback_allowed && !matches!(error, Error::MessageUnavailable) {
                let key = match error {
                    Error::RateLimited => "rate-limited",
                    Error::InvalidCode => "invalid-code",
                    Error::Rule(RuleError::Expired | RuleError::StaleAction) => "stale-action",
                    Error::Rule(RuleError::NotReady) => "not-ready",
                    Error::Rule(RuleError::QuotaExceeded) => "quota-exceeded",
                    Error::Rule(RuleError::InvalidPolicy) => "invalid-policy",
                    Error::Rule(RuleError::InvalidState) => "invalid-state",
                    _ => "invalid-input",
                };
                let mut buttons = Vec::new();
                if let Some(d) = self.active_draft(&a).await? {
                    buttons.push(
                        self.draft_button(&a, &d, "continue", "continue-draft")
                            .await?,
                    );
                }
                buttons.push(self.nav(&a, "home").await?);
                match self.say(&a, key, buttons).await {
                    Ok(_) | Err(Error::MessageUnavailable) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        let mut tx = self.engine.db.begin().await?;
        let at = tx.now().await?;
        put(
            &mut *tx,
            None,
            &HandledEvent {
                id: event,
                actor_id: a.id,
                at,
            },
        )
        .await?;
        tx.commit().await
    }
    async fn callback(&self, a: &Account, c: &Value, _event: Id) -> Result<()> {
        let id = Id::parse_str(c["data"].as_str().ok_or(Error::InvalidInput)?)
            .map_err(|_| Error::InvalidInput)?;
        // The action or its parent can have been consumed by a committed mutation.
        // Return the actor-bound saved outcome before checking old UI epochs.
        match self.engine.operation_receipt(a.id, id).await {
            Ok(_) => {
                self.engine
                    .limit(&format!("action:{}", a.id), 30, 60)
                    .await?;
                return self
                    .history_page(a, 0, c["message"]["message_id"].as_i64())
                    .await;
            }
            Err(Error::NotFound | Error::Rule(RuleError::Expired | RuleError::AccessDenied)) => {}
            Err(error) => return Err(error),
        }
        let mut tx = self.engine.db.begin().await?;
        let action: Action = get(&mut *tx, id).await?;
        if action.actor_id != a.id || action.expires_at <= tx.now().await? {
            return Err(RuleError::StaleAction.into());
        }
        if action.name.starts_with("draft:seal:")
            && let Some(draft) = action.target
        {
            tx.commit().await?;
            match self.engine.operation_receipt(a.id, draft).await {
                Ok(receipt) if receipt.operation == OperationKind::SecretSaved => {
                    self.engine
                        .limit(&format!("action:{}", a.id), 30, 60)
                        .await?;
                    return self
                        .history_page(a, 0, c["message"]["message_id"].as_i64())
                        .await;
                }
                Ok(_) | Err(Error::NotFound | Error::Rule(RuleError::Expired)) => {}
                Err(error) => return Err(error),
            }
            tx = self.engine.db.begin().await?;
        }
        if action.used && !["confirm-person", "setup-confirm"].contains(&action.name.as_str()) {
            return Err(RuleError::StaleAction.into());
        }
        if let Some(plan_id) = action.plan_id {
            let plan: Plan = get(&mut *tx, plan_id).await?;
            let profile: Profile = get(&mut *tx, plan.profile_id).await?;
            if action
                .owner_epoch
                .is_some_and(|epoch| epoch != profile.owner_epoch || profile.owner_id != a.id)
            {
                return Err(RuleError::AccessDenied.into());
            }
            // STOP/check-in are always available for the current owner, even from an old menu.
            if ![
                "stop",
                "stop-secret",
                "stop-confirmed",
                "stop-secret-confirmed",
                "stop-cancel",
                "checkin",
                "ack-grant",
                "ack-recovery",
                "ack-claim",
            ]
            .contains(&action.name.as_str())
                && action.epoch != Some(plan.epoch)
            {
                return Err(RuleError::StaleAction.into());
            }
        }
        tx.commit().await?;
        let priority = [
            "stop",
            "stop-secret",
            "stop-confirmed",
            "stop-secret-confirmed",
            "stop-cancel",
            "checkin",
        ]
        .contains(&action.name.as_str());
        if !priority {
            self.engine
                .limit(&format!("action:{}", a.id), 30, 60)
                .await?;
        }
        let plan = action.plan_id;
        let target = action.target;
        let menu_message = c["message"]["message_id"].as_i64();
        if !priority
            && !action.name.starts_with("draft:")
            && ![
                "continue-setup",
                "new-secret",
                "create",
                "setup-continue",
                "setup-recovery-refresh",
            ]
            .contains(&action.name.as_str())
        {
            self.suspend_draft_question(a).await?;
        }
        // A second tap should show this person's current state, without another
        // mutation or a generic error that strands the owner outside setup.
        if action.used {
            let mut tx = self.engine.db.begin().await?;
            tx.remove(Kind::Dialog, a.id).await?;
            tx.commit().await?;
            if action.name == "setup-confirm" {
                return self
                    .setup_people(a, plan.ok_or(Error::InvalidInput)?, None)
                    .await;
            }
            return self
                .person_card(
                    a,
                    plan.ok_or(Error::InvalidInput)?,
                    target.ok_or(Error::InvalidInput)?,
                    menu_message,
                )
                .await;
        }
        match action.name.as_str() {
            "create" => {
                match self.engine.own_plan(a.id).await {
                    Ok(_) => {}
                    Err(Error::NotFound) => {
                        self.engine.create_profile(a.id).await?;
                    }
                    Err(error) => return Err(error),
                }
                self.continue_setup(a, None).await?;
                if let Some(id) = menu_message {
                    let _ = self.telegram.clear_menu(a.chat_id, id).await;
                }
            }
            "home" => self.menu(a, Menu::Home, menu_message).await?,
            "continue-setup" => {
                self.continue_setup(a, None).await?;
                if let Some(id) = menu_message {
                    let _ = self.telegram.clear_menu(a.chat_id, id).await;
                }
            }
            name if name.starts_with("setup-") => {
                self.setup_callback(a, &action, menu_message).await?
            }
            "plan-menu" => self.menu(a, Menu::Plan, menu_message).await?,
            "settings" => self.menu(a, Menu::Settings, menu_message).await?,
            "language" => self.menu(a, Menu::Language, menu_message).await?,
            "recovery-options" => self.menu(a, Menu::Recovery, menu_message).await?,
            "lang-uk" | "lang-en" => {
                self.engine
                    .locale(a.id, if action.name == "lang-uk" { "uk" } else { "en" })
                    .await?;
                let mut a = a.clone();
                a.locale = action.name[5..].into();
                self.menu(&a, Menu::Language, menu_message).await?;
            }
            "stop" => {
                self.stop_review(a, plan.ok_or(Error::InvalidInput)?, None, menu_message)
                    .await?
            }
            "checkin" | "stop-confirmed" | "resume" => {
                let (op, key) = match action.name.as_str() {
                    "checkin" => (Control::CheckIn, "checked-in"),
                    "stop-confirmed" => (Control::Stop, "stopped"),
                    _ => (Control::Rearm, "armed"),
                };
                self.engine
                    .control(a.id, plan.ok_or(Error::InvalidInput)?, action.id, op)
                    .await?;
                if priority {
                    self.control_feedback(a, plan, action.id, key).await?;
                } else {
                    self.menu(a, Menu::Home, menu_message).await?;
                }
            }
            "participants" => {
                self.people_page(a, plan.ok_or(Error::InvalidInput)?, 0, menu_message)
                    .await?
            }
            "invite" => {
                let id = self
                    .engine
                    .invite_with_id(a.id, plan.ok_or(Error::InvalidInput)?, action.id)
                    .await?;
                self.screen(
                    a,
                    &format!(
                        "https://t.me/{}?start=invite_{}",
                        self.username,
                        id.simple()
                    ),
                    vec![
                        self.button(a, "participants", None, plan, true, &tr(&a.locale, "back"))
                            .await?,
                    ],
                    menu_message,
                )
                .await?;
            }
            "confirm-person" => {
                self.engine
                    .confirm_participant(
                        a.id,
                        plan.ok_or(Error::InvalidInput)?,
                        target.ok_or(Error::InvalidInput)?,
                    )
                    .await?;
                self.person_confirmed(a, plan.unwrap(), target.unwrap(), menu_message)
                    .await?;
            }
            "new-secret" => {
                self.start_draft(a, plan.ok_or(Error::InvalidInput)?, None)
                    .await?;
                if let Some(id) = menu_message {
                    let _ = self.telegram.clear_menu(a.chat_id, id).await;
                }
            }
            name if name.starts_with("draft:") => {
                self.draft_callback(a, &action, menu_message).await?;
            }
            "ack-grant" => {
                self.engine
                    .acknowledge_grant(a.id, target.ok_or(Error::InvalidInput)?)
                    .await?;
                self.delete_callback(a, c).await?;
            }
            "ack-recovery" => {
                self.engine
                    .acknowledge_recovery(
                        a.id,
                        plan.ok_or(Error::InvalidInput)?,
                        target.ok_or(Error::InvalidInput)?,
                    )
                    .await?;
                self.delete_callback(a, c).await?;
                let (_, owned) = self.engine.own_plan(a.id).await?;
                if owned.state == domain::PlanState::Setup {
                    match self.continue_setup(a, None).await {
                        Ok(()) | Err(Error::RateLimited | Error::MessageUnavailable) => {}
                        Err(error) => return Err(error),
                    }
                }
            }
            "ack-claim" => {
                self.engine
                    .acknowledge_claim(a.id, target.ok_or(Error::InvalidInput)?, action.id)
                    .await?;
                self.delete_callback(a, c).await?;
                self.say(a, "recovered", vec![]).await?;
            }
            "submit-code" => {
                let case_id = target.ok_or(Error::InvalidInput)?;
                let mut tx = self.engine.db.begin().await?;
                let case: CaseRecord = get(&mut *tx, case_id).await?;
                let secret: Secret = get(&mut *tx, case.secret_id).await?;
                if !secret.policy.guardians.contains(&a.id)
                    || case.case.state != CaseState::Collecting
                {
                    return Err(RuleError::AccessDenied.into());
                }
                let plan_record: Plan = get(&mut *tx, secret.plan_id).await?;
                let profile: Profile = get(&mut *tx, plan_record.profile_id).await?;
                let owner: Account = get(&mut *tx, profile.owner_id).await?;
                let prompt = format!(
                    "{}\n\n{}: {}\n{}: {}",
                    tr(&a.locale, "code-prompt"),
                    tr(&a.locale, "owner-label"),
                    owner.telegram_id,
                    tr(&a.locale, "secret-reference"),
                    secret.id
                );
                tx.commit().await?;
                let mut d = self.dialog(a, "code", plan, None).await?;
                d.case_id = Some(case_id);
                d.reply_to = Some(match self.telegram.send_prompt(a.chat_id, &prompt).await {
                    SendResult::Sent(id) => id,
                    _ => return Err(Error::MessageUnavailable),
                });
                self.store_dialog(&d).await?;
            }
            "guardians" => self.inbox_page(a, 0, menu_message).await?,
            "recover" | "recoverstop" => {
                self.dialog(a, &action.name, None, None).await?;
                self.say(
                    a,
                    "recovery-prompt",
                    vec![self.back(a, "recovery-options").await?],
                )
                .await?;
            }
            "rotate-recovery" => {
                self.engine
                    .rotate_recovery(a.id, plan.ok_or(Error::InvalidInput)?, action.id)
                    .await?;
                self.say(a, "recovery-pending", vec![self.back(a, "settings").await?])
                    .await?;
            }
            "resend-code" => {
                self.engine
                    .resend_grant(a.id, target.ok_or(Error::InvalidInput)?)
                    .await?;
            }
            "retry-delivery" => {
                self.say(
                    a,
                    "retry-warning",
                    vec![self.b(a, "retry-confirmed", target, plan, false).await?],
                )
                .await?;
            }
            "retry-confirmed" => {
                self.engine
                    .retry_delivery(a.id, target.ok_or(Error::InvalidInput)?)
                    .await?;
                self.say(a, "confirmed", vec![]).await?;
            }
            "request-cancel" => {
                self.engine
                    .request_cancellation(a.id, plan.ok_or(Error::InvalidInput)?, target)
                    .await?;
                self.say(a, "confirmed", vec![]).await?;
            }
            "vote-cancel" => {
                self.engine
                    .vote_cancel(a.id, target.ok_or(Error::InvalidInput)?)
                    .await?;
                self.say(a, "confirmed", vec![]).await?;
            }
            "secrets" => {
                self.secrets_page(a, plan.ok_or(Error::InvalidInput)?, 0, menu_message)
                    .await?
            }
            "delete" => {
                let request = self
                    .engine
                    .prepare_deletion(a.id, plan.ok_or(Error::InvalidInput)?, target, action.id)
                    .await?;
                self.screen(
                    a,
                    &tr(
                        &a.locale,
                        if target.is_some() {
                            "delete-secret-confirm"
                        } else {
                            "delete-confirm"
                        },
                    ),
                    vec![
                        self.b(a, "delete-confirmed", Some(request.id), plan, true)
                            .await?,
                        self.back(a, "home").await?,
                    ],
                    menu_message,
                )
                .await?;
            }
            "delete-confirmed" => {
                self.engine
                    .confirm_deletion(a.id, target.ok_or(Error::InvalidInput)?, action.id)
                    .await?;
                self.say(
                    a,
                    "deleted",
                    vec![self.nav(a, "history").await?, self.nav(a, "home").await?],
                )
                .await?;
            }
            _ if self.workspace_callback(a, &action, menu_message).await? => {}
            _ => return Err(Error::InvalidInput),
        }
        // One-shot UI actions. Critical engine operations additionally have semantic idempotency.
        let mut tx = self.engine.db.begin().await?;
        if let Some(raw) = tx.get(Kind::Action, action.id).await? {
            let mut action: Action = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            action.used = true;
            put(&mut *tx, action.plan_id, &action).await?;
        }
        tx.commit().await
    }
    async fn delete_callback(&self, a: &Account, c: &Value) -> Result<()> {
        let id = c["message"]["message_id"]
            .as_i64()
            .ok_or(Error::InvalidInput)?;
        let mut tx = self.engine.db.begin().await?;
        let now = tx.now().await?;
        enqueue(
            &mut *tx,
            None,
            Task::CleanupMessage {
                chat_id: a.chat_id,
                message_id: id,
                sent_at: c["message"]["date"].as_i64().unwrap_or(now),
                account_id: a.id,
            },
            now,
            now + DAY,
            0,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
    async fn message(&self, a: &Account, m: &Value, event: Id) -> Result<()> {
        let text = m["text"].as_str().unwrap_or("");
        let source = (
            a.chat_id,
            m["message_id"].as_i64().ok_or(Error::InvalidInput)?,
        );
        // Reply-keyboard labels are navigation, including after a language switch;
        // never interpret them as secret content or a code response.
        for locale in crate::localization::LOCALES {
            if text == tr(locale, "nav-home") || text == tr(locale, "nav-continue") {
                self.engine
                    .limit(&format!("action:{}", a.id), 30, 60)
                    .await?;
                return if text == tr(locale, "nav-home") {
                    self.home(a).await
                } else {
                    self.continue_setup(a, None).await
                };
            }
        }
        self.cleanup_sensitive_input(a, m, event).await?;
        if text.starts_with('/') {
            let mut words = text.split_whitespace();
            let command = words.next().unwrap_or("").split('@').next().unwrap_or("");
            let emergency_owner = if matches!(command, "/stop" | "/checkin") {
                match self.engine.own_plan(a.id).await {
                    Ok(_) => true,
                    Err(Error::NotFound) => false,
                    Err(error) => return Err(error),
                }
            } else {
                false
            };
            if !emergency_owner && recovery_submission(text).is_none() {
                self.engine
                    .limit(&format!("action:{}", a.id), 30, 60)
                    .await?;
            }
            match command {
                "/start" => {
                    self.install_navigation(a).await?;
                    if let Some(invite) = words.next().and_then(|w| w.strip_prefix("invite_")) {
                        self.invitation_screen(
                            a,
                            Id::parse_str(invite).map_err(|_| Error::InvalidInput)?,
                            None,
                        )
                        .await?;
                    } else {
                        let view = self.engine.overview(a.id).await?;
                        if view
                            .own
                            .is_some_and(|p| p.state == domain::PlanState::Setup)
                        {
                            self.continue_setup(a, None).await?;
                        } else {
                            self.home(a).await?;
                        }
                    }
                }
                "/continue" => self.continue_setup(a, None).await?,
                "/stop" => {
                    let (_, plan) = self.engine.own_plan(a.id).await?;
                    self.stop_review(a, plan.id, None, None).await?;
                }
                "/checkin" | "/resume" => {
                    let (_, plan) = self.engine.own_plan(a.id).await?;
                    let (op, key) = match command {
                        "/checkin" => (Control::CheckIn, "checked-in"),
                        _ => (Control::Rearm, "armed"),
                    };
                    self.engine.control(a.id, plan.id, event, op).await?;
                    if emergency_owner {
                        self.control_feedback(a, Some(plan.id), event, key).await?;
                    } else {
                        self.menu(a, Menu::Home, None).await?;
                    }
                }
                "/recover" | "/recoverstop" => {
                    self.suspend_draft_question(a).await?;
                    if let Some((command, key)) = recovery_submission(text) {
                        return self
                            .recover_key(a, key, command == "/recoverstop", event)
                            .await;
                    }
                    self.dialog(
                        a,
                        if command == "/recover" {
                            "recover"
                        } else {
                            "recoverstop"
                        },
                        None,
                        None,
                    )
                    .await?;
                    self.say(a, "recovery-prompt", vec![]).await?;
                }
                "/guardians" => {
                    self.suspend_draft_question(a).await?;
                    self.guardians(a).await?;
                }
                "/help" => {
                    self.suspend_draft_question(a).await?;
                    self.screen(
                        a,
                        &tr(&a.locale, "help-text"),
                        vec![
                            self.nav(a, "service-status").await?,
                            self.nav(a, "privacy").await?,
                            self.nav(a, "home").await?,
                        ],
                        None,
                    )
                    .await?;
                }
                "/settings" => self.menu(a, Menu::Settings, None).await?,
                "/status" => {
                    self.suspend_draft_question(a).await?;
                    let (_, plan) = self.engine.own_plan(a.id).await?;
                    self.status(a, plan.id).await?;
                }
                _ => self.home(a).await?,
            }
            return Ok(());
        }
        if !text.starts_with("R1.") {
            self.engine
                .limit(&format!("action:{}", a.id), 30, 60)
                .await?;
        }
        let mut d = match self.load_dialog(a).await {
            Ok(d) => d,
            Err(Error::NotFound | Error::Rule(RuleError::Expired | RuleError::StaleAction)) => {
                self.say(a, "setup-use-buttons", vec![]).await?;
                return self.continue_setup(a, None).await;
            }
            Err(error) => return Err(error),
        };
        // Navigation can close a sensitive prompt while retaining a draft. A late
        // credential reply must never become draft content or a private label.
        if (text.trim_start().starts_with("R1.")
            && !matches!(d.step.as_str(), "recover" | "recoverstop"))
            || (text.trim_start().starts_with("Z1.") && d.step != "code")
        {
            return Err(RuleError::StaleAction.into());
        }
        if d.draft_id.is_some() && d.reply_to.is_none() {
            self.say(a, "setup-use-buttons", vec![]).await?;
            return self.render_draft(a, &d, None).await;
        }
        let input_step = if d.step == "builder" {
            if m["document"].is_object() {
                "file"
            } else {
                "text"
            }
        } else {
            d.step.as_str()
        }
        .to_owned();
        match input_step.as_str() {
            "label" => {
                self.engine
                    .set_label(
                        a.id,
                        d.plan_id.ok_or(Error::InvalidInput)?,
                        d.case_id.ok_or(Error::InvalidInput)?,
                        text,
                    )
                    .await?;
                self.label_saved(a, d.plan_id.unwrap(), d.case_id.unwrap())
                    .await?;
            }
            "timezone" => {
                let Some(offset) = workspace::parse_offset(text) else {
                    self.say(a, "timezone-prompt", vec![self.back(a, "settings").await?])
                        .await?;
                    return Ok(());
                };
                self.engine.set_utc_offset_minutes(a.id, offset).await?;
                self.menu(a, Menu::Settings, None).await?;
            }

            "text" | "copyable" | "spoiler" => {
                if text.is_empty() {
                    return Err(Error::InvalidInput);
                }
                let block = match input_step.as_str() {
                    "text" => Block::Text { text: text.into() },
                    "copyable" => Block::Copyable { text: text.into() },
                    _ => Block::Spoiler { text: text.into() },
                };
                self.engine
                    .append_block(a.id, d.draft_id.ok_or(Error::InvalidInput)?, block, source)
                    .await?;
                d.step = "builder".into();
                self.store_dialog(&d).await?;
                self.render_draft(a, &d, None).await?;
            }
            "file" => {
                let doc = &m["document"];
                if doc["file_size"]
                    .as_u64()
                    .is_none_or(|s| s > 10 * 1024 * 1024)
                {
                    return Err(Error::InvalidInput);
                }
                let id = doc["file_id"].as_str().ok_or(Error::InvalidInput)?;
                let draft_id = d.draft_id.ok_or(Error::InvalidInput)?;
                let metadata = serde_json::to_vec(&(
                    doc["file_name"].as_str().unwrap_or("file"),
                    m["caption"].as_str().unwrap_or(""),
                ))
                .map_err(|_| Error::Internal)?;
                let mut tx = self.engine.db.begin().await?;
                let now = tx.now().await?;
                if tx.get(Kind::Job, event).await?.is_none() {
                    put(
                        &mut *tx,
                        d.plan_id,
                        &Job {
                            id: event,
                            plan_id: d.plan_id,
                            task: Task::DownloadFile {
                                draft_id,
                                account_id: a.id,
                                file_id: self.engine.crypto.wrap(
                                    "telegram-file",
                                    draft_id,
                                    id.as_bytes(),
                                )?,
                                name: self.engine.crypto.wrap(
                                    "telegram-file-meta",
                                    draft_id,
                                    &metadata,
                                )?,
                                source_message: source.1,
                            },
                            state: PartState::Queued,
                            due_at: now,
                            expires_at: now + 900,
                            lease_until: 0,
                            lease_token: Id::nil(),
                            attempts: 0,
                            message_id: None,
                            priority: 4,
                        },
                    )
                    .await?;
                }
                d.step = "file-pending".into();
                put(
                    &mut *tx,
                    d.plan_id,
                    &DraftSession {
                        id: d.id,
                        dialog: d.clone(),
                    },
                )
                .await?;
                tx.commit().await?;
                self.render_draft(a, &d, None).await?;
            }
            "file-pending" => {
                self.say(a, "awaiting-file", vec![]).await?;
            }
            "timing-reminder" | "timing-inactivity" | "timing-wait" => {
                let value = text.trim().parse::<i64>().ok();
                let valid = value.is_some_and(|n| match d.step.as_str() {
                    "timing-reminder" | "timing-wait" => (1..=30).contains(&n),
                    _ => {
                        (2..=365).contains(&n)
                            && n >= 2 * d
                                .timing
                                .as_ref()
                                .unwrap_or(&Timing::default())
                                .reminder_seconds
                                / DAY
                    }
                });
                if !valid {
                    self.say(a, "invalid-timing", vec![]).await?;
                    self.render_draft(a, &d, None).await?;
                    return Ok(());
                }
                let mut timing = d.timing.clone().unwrap_or_default();
                let days = value.ok_or(Error::InvalidInput)?;
                match d.step.as_str() {
                    "timing-reminder" => {
                        timing.reminder_seconds = days * DAY;
                        d.step = "timing-inactivity".into();
                    }
                    "timing-inactivity" => {
                        timing.inactivity_seconds = days * DAY;
                        d.step = "timing-wait".into();
                    }
                    _ => {
                        timing.release_delay_seconds = days * DAY;
                        self.set_timing(a, &mut d, timing.clone()).await?;
                    }
                }
                d.timing = Some(timing);
                self.store_dialog(&d).await?;
                self.render_draft(a, &d, None).await?;
            }
            "timing" => {
                let timing = match drafts::parse_timing(text) {
                    Some(timing) => timing,
                    None => {
                        self.say(a, "invalid-timing", vec![]).await?;
                        self.render_draft(a, &d, None).await?;
                        return Ok(());
                    }
                };
                self.set_timing(a, &mut d, timing).await?;
                self.render_draft(a, &d, None).await?;
            }
            "code" => {
                if d.reply_to != m["reply_to_message"]["message_id"].as_i64() {
                    return Err(Error::InvalidInput);
                }
                self.engine
                    .submit_code(a.id, d.case_id.ok_or(Error::InvalidInput)?, text)
                    .await?;
                let mut tx = self.engine.db.begin().await?;
                tx.remove(Kind::Dialog, a.id).await?;
                tx.commit().await?;
                self.say(
                    a,
                    "code-accepted",
                    vec![self.nav(a, "guardians").await?, self.nav(a, "home").await?],
                )
                .await?;
            }
            "recover" | "recoverstop" => {
                self.recover_key(a, text, d.step == "recoverstop", event)
                    .await?;
            }
            _ if d.draft_id.is_some() => {
                self.say(a, "setup-use-buttons", vec![]).await?;
                self.render_draft(a, &d, None).await?;
            }
            _ => return Err(Error::InvalidInput),
        }
        Ok(())
    }
    async fn recover_key(&self, a: &Account, key: &str, stop: bool, event: Id) -> Result<()> {
        let selector = ArgonHasher::selector(key)?;
        let claim = self
            .engine
            .recover(a.id, selector, key, stop, event)
            .await?;
        let mut tx = self.engine.db.begin().await?;
        tx.remove(Kind::Dialog, a.id).await?;
        tx.commit().await?;
        self.control_feedback(
            a,
            None,
            event,
            if claim.is_some() {
                "recovery-pending"
            } else {
                "stopped"
            },
        )
        .await
    }
    async fn cleanup_sensitive_input(&self, a: &Account, m: &Value, event: Id) -> Result<()> {
        let text = m["text"].as_str().unwrap_or("");
        if text.starts_with('/') && recovery_submission(text).is_none() {
            return Ok(());
        }
        let mut tx = self.engine.db.begin().await?;
        let dialog = tx.get(Kind::Dialog, a.id).await?;
        let sensitive = recovery_submission(text).is_some()
            || text.trim_start().starts_with("R1.")
            || text.trim_start().starts_with("Z1.")
            || dialog
                .as_ref()
                .and_then(|d| d["step"].as_str())
                .is_some_and(|step| matches!(step, "code" | "recover" | "recoverstop" | "label"));
        if sensitive && tx.get(Kind::Job, event).await?.is_none() {
            let now = tx.now().await?;
            let job = Job {
                id: event,
                plan_id: None,
                task: Task::CleanupMessage {
                    chat_id: a.chat_id,
                    message_id: m["message_id"].as_i64().ok_or(Error::InvalidInput)?,
                    sent_at: m["date"].as_i64().unwrap_or(now),
                    account_id: a.id,
                },
                state: domain::PartState::Queued,
                due_at: now,
                expires_at: now + DAY,
                lease_until: 0,
                lease_token: Id::nil(),
                attempts: 0,
                message_id: None,
                priority: 0,
            };
            put(&mut *tx, None, &job).await?;
        }
        tx.commit().await
    }
    async fn track_preview(&self, a: &Account, d: &Dialog, message_id: i64) -> Result<()> {
        let mut tx = self.engine.db.begin().await?;
        let mut draft: Draft = get(&mut *tx, d.draft_id.ok_or(Error::InvalidInput)?).await?;
        draft.sources.push((a.chat_id, message_id));
        put(&mut *tx, d.plan_id, &draft).await?;
        tx.commit().await
    }
    async fn status(&self, a: &Account, plan: Id) -> Result<()> {
        self.secrets_page(a, plan, 0, None).await
    }
    async fn guardians(&self, a: &Account) -> Result<()> {
        self.inbox_page(a, 0, None).await
    }
    pub async fn process_job(&self, job: Job) -> Result<()> {
        match &job.task {
            Task::Deliver { .. } => {
                let (account, block, file) = self.engine.delivery_block(&job).await?;
                self.telegram.reserve_send(account.chat_id).await;
                if !self
                    .engine
                    .authorize_dispatch(&job, account.chat_id)
                    .await?
                {
                    return Ok(());
                }
                let result = self
                    .telegram
                    .send_block_reserved(account.chat_id, &block, file)
                    .await;
                self.engine
                    .finish_job(job.id, job.lease_token, result)
                    .await
            }
            Task::CleanupMessage {
                chat_id,
                message_id,
                ..
            } => {
                let result = match self.telegram.delete(*chat_id, *message_id).await {
                    DeleteResult::Deleted => SendResult::Sent(0),
                    DeleteResult::RetryAfter(seconds) => SendResult::RetryAfter(seconds),
                    DeleteResult::Permanent => SendResult::Permanent,
                };
                self.engine
                    .finish_job(job.id, job.lease_token, result)
                    .await
            }
            Task::DeleteObject { object_id } => {
                let mut tx = self.engine.db.begin().await?;
                let o: FileObject = get(&mut *tx, *object_id).await?;
                tx.commit().await?;
                self.engine.blobs.delete(&o.key).await?;
                self.engine
                    .finish_job(job.id, job.lease_token, SendResult::Sent(0))
                    .await
            }
            Task::DownloadFile {
                draft_id,
                account_id,
                file_id,
                name,
                source_message,
            } => {
                let file = self
                    .engine
                    .crypto
                    .unwrap("telegram-file", *draft_id, file_id)?;
                let file = std::str::from_utf8(&file).map_err(|_| Error::Crypto)?;
                let metadata = self
                    .engine
                    .crypto
                    .unwrap("telegram-file-meta", *draft_id, name)?;
                let (name, caption): (String, String) =
                    serde_json::from_slice(&metadata).map_err(|_| Error::Crypto)?;
                let bytes = self.telegram.download(file).await?;
                let mut tx = self.engine.db.begin().await?;
                let a: Account = get(&mut *tx, *account_id).await?;
                tx.commit().await?;
                self.engine
                    .append_file(
                        *account_id,
                        *draft_id,
                        name,
                        caption,
                        bytes,
                        (a.chat_id, *source_message),
                    )
                    .await?;
                // Content durability must not depend on the current UI prompt.
                self.engine
                    .finish_job(job.id, job.lease_token, SendResult::Sent(0))
                    .await?;
                if let Some(mut d) = self.active_draft(&a).await?
                    && d.draft_id == Some(*draft_id)
                {
                    d.step = "builder".into();
                    self.store_dialog(&d).await?;
                    self.say(&a, "block-saved", vec![]).await?;
                    let mut tx = self.engine.db.begin().await?;
                    let prompt_open = tx.get(Kind::Dialog, a.id).await?.is_some();
                    tx.commit().await?;
                    if !prompt_open && d.reply_to.is_some() {
                        self.render_draft(&a, &d, None).await?;
                    }
                }
                Ok(())
            }
            _ => self.send_notice_job(job).await,
        }
    }
    async fn send_notice_job(&self, job: Job) -> Result<()> {
        let mut tx = self.engine.db.begin().await?;
        let mut guardian_secret = None;
        let (account, key, code, action, target) = match &job.task {
            Task::Notice {
                account_id, key, ..
            } => (
                get::<Account>(&mut *tx, *account_id).await?,
                key.as_str(),
                None,
                None,
                None,
            ),
            Task::GuardianCode { grant_id } => {
                let g: GuardianGrant = get(&mut *tx, *grant_id).await?;
                guardian_secret = Some(g.secret_id);
                if g.ready || g.expires_at <= tx.now().await? {
                    return Ok(());
                }
                let code = self.engine.crypto.unwrap(
                    "grant",
                    g.id,
                    g.delivery.as_ref().ok_or(Error::NotFound)?,
                )?;
                (
                    get::<Account>(&mut *tx, g.account_id).await?,
                    "guardian-key",
                    Some(code),
                    Some("ack-grant"),
                    Some(g.id),
                )
            }
            Task::RecoveryCode {
                profile_id,
                account_id,
                envelope,
                selector,
            } => {
                let p: Profile = get(&mut *tx, *profile_id).await?;
                if p.recovery_saved || p.recovery_selector != *selector || p.owner_id != *account_id
                {
                    return Ok(());
                }
                (
                    get::<Account>(&mut *tx, *account_id).await?,
                    "recovery-key",
                    Some(self.engine.crypto.unwrap("recovery", *selector, envelope)?),
                    Some("ack-recovery"),
                    Some(*selector),
                )
            }
            Task::ClaimCode { claim_id } => {
                let c: Claim = get(&mut *tx, *claim_id).await?;
                let p: Profile = get(&mut *tx, c.profile_id).await?;
                if p.pending_claim != Some(c.id) || c.expires_at <= tx.now().await? {
                    return Ok(());
                }
                (
                    c.target,
                    "recovery-key",
                    Some(self.engine.crypto.unwrap("claim", c.id, &c.delivery)?),
                    Some("ack-claim"),
                    Some(c.id),
                )
            }
            Task::GuardianRequest {
                case_id,
                account_id,
            } => {
                let c: CaseRecord = get(&mut *tx, *case_id).await?;
                guardian_secret = Some(c.secret_id);
                if c.case.state != CaseState::Collecting {
                    return Ok(());
                }
                (
                    get::<Account>(&mut *tx, *account_id).await?,
                    "guardian-request",
                    None,
                    Some("submit-code"),
                    Some(*case_id),
                )
            }
            _ => return Err(Error::InvalidInput),
        };
        let context = if let Some(secret_id) = guardian_secret {
            let secret: Secret = get(&mut *tx, secret_id).await?;
            let plan: Plan = get(&mut *tx, secret.plan_id).await?;
            let profile: Profile = get(&mut *tx, plan.profile_id).await?;
            let owner: Account = get(&mut *tx, profile.owner_id).await?;
            Some(format!(
                "{}: {}\n{}: {}",
                tr(&account.locale, "owner-label"),
                owner.telegram_id,
                tr(&account.locale, "secret-reference"),
                secret_id
            ))
        } else {
            None
        };
        tx.commit().await?;
        let mut text = Zeroizing::new(tr(&account.locale, key));
        if let Some(context) = context {
            text.push_str("\n\n");
            text.push_str(&context);
        }
        let mut entities = vec![];
        let mut buttons = vec![];
        if let Some(code) = code {
            let code = std::str::from_utf8(&code).map_err(|_| Error::Crypto)?;
            text.push_str("\n\n");
            let offset = text.encode_utf16().count();
            text.push_str(code);
            let mut e = entity(code, MessageEntityKind::Pre { language: None });
            e.offset = offset;
            entities.push(e);
        }
        if let Some(action) = action {
            buttons.push(
                self.button(
                    &account,
                    action,
                    target,
                    job.plan_id,
                    action == "ack-recovery",
                    &tr(
                        &account.locale,
                        if action == "submit-code" {
                            "submit-code"
                        } else {
                            "saved-delete"
                        },
                    ),
                )
                .await?,
            );
        } else if matches!(&job.task, Task::Notice { .. }) {
            let destination = match key {
                "participant-joined" => Some(("setup-people", "setup-check-people")),
                "secret-armed" => Some(("setup-ready", "setup-check-readiness")),
                _ => None,
            };
            let button = if let Some((action, label)) = destination
                && let Some(plan) = job.plan_id
                && self
                    .engine
                    .overview(account.id)
                    .await?
                    .own
                    .is_some_and(|p| p.id == plan)
            {
                self.button(
                    &account,
                    action,
                    None,
                    Some(plan),
                    true,
                    &tr(&account.locale, label),
                )
                .await
            } else {
                self.nav(&account, "home").await
            };
            match button {
                Ok(button) => buttons.push(button),
                Err(Error::RateLimited) => {}
                Err(error) => return Err(error),
            }
        }
        self.telegram.reserve_send(account.chat_id).await;
        if !self
            .engine
            .authorize_dispatch(&job, account.chat_id)
            .await?
        {
            return Ok(());
        }
        let result = self
            .telegram
            .send_text_reserved(account.chat_id, &text, entities, buttons, None)
            .await;
        self.engine
            .finish_job(job.id, job.lease_token, result)
            .await
    }
}

fn recovery_submission(text: &str) -> Option<(&str, &str)> {
    let mut words = text.split_whitespace();
    let command = words.next()?.split('@').next()?;
    if !matches!(command, "/recover" | "/recoverstop") {
        return None;
    }
    let key = words.next()?;
    words.next().is_none().then_some((command, key))
}
