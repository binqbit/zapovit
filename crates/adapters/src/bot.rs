//! Telegram conversation adapter. Business authority is rechecked by Engine commands.
use crate::{
    crypto::ArgonHasher,
    localization::tr,
    telegram::{DeleteResult, Telegram, entity},
};
use application::*;
use domain::{Block, CaseState, DAY, Id, Policy, RuleError, SecretState, Timing};
use serde_json::Value;
use teloxide_core::types::MessageEntityKind;
use zeroize::Zeroizing;

mod drafts;
mod menus;
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
    async fn text(&self, a: &Account, text: &str, buttons: Vec<(String, String)>) -> Result<i64> {
        match self
            .telegram
            .send_text(a.chat_id, text, vec![], buttons, None)
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
            expires_at: now + DAY,
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
        };
        put(&mut *tx, plan, &d).await?;
        tx.commit().await?;
        Ok(d)
    }
    async fn store_dialog(&self, d: &Dialog) -> Result<()> {
        let mut tx = self.engine.db.begin().await?;
        put(&mut *tx, d.plan_id, d).await?;
        tx.commit().await
    }
    async fn load_dialog(&self, a: &Account) -> Result<Dialog> {
        let mut tx = self.engine.db.begin().await?;
        let d: Dialog = get(&mut *tx, a.id).await?;
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
        let a = self
            .engine
            .account(
                telegram_id,
                chat["id"].as_i64().ok_or(Error::InvalidInput)?,
                from["language_code"].as_str().unwrap_or("en"),
            )
            .await?;
        let mut tx = self.engine.db.begin().await?;
        if tx.get(Kind::HandledEvent, event).await?.is_some() {
            return Ok(());
        }
        tx.commit().await?;
        let result = if let Some(c) = callback {
            if let Some(id) = c["id"].as_str() {
                self.telegram.answer_callback(id).await;
            }
            self.callback(&a, c, event).await
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
    async fn callback(&self, a: &Account, c: &Value, event: Id) -> Result<()> {
        let id = Id::parse_str(c["data"].as_str().ok_or(Error::InvalidInput)?)
            .map_err(|_| Error::InvalidInput)?;
        let mut tx = self.engine.db.begin().await?;
        let action: Action = get(&mut *tx, id).await?;
        if action.actor_id != a.id || action.expires_at <= tx.now().await? || action.used {
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
            if !["stop", "checkin", "ack-grant", "ack-recovery", "ack-claim"]
                .contains(&action.name.as_str())
                && action.epoch != Some(plan.epoch)
            {
                return Err(RuleError::StaleAction.into());
            }
        }
        tx.commit().await?;
        let priority = ["stop", "checkin"].contains(&action.name.as_str());
        if !priority {
            self.engine
                .limit(&format!("action:{}", a.id), 30, 60)
                .await?;
        }
        let plan = action.plan_id;
        let target = action.target;
        let menu_message = c["message"]["message_id"].as_i64();
        match action.name.as_str() {
            "create" => {
                match self.engine.own_plan(a.id).await {
                    Ok(_) => {}
                    Err(Error::NotFound) => {
                        self.engine.create_profile(a.id).await?;
                    }
                    Err(error) => return Err(error),
                }
                self.say(a, "created", vec![self.nav(a, "plan-menu").await?])
                    .await?;
            }
            "home" => self.menu(a, Menu::Home, menu_message).await?,
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
            "checkin" | "stop" | "resume" => {
                let (op, key) = match action.name.as_str() {
                    "checkin" => (Control::CheckIn, "checked-in"),
                    "stop" => (Control::Stop, "stopped"),
                    _ => (Control::Rearm, "armed"),
                };
                self.engine
                    .control(a.id, plan.ok_or(Error::InvalidInput)?, action.id, op)
                    .await?;
                self.say(a, key, vec![self.nav(a, "home").await?]).await?;
            }
            "participants" => {
                self.participants(a, plan.ok_or(Error::InvalidInput)?)
                    .await?
            }
            "invite" => {
                let id = self
                    .engine
                    .invite(a.id, plan.ok_or(Error::InvalidInput)?)
                    .await?;
                self.text(
                    a,
                    &format!(
                        "https://t.me/{}?start=invite_{}",
                        self.username,
                        id.simple()
                    ),
                    vec![self.back(a, "plan-menu").await?],
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
                self.say(a, "confirmed", vec![self.back(a, "plan-menu").await?])
                    .await?;
            }
            "new-secret" => {
                self.start_draft(a, plan.ok_or(Error::InvalidInput)?, menu_message)
                    .await?;
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
            }
            "ack-claim" => {
                self.engine
                    .acknowledge_claim(a.id, target.ok_or(Error::InvalidInput)?, event)
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
            "guardians" => self.guardians(a).await?,
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
            "secrets" => self.status(a, plan.ok_or(Error::InvalidInput)?).await?,
            "delete" => {
                self.say(
                    a,
                    if target.is_some() {
                        "delete-secret-confirm"
                    } else {
                        "delete-confirm"
                    },
                    vec![
                        self.b(a, "delete-confirmed", target, plan, true).await?,
                        self.back(
                            a,
                            if target.is_some() {
                                "plan-menu"
                            } else {
                                "settings"
                            },
                        )
                        .await?,
                    ],
                )
                .await?;
            }
            "delete-confirmed" => {
                self.engine
                    .control(
                        a.id,
                        plan.ok_or(Error::InvalidInput)?,
                        action.id,
                        target
                            .map(|secret_id| Control::DeleteSecret { secret_id })
                            .unwrap_or(Control::DeleteProfile),
                    )
                    .await?;
                self.say(a, "deleted", vec![self.nav(a, "home").await?])
                    .await?;
            }
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
            if !emergency_owner {
                self.engine
                    .limit(&format!("action:{}", a.id), 30, 60)
                    .await?;
            }
            match command {
                "/start" => {
                    if let Some(invite) = words.next().and_then(|w| w.strip_prefix("invite_")) {
                        self.engine
                            .accept_invite(
                                a.id,
                                Id::parse_str(invite).map_err(|_| Error::InvalidInput)?,
                            )
                            .await?;
                        self.say(a, "joined", vec![]).await?;
                    } else {
                        self.home(a).await?;
                    }
                }
                "/stop" | "/checkin" | "/resume" => {
                    let (_, plan) = self.engine.own_plan(a.id).await?;
                    let (op, key) = match command {
                        "/stop" => (Control::Stop, "stopped"),
                        "/checkin" => (Control::CheckIn, "checked-in"),
                        _ => (Control::Rearm, "armed"),
                    };
                    self.engine.control(a.id, plan.id, event, op).await?;
                    self.say(a, key, vec![]).await?;
                }
                "/recover" | "/recoverstop" => {
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
                "/guardians" => self.guardians(a).await?,
                "/help" => {
                    self.say(
                        a,
                        "help-text",
                        vec![self.nav(a, "home").await?, self.nav(a, "settings").await?],
                    )
                    .await?;
                }
                "/settings" => self.menu(a, Menu::Settings, None).await?,
                "/status" => {
                    let (_, plan) = self.engine.own_plan(a.id).await?;
                    self.status(a, plan.id).await?;
                }
                _ => self.home(a).await?,
            }
            return Ok(());
        }
        self.engine
            .limit(&format!("action:{}", a.id), 30, 60)
            .await?;
        let mut d = self.load_dialog(a).await?;
        match d.step.as_str() {
            "text" | "copyable" | "spoiler" => {
                if text.is_empty() {
                    return Err(Error::InvalidInput);
                }
                let block = match d.step.as_str() {
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
                enqueue(
                    &mut *tx,
                    d.plan_id,
                    Task::DownloadFile {
                        draft_id,
                        account_id: a.id,
                        file_id: self.engine.crypto.wrap(
                            "telegram-file",
                            draft_id,
                            id.as_bytes(),
                        )?,
                        name: self
                            .engine
                            .crypto
                            .wrap("telegram-file-meta", draft_id, &metadata)?,
                        source_message: source.1,
                    },
                    now,
                    now + 900,
                    4,
                )
                .await?;
                tx.commit().await?;
                d.step = "file-pending".into();
                self.store_dialog(&d).await?;
                self.say(a, "file-received", vec![]).await?;
            }
            "file-pending" => {
                self.say(a, "awaiting-file", vec![]).await?;
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
                self.say(a, "code-accepted", vec![]).await?;
            }
            "recover" | "recoverstop" => {
                let selector = ArgonHasher::selector(text)?;
                let claim = self
                    .engine
                    .recover(a.id, selector, text, d.step == "recoverstop", event)
                    .await?;
                self.say(
                    a,
                    if claim.is_some() {
                        "recovery-pending"
                    } else {
                        "stopped"
                    },
                    vec![],
                )
                .await?;
            }
            _ => return Err(Error::InvalidInput),
        }
        Ok(())
    }
    async fn cleanup_sensitive_input(&self, a: &Account, m: &Value, event: Id) -> Result<()> {
        let text = m["text"].as_str().unwrap_or("");
        if text.starts_with('/') {
            return Ok(());
        }
        let mut tx = self.engine.db.begin().await?;
        let dialog = tx.get(Kind::Dialog, a.id).await?;
        let sensitive = text.starts_with("R1.")
            || text.starts_with("Z1.")
            || dialog
                .as_ref()
                .and_then(|d| d["step"].as_str())
                .is_some_and(|step| matches!(step, "code" | "recover" | "recoverstop"));
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
    async fn participants(&self, a: &Account, plan: Id) -> Result<()> {
        let mut tx = self.engine.db.begin().await?;
        let people = list::<Participant>(&mut *tx, Some(plan)).await?;
        tx.commit().await?;
        for p in people {
            let mut tx = self.engine.db.begin().await?;
            let person: Account = get(&mut *tx, p.account_id).await?;
            tx.commit().await?;
            self.text(
                a,
                &format!(
                    "Telegram ID: {} · {}",
                    person.telegram_id,
                    if p.confirmed { "✓" } else { "—" }
                ),
                if p.confirmed {
                    vec![]
                } else {
                    vec![
                        self.b(a, "confirm-person", Some(p.id), Some(plan), true)
                            .await?,
                    ]
                },
            )
            .await?;
        }
        self.say(
            a,
            "participants",
            vec![
                self.b(a, "invite", None, Some(plan), true).await?,
                self.back(a, "plan-menu").await?,
            ],
        )
        .await?;
        Ok(())
    }
    async fn track_preview(&self, a: &Account, d: &Dialog, message_id: i64) -> Result<()> {
        let mut tx = self.engine.db.begin().await?;
        let mut draft: Draft = get(&mut *tx, d.draft_id.ok_or(Error::InvalidInput)?).await?;
        draft.sources.push((a.chat_id, message_id));
        put(&mut *tx, d.plan_id, &draft).await?;
        tx.commit().await
    }
    async fn status(&self, a: &Account, plan_id: Id) -> Result<()> {
        let mut tx = self.engine.db.begin().await?;
        let plan: Plan = get(&mut *tx, plan_id).await?;
        let profile: Profile = get(&mut *tx, plan.profile_id).await?;
        if profile.owner_id != a.id {
            return Err(RuleError::AccessDenied.into());
        }
        let secrets = list::<Secret>(&mut *tx, Some(plan_id)).await?;
        tx.commit().await?;
        self.text(
            a,
            &format!(
                "{}: {}\n{}: {}",
                tr(&a.locale, "status"),
                crate::localization::state(&a.locale, &plan.state),
                tr(&a.locale, "last-checkin"),
                drafts::display_time(plan.last_activity)
            ),
            vec![],
        )
        .await?;
        for secret in secrets
            .into_iter()
            .filter(|s| s.state != SecretState::Deleted)
        {
            self.text(
                a,
                &format!(
                    "{}\n{}\n{} / {}",
                    secret.id,
                    crate::localization::state(&a.locale, &secret.state),
                    secret.policy.threshold,
                    secret.policy.guardians.len()
                ),
                vec![
                    self.b(a, "delete", Some(secret.id), Some(plan_id), true)
                        .await?,
                ],
            )
            .await?;
        }
        self.say(a, "secrets", vec![self.back(a, "plan-menu").await?])
            .await?;
        Ok(())
    }
    async fn guardians(&self, a: &Account) -> Result<()> {
        let mut tx = self.engine.db.begin().await?;
        let secrets = list::<Secret>(&mut *tx, None).await?;
        let cancellations = list::<Cancellation>(&mut *tx, None).await?;
        tx.commit().await?;
        let mut shown = false;
        for secret in secrets
            .into_iter()
            .filter(|s| s.state != SecretState::Deleted && s.policy.guardians.contains(&a.id))
        {
            let mut buttons = vec![
                self.button(
                    a,
                    "request-cancel",
                    Some(secret.id),
                    Some(secret.plan_id),
                    false,
                    &tr(&a.locale, "cancel-secret"),
                )
                .await?,
            ];
            buttons.push(
                self.button(
                    a,
                    "request-cancel",
                    None,
                    Some(secret.plan_id),
                    false,
                    &tr(&a.locale, "cancel-plan"),
                )
                .await?,
            );
            let mut tx = self.engine.db.begin().await?;
            let grants = list::<GuardianGrant>(&mut *tx, Some(secret.id)).await?;
            tx.commit().await?;
            for grant in grants
                .into_iter()
                .filter(|g| g.account_id == a.id && !g.ready && g.delivery.is_some())
            {
                buttons.push(
                    self.b(
                        a,
                        "resend-code",
                        Some(grant.id),
                        Some(secret.plan_id),
                        false,
                    )
                    .await?,
                );
            }
            if let Some(case_id) = secret.last_case {
                let mut tx = self.engine.db.begin().await?;
                let c: CaseRecord = get(&mut *tx, case_id).await?;
                tx.commit().await?;
                if c.case.state == CaseState::Collecting {
                    buttons.push(
                        self.b(a, "submit-code", Some(case_id), Some(secret.plan_id), false)
                            .await?,
                    );
                }
            }
            self.text(
                a,
                &format!(
                    "{}: {}\n{}: {}",
                    tr(&a.locale, "secret-reference"),
                    secret.id,
                    tr(&a.locale, "status"),
                    crate::localization::state(&a.locale, &secret.state)
                ),
                buttons,
            )
            .await?;
            shown = true;
        }
        for c in cancellations
            .into_iter()
            .filter(|c| c.state == "open" && c.members.contains(&a.id))
        {
            self.text(
                a,
                &format!(
                    "{}\n{}: {} / {}",
                    tr(
                        &a.locale,
                        if c.secret_id.is_some() {
                            "cancel-secret"
                        } else {
                            "cancel-plan"
                        }
                    ),
                    tr(&a.locale, "cancellation-votes"),
                    c.votes.len(),
                    c.members.len()
                ),
                vec![
                    self.b(a, "vote-cancel", Some(c.id), Some(c.plan_id), false)
                        .await?,
                ],
            )
            .await?;
            shown = true;
        }
        let mut tx = self.engine.db.begin().await?;
        let parts = find::<DeliveryPart>(&mut *tx, "recipient_id", &a.id.to_string()).await?;
        tx.commit().await?;
        for part in parts.into_iter().filter(|p| {
            matches!(
                p.state,
                domain::PartState::Unknown
                    | domain::PartState::PermanentFailed
                    | domain::PartState::RetryableFailed
            )
        }) {
            let mut tx = self.engine.db.begin().await?;
            let secret: Secret = get(&mut *tx, part.secret_id).await?;
            tx.commit().await?;
            self.text(
                a,
                &format!("{} · {}", part.secret_id, part.index + 1),
                vec![
                    self.b(
                        a,
                        "retry-delivery",
                        Some(part.id),
                        Some(secret.plan_id),
                        false,
                    )
                    .await?,
                ],
            )
            .await?;
            shown = true;
        }
        if !shown {
            self.say(a, "no-items", vec![]).await?;
        }
        self.say(a, "guardians", vec![self.back(a, "home").await?])
            .await?;
        Ok(())
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
                account_id,
                ..
            } => {
                let result = match self.telegram.delete(*chat_id, *message_id).await {
                    DeleteResult::Deleted => SendResult::Sent(0),
                    DeleteResult::RetryAfter(seconds) => SendResult::RetryAfter(seconds),
                    DeleteResult::Permanent => {
                        let mut tx = self.engine.db.begin().await?;
                        let now = tx.now().await?;
                        enqueue(
                            &mut *tx,
                            None,
                            Task::Notice {
                                account_id: *account_id,
                                key: "manual-delete".into(),
                                buttons: vec![],
                            },
                            now,
                            now + DAY,
                            0,
                        )
                        .await?;
                        tx.commit().await?;
                        SendResult::Permanent
                    }
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
                let mut d = self.load_dialog(&a).await?;
                if d.draft_id == Some(*draft_id) {
                    d.step = "builder".into();
                    self.store_dialog(&d).await?;
                    self.say(&a, "block-saved", vec![]).await?;
                    self.render_draft(&a, &d, None).await?;
                }
                self.engine
                    .finish_job(job.id, job.lease_token, SendResult::Sent(0))
                    .await
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
