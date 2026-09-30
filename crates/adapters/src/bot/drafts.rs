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
        match self.load_dialog(a).await {
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
        let draft_id = self.engine.new_draft(a.id, plan).await?;
        let d = match self.active_draft(a).await? {
            Some(d) if d.draft_id == Some(draft_id) => d,
            _ => {
                let mut d = self
                    .dialog(a, "builder", Some(plan), Some(draft_id))
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
        let mut d = self.load_dialog(a).await?;
        let mut tx = self.engine.db.begin().await?;
        let draft: Draft = get(&mut *tx, d.draft_id.ok_or(RuleError::StaleAction)?).await?;
        tx.commit().await?;
        if action.target != d.draft_id
            || action.plan_id != d.plan_id
            || step != d.step
            || revision.parse::<i64>().ok() != Some(draft.revision)
        {
            return Err(RuleError::StaleAction.into());
        }
        match verb {
            "continue" => {}
            "text" | "copyable" | "spoiler" | "file" if d.step == "builder" => d.step = verb.into(),
            "builder"
                if matches!(
                    d.step.as_str(),
                    "text" | "copyable" | "spoiler" | "file" | "guardians" | "ready"
                ) =>
            {
                d.step = "builder".into()
            }
            "guardians" if matches!(d.step.as_str(), "builder" | "ready" | "recipients") => {
                if self.engine.draft_blocks(a.id, draft.id).await?.is_empty() {
                    self.say(a, "add-block-first", vec![]).await?;
                    return self.render_draft(a, &d, message).await;
                }
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
            "threshold" if matches!(d.step.as_str(), "timing" | "timing-choice") => {
                d.step = "threshold".into()
            }
            name if name.starts_with("toggle.")
                && matches!(d.step.as_str(), "guardians" | "recipients") =>
            {
                let person = Id::parse_str(&name[7..]).map_err(|_| Error::InvalidInput)?;
                let mut tx = self.engine.db.begin().await?;
                let people = list::<Participant>(&mut *tx, d.plan_id).await?;
                tx.commit().await?;
                if !people.iter().any(|p| p.account_id == person && p.confirmed) {
                    return Err(RuleError::StaleAction.into());
                }
                if !d.selected.remove(&person) {
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
            "timing-default" if matches!(d.step.as_str(), "timing-choice" | "timing") => {
                self.set_timing(a, &mut d, Timing::default()).await?;
            }
            "timing-custom" if d.step == "timing-choice" => d.step = "timing".into(),
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
            "save" if d.step == "ready" => {
                self.engine.save(a.id, draft.id).await?;
                let mut tx = self.engine.db.begin().await?;
                tx.remove(Kind::Dialog, a.id).await?;
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
                    timing,
                },
            )
            .await?;
        d.step = "ready".into();
        self.store_dialog(d).await
    }

    pub(super) async fn render_draft(
        &self,
        a: &Account,
        d: &Dialog,
        message: Option<i64>,
    ) -> Result<()> {
        let mut buttons = Vec::new();
        let text = match d.step.as_str() {
            "builder" => {
                let count = self
                    .engine
                    .draft_blocks(a.id, d.draft_id.ok_or(Error::InvalidInput)?)
                    .await?
                    .len();
                for name in ["text", "copyable", "spoiler", "file"] {
                    buttons.push(self.draft_button(a, d, name, name).await?);
                }
                if count > 0 {
                    buttons.push(
                        self.draft_button(a, d, "guardians", "choose-people")
                            .await?,
                    );
                }
                format!(
                    "{}\n\n{}: {count}/20",
                    tr(&a.locale, "builder"),
                    tr(&a.locale, "block-count")
                )
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
                let mut tx = self.engine.db.begin().await?;
                let people = list::<Participant>(&mut *tx, d.plan_id).await?;
                let mut labels = Vec::new();
                for p in people.into_iter().filter(|p| p.confirmed) {
                    let person: Account = get(&mut *tx, p.account_id).await?;
                    labels.push((
                        person.id,
                        format!(
                            "{} {}",
                            if d.selected.contains(&person.id) {
                                "✓"
                            } else {
                                "○"
                            },
                            person.telegram_id
                        ),
                    ));
                }
                tx.commit().await?;
                let empty = labels.is_empty();
                for (person, label) in labels {
                    buttons.push(
                        self.draft_button_label(
                            a,
                            d,
                            &format!("toggle.{}", person.simple()),
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
            "timing-choice" | "timing" => {
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
                    if d.step == "timing" {
                        "choose-timing"
                    } else {
                        "timing-explained"
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
        buttons.push(self.nav(a, "plan-menu").await?);
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
