//! Draft screens and their callbacks stay in the Telegram adapter. Every button
//! is bound to the draft, visible step, and content/policy revision it represents.
use super::*;

const PEOPLE_PER_PAGE: usize = 6;

fn selection_step(step: &str) -> Option<(&str, usize)> {
    let (role, page) = step
        .split_once(".page.")
        .map_or(Some((step, 0)), |(role, page)| {
            page.parse::<usize>().ok().map(|page| (role, page))
        })?;
    matches!(role, "guardians" | "recipients").then_some((role, page))
}

fn person_name(person: &ContactView) -> String {
    workspace::short_label(
        &person
            .label
            .clone()
            .or_else(|| person.display_name.clone())
            .unwrap_or_else(|| person.telegram_id.to_string()),
    )
}

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
                if owned.pending_claim.is_none() {
                    if !owned.recovery_saved {
                        return self.setup_recovery(a, plan).await;
                    }
                    if owned.confirmed_people == 0 {
                        return self.setup_people(a, plan, None).await;
                    }
                }
                let (destination, explanation) = if owned.pending_claim.is_some() {
                    ("recovery-options", "recovery-pending-guidance")
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
                    d.timing = Some(policy.timing);
                    d.step = if self.engine.draft_blocks(a.id, draft_id).await?.is_empty() {
                        "builder"
                    } else {
                        "ready"
                    }
                    .into();
                    self.store_dialog(&d).await?;
                }
                d
            }
        };
        self.render_draft(a, &d, None).await
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
            || (verb != "continue" && (message.is_none() || message != d.reply_to))
        {
            return Err(RuleError::StaleAction.into());
        }
        // Only a valid return to this draft cancels a separate sensitive prompt.
        tx.remove(Kind::Dialog, a.id).await?;
        tx.commit().await?;
        let previous_step = d.step.clone();
        match verb {
            "continue" => {}
            "invite" if selection_step(&d.step).is_some() => {
                self.setup_people(a, d.plan_id.ok_or(Error::InvalidInput)?, None)
                    .await?;
                let mut tx = self.engine.db.begin().await?;
                tx.lock(Kind::DraftSession, a.id).await?;
                let mut current: DraftSession = get(&mut *tx, a.id).await?;
                if current.dialog.draft_id == d.draft_id
                    && current.dialog.step == d.step
                    && current.dialog.reply_to == d.reply_to
                {
                    // No draft question is active while adding a person. Only
                    // an explicit continuation may issue a new bound question.
                    current.dialog.reply_to = None;
                    put(&mut *tx, current.dialog.plan_id, &current).await?;
                }
                tx.commit().await?;
                if let Some(previous) = d.reply_to {
                    let _ = self.telegram.clear_menu(a.chat_id, previous).await;
                }
                return Ok(());
            }
            "text" | "copyable" | "spoiler" | "file"
                if matches!(d.step.as_str(), "builder" | "formatting" | "options") =>
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
                        | "ready"
                        | "blocks"
                        | "formatting"
                        | "options"
                        | "threshold"
                ) =>
            {
                d.step = "builder".into()
            }
            "guardians"
                if matches!(d.step.as_str(), "options" | "ready")
                    || selection_step(&d.step).is_some_and(|(role, _)| role == "recipients") =>
            {
                if selection_step(&d.step).is_some_and(|(role, _)| role == "recipients") {
                    d.recipients = d.selected.clone();
                }
                d.step = "guardians".into();
                d.selected = d.guardians.clone();
            }
            "recipients" if matches!(d.step.as_str(), "builder" | "threshold") => {
                d.step = "recipients".into();
                d.selected = d.recipients.clone();
            }
            "threshold" if matches!(d.step.as_str(), "builder" | "timing-choice" | "timing") => {
                if self.engine.draft_blocks(a.id, draft.id).await?.is_empty() {
                    return Err(RuleError::NotReady.into());
                }
                d.step = "threshold".into()
            }
            "timing-choice" if d.step == "timing-reminder" => d.step = "timing-choice".into(),
            "timing-reminder" if d.step == "timing-inactivity" => d.step = "timing-reminder".into(),
            "timing-inactivity" if d.step == "timing-wait" => d.step = "timing-inactivity".into(),
            name if name.starts_with("page.") && selection_step(&d.step).is_some() => {
                let (role, _) = selection_step(&d.step).ok_or(RuleError::StaleAction)?;
                let page = name[5..]
                    .parse::<usize>()
                    .map_err(|_| Error::InvalidInput)?;
                let count = self
                    .engine
                    .contacts(a.id, d.plan_id.ok_or(Error::InvalidInput)?)
                    .await?
                    .into_iter()
                    .filter(|p| p.confirmed && !p.archived)
                    .count();
                if page > count.saturating_sub(1) / PEOPLE_PER_PAGE {
                    return Err(RuleError::StaleAction.into());
                }
                d.step = if page == 0 {
                    role.to_owned()
                } else {
                    format!("{role}.page.{page}")
                };
            }
            name if (name.starts_with("select.") || name.starts_with("unselect."))
                && selection_step(&d.step).is_some() =>
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
                if selection_step(&d.step).is_some_and(|(role, _)| role == "guardians") {
                    d.guardians = d.selected.clone();
                } else {
                    d.recipients = d.selected.clone();
                }
            }
            "done" if selection_step(&d.step).is_some() => {
                if d.selected.is_empty() {
                    self.say(a, "select-person-first", vec![]).await?;
                    return self.render_draft(a, &d, message).await;
                }
                if selection_step(&d.step).is_some_and(|(role, _)| role == "guardians") {
                    d.guardians = d.selected.clone();
                    d.selected = d.recipients.clone();
                    d.step = "recipients".into();
                } else {
                    d.recipients = d.selected.clone();
                    d.step = "builder".into();
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
            "timing-custom" if matches!(d.step.as_str(), "timing-choice" | "timing") => {
                d.timing.get_or_insert_with(Timing::default);
                d.step = "timing-reminder".into();
            }
            "options"
                if matches!(
                    d.step.as_str(),
                    "builder" | "formatting" | "blocks" | "discard" | "text" | "file"
                ) || (selection_step(&d.step).is_some_and(|(role, _)| role == "guardians")
                    && draft.policy.is_some()) =>
            {
                d.step = "options".into()
            }
            "formatting" if matches!(d.step.as_str(), "options" | "copyable" | "spoiler") => {
                d.step = "formatting".into()
            }
            "blocks" if d.step == "options" || d.step.starts_with("block.") => {
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
            "discard" if d.step == "options" => d.step = "discard".into(),
            "discard-confirmed" if d.step == "discard" => {
                self.engine
                    .cancel_draft(a.id, draft.id, draft.revision)
                    .await?;
                self.menu(a, Menu::Home, None).await?;
                if let Some(previous) = d.reply_to {
                    let _ = self.telegram.clear_menu(a.chat_id, previous).await;
                }
                return Ok(());
            }
            "name" if d.step == "options" => {
                let mut prompt = self.dialog(a, "label", d.plan_id, None).await?;
                prompt.case_id = Some(draft.id);
                self.store_dialog(&prompt).await?;
                self.screen(
                    a,
                    &tr(&a.locale, "label-prompt"),
                    vec![self.draft_button(a, &d, "continue", "back").await?],
                    None,
                )
                .await?;
                if let Some(previous) = d.reply_to {
                    let _ = self.telegram.clear_menu(a.chat_id, previous).await;
                }
                return Ok(());
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
                self.say(a, "saved", vec![]).await?;
                self.setup_status(a, draft.plan_id, None).await?;
                if let Some(previous) = d.reply_to {
                    let _ = self.telegram.clear_menu(a.chat_id, previous).await;
                }
                return Ok(());
            }
            _ => return Err(RuleError::StaleAction.into()),
        }
        self.store_dialog(&d).await?;
        let same_question = previous_step == d.step
            || selection_step(&previous_step)
                .zip(selection_step(&d.step))
                .is_some_and(|((previous, _), (current, _))| previous == current);
        self.render_draft(
            a,
            &d,
            if same_question && verb != "continue" {
                message
            } else {
                None
            },
        )
        .await
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
                if count > 0 {
                    buttons.push(self.draft_button(a, d, "threshold", "draft-next").await?);
                }
                buttons.push(self.draft_button(a, d, "options", "draft-options").await?);
                buttons.push(self.draft_button(a, d, "recipients", "back").await?);
                format!(
                    "{}\n\n{}\n\n{}: {count}/20",
                    tr(&a.locale, "draft-stage-content"),
                    tr(&a.locale, "builder"),
                    tr(&a.locale, "block-count")
                )
            }
            "options" => {
                for name in ["text", "file"] {
                    buttons.push(self.draft_button(a, d, name, name).await?);
                }
                buttons.push(self.draft_button(a, d, "formatting", "formatting").await?);
                if !self
                    .engine
                    .draft_blocks(a.id, d.draft_id.ok_or(Error::InvalidInput)?)
                    .await?
                    .is_empty()
                {
                    buttons.push(self.draft_button(a, d, "blocks", "manage-blocks").await?);
                }
                buttons.push(self.draft_button(a, d, "name", "name-secret").await?);
                buttons.push(self.draft_button(a, d, "guardians", "edit-people").await?);
                buttons.push(self.draft_button(a, d, "discard", "discard").await?);
                buttons.push(self.draft_button(a, d, "builder", "back").await?);
                tr(&a.locale, "draft-options-explained")
            }
            "formatting" => {
                for name in ["copyable", "spoiler"] {
                    buttons.push(self.draft_button(a, d, name, name).await?);
                }
                buttons.push(self.draft_button(a, d, "options", "back").await?);
                format!(
                    "{}\n\n{}",
                    tr(&a.locale, "formatting"),
                    tr(&a.locale, "formatting-explained")
                )
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
                buttons.push(self.draft_button(a, d, "options", "back").await?);
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
                buttons.push(self.draft_button(a, d, "options", "back").await?);
                format!(
                    "{}\n\n{}",
                    tr(&a.locale, "draft-stage-discard"),
                    tr(&a.locale, "discard-explained")
                )
            }
            "seal" => {
                buttons.push(self.draft_button(a, d, "seal", "seal-confirmed").await?);
                buttons.push(self.draft_button(a, d, "ready", "back").await?);
                format!(
                    "{}\n\n{}",
                    tr(&a.locale, "draft-stage-seal"),
                    tr(&a.locale, "seal-explained")
                )
            }
            "text" | "copyable" | "spoiler" | "file" => {
                let previous = if matches!(d.step.as_str(), "copyable" | "spoiler") {
                    "formatting"
                } else {
                    "options"
                };
                buttons.push(self.draft_button(a, d, previous, "back").await?);
                format!(
                    "{}\n\n{}",
                    tr(&a.locale, &d.step),
                    tr(
                        &a.locale,
                        if d.step == "file" {
                            "file-prompt"
                        } else {
                            "block-prompt"
                        },
                    )
                )
            }
            "file-pending" => tr(&a.locale, "awaiting-file"),
            step if selection_step(step).is_some() => {
                let (role, page) = selection_step(step).ok_or(RuleError::StaleAction)?;
                let people: Vec<_> = self
                    .engine
                    .contacts(a.id, d.plan_id.ok_or(Error::InvalidInput)?)
                    .await?
                    .into_iter()
                    .filter(|p| p.confirmed && !p.archived)
                    .collect();
                let start =
                    page.min(people.len().saturating_sub(1) / PEOPLE_PER_PAGE) * PEOPLE_PER_PAGE;
                let selected: Vec<_> = people
                    .iter()
                    .filter(|person| d.selected.contains(&person.account_id))
                    .map(person_name)
                    .collect();
                if !selected.is_empty() {
                    buttons.push(self.draft_button(a, d, "done", "draft-next").await?);
                }
                for person in people.iter().skip(start).take(PEOPLE_PER_PAGE) {
                    let chosen = d.selected.contains(&person.account_id);
                    buttons.push(
                        self.draft_button_label(
                            a,
                            d,
                            &format!(
                                "{}.{}",
                                if chosen { "unselect" } else { "select" },
                                person.account_id.simple()
                            ),
                            &format!("{} {}", if chosen { "✓" } else { "○" }, person_name(person)),
                        )
                        .await?,
                    );
                }
                if start > 0 {
                    buttons.push(
                        self.draft_button(
                            a,
                            d,
                            &format!("page.{}", start / PEOPLE_PER_PAGE - 1),
                            "page-previous",
                        )
                        .await?,
                    );
                }
                if start + PEOPLE_PER_PAGE < people.len() {
                    buttons.push(
                        self.draft_button(
                            a,
                            d,
                            &format!("page.{}", start / PEOPLE_PER_PAGE + 1),
                            "page-next",
                        )
                        .await?,
                    );
                }
                buttons.push(
                    self.draft_button(a, d, "invite", "setup-add-person")
                        .await?,
                );
                if role == "recipients" {
                    buttons.push(self.draft_button(a, d, "guardians", "back").await?);
                } else {
                    let mut tx = self.engine.db.begin().await?;
                    let draft: Draft =
                        get(&mut *tx, d.draft_id.ok_or(Error::InvalidInput)?).await?;
                    tx.commit().await?;
                    if draft.policy.is_some() {
                        buttons.push(self.draft_button(a, d, "options", "back").await?);
                    } else {
                        buttons.push(self.back(a, "home").await?);
                    }
                }
                if people.is_empty() {
                    tr(&a.locale, "no-confirmed-people")
                } else {
                    format!(
                        "{}\n\n{}\n\n{}: {}/10\n{}\n\n{}",
                        tr(
                            &a.locale,
                            if role == "guardians" {
                                "draft-stage-guardians"
                            } else {
                                "draft-stage-recipients"
                            }
                        ),
                        tr(
                            &a.locale,
                            if role == "guardians" {
                                "choose-guardians"
                            } else {
                                "choose-recipients"
                            }
                        ),
                        tr(&a.locale, "selected-count"),
                        selected.len(),
                        if selected.is_empty() {
                            tr(&a.locale, "draft-none-selected")
                        } else {
                            selected.join(", ")
                        },
                        tr(&a.locale, "draft-selection-help")
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
                buttons.push(self.draft_button(a, d, "builder", "back").await?);
                format!(
                    "{}\n\n{}",
                    tr(&a.locale, "draft-stage-threshold"),
                    tr(&a.locale, "choose-threshold")
                )
            }
            "timing-choice" | "timing" | "timing-reminder" | "timing-inactivity"
            | "timing-wait" => {
                if matches!(d.step.as_str(), "timing-choice" | "timing") {
                    buttons.push(
                        self.draft_button(a, d, "timing-default", "timing-default")
                            .await?,
                    );
                    buttons.push(
                        self.draft_button(a, d, "timing-custom", "timing-custom")
                            .await?,
                    );
                }
                let previous = match d.step.as_str() {
                    "timing-reminder" => "timing-choice",
                    "timing-inactivity" => "timing-reminder",
                    "timing-wait" => "timing-inactivity",
                    _ => "threshold",
                };
                buttons.push(self.draft_button(a, d, previous, "back").await?);
                format!(
                    "{}\n\n{}",
                    tr(&a.locale, "draft-stage-timing"),
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
                )
            }
            "ready" => {
                let mut tx = self.engine.db.begin().await?;
                let draft: Draft = get(&mut *tx, d.draft_id.ok_or(Error::InvalidInput)?).await?;
                let policy = draft.policy.ok_or(Error::InvalidInput)?;
                tx.commit().await?;
                let contacts = self
                    .engine
                    .contacts(a.id, d.plan_id.ok_or(Error::InvalidInput)?)
                    .await?;
                let names = |ids: &std::collections::BTreeSet<Id>| {
                    ids.iter()
                        .map(|id| {
                            contacts
                                .iter()
                                .find(|person| person.account_id == *id)
                                .map(person_name)
                                .unwrap_or_else(|| tr(&a.locale, "person-unavailable"))
                        })
                        .collect::<Vec<_>>()
                };
                let guardians = names(&policy.guardians);
                let recipients = names(&policy.recipients);
                for (verb, label) in [
                    ("preview", "preview"),
                    ("save", "save"),
                    ("builder", "edit-blocks"),
                ] {
                    buttons.push(self.draft_button(a, d, verb, label).await?);
                }
                let mut summary = format!(
                    "{}\n\n{}\n\n{}: {}\n{}: {}\n{}: {} / {}\n{}: {} / {} / {}",
                    tr(&a.locale, "draft-stage-review"),
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
                );
                if policy.threshold == 1 {
                    summary.push_str(&format!("\n\n{}", tr(&a.locale, "single-approval-warning")));
                }
                summary
            }
            _ => return Err(RuleError::StaleAction.into()),
        };
        if matches!(d.step.as_str(), "guardians" | "builder" | "ready") {
            let mut tx = self.engine.db.begin().await?;
            let draft: Draft = get(&mut *tx, d.draft_id.ok_or(Error::InvalidInput)?).await?;
            tx.commit().await?;
            text.push_str(&format!(
                "\n\n{}: {}",
                tr(&a.locale, "draft-until"),
                self.date(a, draft.expires_at.min(draft.created_at + 3600))
                    .await?
            ));
        }
        let message_id = self
            .screen_grid_id(
                a,
                &text,
                buttons.into_iter().map(|button| vec![button]).collect(),
                message,
            )
            .await?;
        let mut tx = self.engine.db.begin().await?;
        tx.lock(Kind::DraftSession, a.id).await?;
        let mut current = tx
            .get(Kind::DraftSession, a.id)
            .await?
            .map(serde_json::from_value::<DraftSession>)
            .transpose()
            .map_err(|_| Error::Internal)?;
        let updated = if let Some(current) = current.as_mut().filter(|current| {
            current.dialog.draft_id == d.draft_id
                && current.dialog.step == d.step
                && current.dialog.reply_to == d.reply_to
        }) {
            current.dialog.reply_to = Some(message_id);
            put(&mut *tx, current.dialog.plan_id, current).await?;
            true
        } else {
            false
        };
        tx.commit().await?;
        if updated {
            if let Some(previous) = d.reply_to.filter(|previous| *previous != message_id) {
                let _ = self.telegram.clear_menu(a.chat_id, previous).await;
            }
        } else if message != Some(message_id) {
            // A late upload/render cannot make a superseded question actionable.
            let _ = self.telegram.clear_menu(a.chat_id, message_id).await;
        }
        Ok(())
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
