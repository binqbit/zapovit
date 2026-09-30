//! Draft screens and their callbacks stay in the Telegram adapter. Every button
//! is bound to the draft, visible step, and content/policy revision it represents.
use super::*;

pub(super) fn parse_timing(text: &str) -> Option<Timing> {
    let days: Vec<i64> = text
        .split_whitespace()
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    if days.len() != 3 || days.iter().any(|day| !(1..=365).contains(day)) {
        return None;
    }
    let timing = Timing {
        reminder_seconds: days[0] * DAY,
        inactivity_seconds: days[1] * DAY,
        release_delay_seconds: days[2] * DAY,
    };
    timing.validate().ok()?;
    Some(timing)
}

#[cfg(test)]
pub(super) fn display_time(timestamp: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(timestamp)
        .ok()
        .map(|date| {
            format!(
                "{:02}.{:02}.{} {:02}:{:02} UTC",
                date.day(),
                u8::from(date.month()),
                date.year(),
                date.hour(),
                date.minute()
            )
        })
        .unwrap_or_else(|| "—".into())
}

impl BotUi {
    pub(super) async fn active_draft(&self, a: &Account) -> Result<Option<Dialog>> {
        match self.load_draft_dialog(a).await {
            Ok(d) if d.draft_id.is_some() => Ok(Some(d)),
            Ok(_)
            | Err(Error::NotFound | Error::Rule(RuleError::Expired | RuleError::StaleAction)) => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) async fn draft_button(
        &self,
        a: &Account,
        d: &Dialog,
        verb: &str,
        label: &str,
    ) -> Result<(String, String)> {
        self.draft_button_label(a, d, verb, &tr(&a.locale, label))
            .await
    }

    async fn draft_button_label(
        &self,
        a: &Account,
        d: &Dialog,
        verb: &str,
        label: &str,
    ) -> Result<(String, String)> {
        let mut tx = self.engine.db.begin().await?;
        let draft: Draft = get(&mut *tx, d.draft_id.ok_or(Error::InvalidInput)?).await?;
        tx.commit().await?;
        self.button(
            a,
            &format!("draft:{verb}:{}:{}", d.step, draft.revision),
            d.draft_id,
            d.plan_id,
            true,
            label,
        )
        .await
    }

    pub(super) async fn start_draft(
        &self,
        a: &Account,
        plan: Id,
        message: Option<i64>,
    ) -> Result<()> {
        if self.active_draft(a).await?.is_none() {
            let overview = self.engine.overview(a.id).await?;
            let owned = overview
                .own
                .filter(|p| p.id == plan)
                .ok_or(RuleError::AccessDenied)?;
            if !owned.can_create_draft {
                let (destination, explanation) = if owned.pending_claim.is_some() {
                    ("recovery-options", "recovery-pending-guidance")
                } else if !owned.recovery_saved {
                    ("recovery-options", "prepare-before-content")
                } else if owned.confirmed_people == 0 {
                    ("participants", "prepare-before-content")
                } else if !owned.operational.writes_ready {
                    ("home", "service-delayed")
                } else {
                    ("secrets", "quota-exceeded")
                };
                return self
                    .screen(
                        a,
                        &tr(&a.locale, explanation),
                        vec![
                            self.b(a, destination, None, Some(plan), true).await?,
                            self.nav(a, "home").await?,
                        ],
                        message,
                    )
                    .await;
            }
        }
        let draft_id = self.engine.new_draft(a.id, plan).await?;
        let d = match self.active_draft(a).await? {
            Some(d) if d.draft_id == Some(draft_id) => d,
            _ => {
                let mut d = self
                    .dialog(a, "guardians", Some(plan), Some(draft_id))
                    .await?;
                let mut tx = self.engine.db.begin().await?;
                let draft: Draft = get(&mut *tx, draft_id).await?;
                tx.commit().await?;
                if let Some(policy) = draft.policy {
                    d.guardians = policy.guardians;
                    d.recipients = policy.recipients;
                    d.threshold = policy.threshold;
                    d.step = "ready".into();
                    self.store_dialog(&d).await?;
                }
                d
            }
        };
        self.render_draft(a, &d, message).await
    }

    pub(super) async fn draft_callback(
        &self,
        a: &Account,
        action: &Action,
        message: Option<i64>,
    ) -> Result<()> {
        let mut parts = action.name.split(':');
        let (_, Some(verb), Some(step), Some(revision), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(RuleError::StaleAction.into());
        };
        let mut d = self.load_draft_dialog(a).await?;
        let mut tx = self.engine.db.begin().await?;
        let draft: Draft = get(&mut *tx, d.draft_id.ok_or(RuleError::StaleAction)?).await?;
        if action.target != d.draft_id
            || action.plan_id != d.plan_id
            || step != d.step
            || revision.parse::<i64>().ok() != Some(draft.revision)
        {
            return Err(RuleError::StaleAction.into());
        }
        // Only a valid return to this draft cancels a separate sensitive prompt.
        tx.remove(Kind::Dialog, a.id).await?;
        tx.commit().await?;
        match verb {
            "continue" => {}
            "text" | "copyable" | "spoiler" | "file"
                if matches!(d.step.as_str(), "builder" | "formatting") =>
            {
                d.step = verb.into()
            }
            "builder"
                if matches!(
                    d.step.as_str(),
                    "text"
                        | "copyable"
                        | "spoiler"
                        | "file"
                        | "guardians"
                        | "ready"
                        | "blocks"
                        | "formatting"
                        | "discard"
                ) =>
            {
                d.step = "builder".into()
            }
            "guardians" if matches!(d.step.as_str(), "builder" | "ready" | "recipients") => {
                if d.step == "recipients" {
                    d.recipients = d.selected.clone();
                }
                d.step = "guardians".into();
                d.selected = d.guardians.clone();
            }
            "recipients" if d.step == "threshold" => {
                d.step = "recipients".into();
                d.selected = d.recipients.clone();
            }
            "threshold" if d.step.starts_with("timing") => d.step = "threshold".into(),
            name if (name.starts_with("select.") || name.starts_with("unselect."))
                && matches!(d.step.as_str(), "guardians" | "recipients") =>
            {
                let (verb, person) = name.split_once('.').ok_or(Error::InvalidInput)?;
                let person = Id::parse_str(person).map_err(|_| Error::InvalidInput)?;
                let people = self
                    .engine
                    .contacts(a.id, d.plan_id.ok_or(Error::InvalidInput)?)
                    .await?;
                if !people
                    .iter()
                    .any(|p| p.account_id == person && p.confirmed && !p.archived)
                {
                    return Err(RuleError::StaleAction.into());
                }
                if verb == "unselect" {
                    d.selected.remove(&person);
                } else if !d.selected.contains(&person) {
                    if d.selected.len() == 10 {
                        self.say(a, "selection-limit", vec![]).await?;
                        return Ok(());
                    }
                    d.selected.insert(person);
                }
                if d.step == "guardians" {
                    d.guardians = d.selected.clone();
                } else {
                    d.recipients = d.selected.clone();
                }
            }
            "done" if matches!(d.step.as_str(), "guardians" | "recipients") => {
                if d.selected.is_empty() {
                    self.say(a, "select-person-first", vec![]).await?;
                    return self.render_draft(a, &d, message).await;
                }
                if d.step == "guardians" {
                    d.guardians = d.selected.clone();
                    d.selected = d.recipients.clone();
                    d.step = "recipients".into();
                } else {
                    d.recipients = d.selected.clone();
                    d.step = "threshold".into();
                }
            }
            name if name.starts_with("threshold.") && d.step == "threshold" => {
                let threshold: u8 = name[10..].parse().map_err(|_| Error::InvalidInput)?;
                if threshold == 0 || usize::from(threshold) > d.guardians.len() {
                    return Err(Error::InvalidInput);
                }
                d.threshold = threshold;
                d.step = "timing-choice".into();
            }
            "timing-default" if d.step.starts_with("timing") => {
                self.set_timing(a, &mut d, Timing::default()).await?;
            }
            "timing-custom" if d.step == "timing-choice" => {
                d.timing = Some(Timing::default());
                d.step = "timing-reminder".into();
            }
            "formatting" if d.step == "builder" => d.step = "formatting".into(),
            "blocks" if d.step == "builder" || d.step.starts_with("block.") => {
                d.step = "blocks".into()
            }
            name if name.starts_with("block.") && d.step == "blocks" => {
                let index = name[6..]
                    .parse::<usize>()
                    .map_err(|_| Error::InvalidInput)?;
                if index >= self.engine.draft_blocks(a.id, draft.id).await?.len() {
                    return Err(RuleError::StaleAction.into());
                }
                d.step = name.into();
            }
            name if name.starts_with("remove.") && d.step.starts_with("block.") => {
                let index = name[7..]
                    .parse::<usize>()
                    .map_err(|_| Error::InvalidInput)?;
                if d.step != format!("block.{index}") {
                    return Err(RuleError::StaleAction.into());
                }
                self.engine
                    .remove_draft_block(a.id, draft.id, draft.revision, index)
                    .await?;
                d.step = "blocks".into();
            }
            name if name.starts_with("move.") && d.step.starts_with("block.") => {
                let (from, to) = name[5..].split_once('.').ok_or(Error::InvalidInput)?;
                let from = from.parse::<usize>().map_err(|_| Error::InvalidInput)?;
                let to = to.parse::<usize>().map_err(|_| Error::InvalidInput)?;
                if d.step != format!("block.{from}") {
                    return Err(RuleError::StaleAction.into());
                }
                self.engine
                    .move_draft_block(a.id, draft.id, draft.revision, from, to)
                    .await?;
                d.step = "blocks".into();
            }
            "ready" if matches!(d.step.as_str(), "builder" | "seal") => {
                if draft.policy.is_none()
                    || self.engine.draft_blocks(a.id, draft.id).await?.is_empty()
                {
                    return Err(RuleError::NotReady.into());
                }
                d.step = "ready".into();
            }
            "discard" => d.step = "discard".into(),
            "discard-confirmed" if d.step == "discard" => {
                self.engine
                    .cancel_draft(a.id, draft.id, draft.revision)
                    .await?;
                return self.menu(a, Menu::Home, message).await;
            }
            "name" if d.step == "builder" || d.step == "ready" => {
                let mut prompt = self.dialog(a, "label", d.plan_id, None).await?;
                prompt.case_id = Some(draft.id);
                self.store_dialog(&prompt).await?;
                return self
                    .screen(
                        a,
                        &tr(&a.locale, "label-prompt"),
                        vec![self.nav(a, "home").await?],
                        message,
                    )
                    .await;
            }
            "preview" if d.step == "ready" => {
                let blocks = self.engine.draft_blocks(a.id, draft.id).await?;
                let preview = blocks
                    .into_iter()
                    .map(|block| match &block {
                        Block::File { name, caption, .. } => Block::Text {
                            text: format!("{name}\n{caption}"),
                        },
                        _ => block,
                    })
                    .collect();
                for block in delivery_blocks(preview) {
                    match self.telegram.send_block(a.chat_id, &block, None).await {
                        SendResult::Sent(id) => self.track_preview(a, &d, id).await?,
                        _ => return Err(Error::MessageUnavailable),
                    }
                }
                return self.render_draft(a, &d, None).await;
            }
            "save" if d.step == "ready" => d.step = "seal".into(),
            "seal" if d.step == "seal" => {
                self.engine.save(a.id, draft.id).await?;
                let mut tx = self.engine.db.begin().await?;
                tx.remove(Kind::DraftSession, a.id).await?;
                tx.commit().await?;
                self.say(a, "saved", vec![self.back(a, "plan-menu").await?])
                    .await?;
                return Ok(());
            }
            _ => return Err(RuleError::StaleAction.into()),
        }
        self.store_dialog(&d).await?;
        self.render_draft(a, &d, message).await
    }

    pub(super) async fn set_timing(
        &self,
        a: &Account,
        d: &mut Dialog,
        timing: Timing,
    ) -> Result<()> {
        self.engine
            .draft_policy(
                a.id,
                d.draft_id.ok_or(Error::InvalidInput)?,
                Policy {
                    guardians: d.guardians.clone(),
                    recipients: d.recipients.clone(),
                    threshold: d.threshold,
                    timing: timing.clone(),
                },
            )
            .await?;
        d.timing = Some(timing);
        d.step = if self
            .engine
            .draft_blocks(a.id, d.draft_id.ok_or(Error::InvalidInput)?)
            .await?
            .is_empty()
        {
            "builder"
        } else {
            "ready"
        }
        .into();
        self.store_dialog(d).await
    }

    pub(super) async fn render_draft(
        &self,
        a: &Account,
        d: &Dialog,
        message: Option<i64>,
    ) -> Result<()> {
        let mut buttons = Vec::new();
        let mut text = match d.step.as_str() {
            "builder" => {
                let count = self
                    .engine
                    .draft_blocks(a.id, d.draft_id.ok_or(Error::InvalidInput)?)
                    .await?
                    .len();
                for name in ["text", "file"] {
                    buttons.push(self.draft_button(a, d, name, name).await?);
                }
                if count > 0 {
                    buttons.push(self.draft_button(a, d, "ready", "review").await?);
                    buttons.push(self.draft_button(a, d, "blocks", "manage-blocks").await?);
                }
                buttons.push(self.draft_button(a, d, "formatting", "formatting").await?);
                buttons.push(self.draft_button(a, d, "name", "name-secret").await?);
                buttons.push(self.draft_button(a, d, "guardians", "edit-people").await?);
                format!(
                    "{}\n\n{}: {count}/20",
                    tr(&a.locale, "builder"),
                    tr(&a.locale, "block-count")
                )
            }
            "formatting" => {
                for name in ["copyable", "spoiler"] {
                    buttons.push(self.draft_button(a, d, name, name).await?);
                }
                buttons.push(self.draft_button(a, d, "builder", "back").await?);
                tr(&a.locale, "formatting-explained")
            }
            "blocks" => {
                let blocks = self
                    .engine
                    .draft_blocks(a.id, d.draft_id.ok_or(Error::InvalidInput)?)
                    .await?;
                for (i, block) in blocks.iter().enumerate() {
                    let key = match block {
                        Block::Text { .. } => "text",
                        Block::Copyable { .. } => "copyable",
                        Block::Spoiler { .. } => "spoiler",
                        Block::File { .. } => "file",
                    };
                    buttons.push(
                        self.draft_button_label(
                            a,
                            d,
                            &format!("block.{i}"),
                            &format!("{} · {}", i + 1, tr(&a.locale, key)),
                        )
                        .await?,
                    );
                }
                buttons.push(self.draft_button(a, d, "builder", "back").await?);
                tr(&a.locale, "manage-blocks")
            }
            step if step.starts_with("block.") => {
                let index = step[6..]
                    .parse::<usize>()
                    .map_err(|_| Error::InvalidInput)?;
                let count = self
                    .engine
                    .draft_blocks(a.id, d.draft_id.ok_or(Error::InvalidInput)?)
                    .await?
                    .len();
                if index >= count {
                    return Err(RuleError::StaleAction.into());
                }
                if index > 0 {
                    buttons.push(
                        self.draft_button(a, d, &format!("move.{index}.{}", index - 1), "move-up")
                            .await?,
                    );
                }
                if index + 1 < count {
                    buttons.push(
                        self.draft_button(
                            a,
                            d,
                            &format!("move.{index}.{}", index + 1),
                            "move-down",
                        )
                        .await?,
                    );
                }
                buttons.push(
                    self.draft_button(a, d, &format!("remove.{index}"), "remove-block")
                        .await?,
                );
                buttons.push(self.draft_button(a, d, "blocks", "back").await?);
                format!("{} {} / {count}", tr(&a.locale, "block-label"), index + 1)
            }
            "discard" => {
                buttons.push(
                    self.draft_button(a, d, "discard-confirmed", "discard-confirmed")
                        .await?,
                );
                buttons.push(self.draft_button(a, d, "builder", "back").await?);
                tr(&a.locale, "discard-explained")
            }
            "seal" => {
                buttons.push(self.draft_button(a, d, "seal", "seal-confirmed").await?);
                buttons.push(self.draft_button(a, d, "ready", "back").await?);
                tr(&a.locale, "seal-explained")
            }
            "text" | "copyable" | "spoiler" | "file" => {
                buttons.push(self.draft_button(a, d, "builder", "back").await?);
                tr(
                    &a.locale,
                    if d.step == "file" {
                        "file-prompt"
                    } else {
                        "block-prompt"
                    },
                )
            }
            "file-pending" => tr(&a.locale, "awaiting-file"),
            "guardians" | "recipients" => {
                let people = self
                    .engine
                    .contacts(a.id, d.plan_id.ok_or(Error::InvalidInput)?)
                    .await?;
                let labels: Vec<_> = people
                    .into_iter()
                    .filter(|p| p.confirmed && !p.archived)
                    .map(|p| {
                        let name = p.label.unwrap_or_else(|| p.telegram_id.to_string());
                        (
                            p.account_id,
                            format!(
                                "{} {}",
                                if d.selected.contains(&p.account_id) {
                                    "✓"
                                } else {
                                    "○"
                                },
                                name
                            ),
                        )
                    })
                    .collect();
                let empty = labels.is_empty();
                for (person, label) in labels {
                    buttons.push(
                        self.draft_button_label(
                            a,
                            d,
                            &format!(
                                "{}.{}",
                                if d.selected.contains(&person) {
                                    "unselect"
                                } else {
                                    "select"
                                },
                                person.simple()
                            ),
                            &label,
                        )
                        .await?,
                    );
                }
                if !d.selected.is_empty() {
                    buttons.push(self.draft_button(a, d, "done", "done").await?);
                }
                if empty {
                    buttons.push(self.b(a, "participants", None, d.plan_id, true).await?);
                }
                buttons.push(
                    self.draft_button(
                        a,
                        d,
                        if d.step == "guardians" {
                            "builder"
                        } else {
                            "guardians"
                        },
                        "back",
                    )
                    .await?,
                );
                if empty {
                    tr(&a.locale, "no-confirmed-people")
                } else {
                    format!(
                        "{}\n\n{}: {}/10",
                        tr(
                            &a.locale,
                            if d.step == "guardians" {
                                "choose-guardians"
                            } else {
                                "choose-recipients"
                            }
                        ),
                        tr(&a.locale, "selected-count"),
                        d.selected.len()
                    )
                }
            }
            "threshold" => {
                for n in 1..=d.guardians.len() {
                    buttons.push(
                        self.draft_button_label(
                            a,
                            d,
                            &format!("threshold.{n}"),
                            &format!("{n} / {}", d.guardians.len()),
                        )
                        .await?,
                    );
                }
                buttons.push(self.draft_button(a, d, "recipients", "back").await?);
                tr(&a.locale, "choose-threshold")
            }
            "timing-choice" | "timing" | "timing-reminder" | "timing-inactivity"
            | "timing-wait" => {
                buttons.push(
                    self.draft_button(a, d, "timing-default", "timing-default")
                        .await?,
                );
                if d.step == "timing-choice" {
                    buttons.push(
                        self.draft_button(a, d, "timing-custom", "timing-custom")
                            .await?,
                    );
                }
                buttons.push(self.draft_button(a, d, "threshold", "back").await?);
                tr(
                    &a.locale,
                    match d.step.as_str() {
                        "timing" => "choose-timing",
                        "timing-reminder" => "timing-reminder-prompt",
                        "timing-inactivity" => "timing-inactivity-prompt",
                        "timing-wait" => "timing-wait-prompt",
                        _ => "timing-explained",
                    },
                )
            }
            "ready" => {
                let mut tx = self.engine.db.begin().await?;
                let draft: Draft = get(&mut *tx, d.draft_id.ok_or(Error::InvalidInput)?).await?;
                let policy = draft.policy.ok_or(Error::InvalidInput)?;
                let mut guardians = Vec::new();
                let mut recipients = Vec::new();
                for id in &policy.guardians {
                    guardians.push(get::<Account>(&mut *tx, *id).await?.telegram_id.to_string());
                }
                for id in &policy.recipients {
                    recipients.push(get::<Account>(&mut *tx, *id).await?.telegram_id.to_string());
                }
                tx.commit().await?;
                for (verb, label) in [
                    ("preview", "preview"),
                    ("save", "save"),
                    ("builder", "edit-blocks"),
                    ("guardians", "edit-people"),
                ] {
                    buttons.push(self.draft_button(a, d, verb, label).await?);
                }
                format!(
                    "{}\n\n{}: {}\n{}: {}\n{}: {} / {}\n{}: {} / {} / {}",
                    tr(&a.locale, "draft-ready"),
                    tr(&a.locale, "guardians-label"),
                    guardians.join(", "),
                    tr(&a.locale, "recipients-label"),
                    recipients.join(", "),
                    tr(&a.locale, "threshold-label"),
                    policy.threshold,
                    policy.guardians.len(),
                    tr(&a.locale, "timing-label"),
                    policy.timing.reminder_seconds / DAY,
                    policy.timing.inactivity_seconds / DAY,
                    policy.timing.release_delay_seconds / DAY
                )
            }
            _ => return Err(RuleError::StaleAction.into()),
        };
        if d.threshold == 1 && matches!(d.step.as_str(), "timing-choice" | "ready" | "seal") {
            text.push_str(&format!("\n\n{}", tr(&a.locale, "single-approval-warning")));
        }
        let mut tx = self.engine.db.begin().await?;
        let draft: Draft = get(&mut *tx, d.draft_id.ok_or(Error::InvalidInput)?).await?;
        tx.commit().await?;
        text.push_str(&format!(
            "\n\n{}: {}",
            tr(&a.locale, "draft-until"),
            self.date(a, draft.expires_at.min(draft.created_at + 3600))
                .await?
        ));
        if !matches!(d.step.as_str(), "discard" | "seal") {
            buttons.push(self.draft_button(a, d, "discard", "discard").await?);
        }
        buttons.push(self.nav(a, "home").await?);
        self.screen(a, &text, buttons, message).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timing_input_uses_domain_limits_without_overflow() {
        assert_eq!(parse_timing("7 28 7"), Some(Timing::default()));
        for input in [
            "7 10 7",
            "31 62 7",
            "7 28 31",
            "7 28",
            "7 x 7",
            "9223372036854775807 28 7",
            "0 28 7",
        ] {
            assert!(parse_timing(input).is_none(), "{input}");
        }
    }
    #[test]
    fn status_time_is_a_readable_utc_date() {
        assert_eq!(display_time(0), "01.01.1970 00:00 UTC");
        assert_eq!(display_time(i64::MAX), "—");
    }
}
