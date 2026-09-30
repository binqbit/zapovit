//! Role-specific cards. All projections and mutations pass through application use cases.
use super::*;
use domain::PartState;
use sha2::{Digest, Sha256};
const PAGE: usize = 6;

fn reference(id: Id) -> String {
    id.simple().to_string()[..8].to_owned()
}
fn secret_label(a: &Account, secret: &SecretOverview) -> String {
    secret.label.clone().unwrap_or_else(|| {
        format!(
            "{} · {}",
            tr(&a.locale, "secret-label"),
            reference(secret.id)
        )
    })
}
/// Keep compact Telegram labels bounded without splitting a Unicode scalar.
/// UTF-16 units also match Telegram's native entity offsets.
pub(super) fn short_label(text: &str) -> String {
    const LIMIT: usize = 60;
    if text.encode_utf16().count() <= LIMIT {
        return text.to_owned();
    }
    let mut result = String::new();
    let mut units = 0;
    for ch in text.chars() {
        if units + ch.len_utf16() > LIMIT - 1 {
            break;
        }
        result.push(ch);
        units += ch.len_utf16();
    }
    result.push('…');
    result
}
fn contact_name(a: &Account, person: &ContactView) -> String {
    person
        .label
        .clone()
        .or_else(|| person.display_name.clone())
        .unwrap_or_else(|| format!("{} · {}", tr(&a.locale, "person-label"), person.telegram_id))
}
pub(super) fn contact_label(a: &Account, person: &ContactView) -> String {
    short_label(&contact_name(a, person))
}
fn page_start(page: usize, length: usize) -> usize {
    page.min(length.saturating_sub(1) / PAGE) * PAGE
}
impl BotUi {
    pub(super) async fn control_feedback(
        &self,
        a: &Account,
        plan: Option<Id>,
        operation: Id,
        key: &str,
    ) -> Result<()> {
        // Protective mutations bypass ordinary admission; redundant acknowledgments
        // do not get an unbounded queue of their own. Receipts retain every outcome.
        match self
            .engine
            .limit(&format!("control-feedback:{}:{key}", a.id), 3, 60)
            .await
        {
            Ok(()) => {}
            Err(Error::RateLimited) => return Ok(()),
            Err(error) => return Err(error),
        }
        let digest =
            Sha256::digest([b"control-feedback:".as_slice(), operation.as_bytes()].concat());
        let job_id = Id::from_bytes(digest[..16].try_into().map_err(|_| Error::Internal)?);
        let mut tx = self.engine.db.begin().await?;
        if tx.get(Kind::Job, job_id).await?.is_none() {
            let now = tx.now().await?;
            put(
                &mut *tx,
                plan,
                &Job {
                    id: job_id,
                    plan_id: plan,
                    task: Task::Notice {
                        account_id: a.id,
                        key: key.into(),
                        buttons: vec![],
                    },
                    state: PartState::Queued,
                    due_at: now,
                    expires_at: now + DAY,
                    lease_until: 0,
                    lease_token: Id::nil(),
                    attempts: 0,
                    message_id: None,
                    priority: 0,
                },
            )
            .await?;
        }
        tx.commit().await
    }
    pub(super) async fn date(&self, a: &Account, at: i64) -> Result<String> {
        let minutes = self.engine.utc_offset_minutes(a.id).await?;
        Ok(format_date(at, minutes))
    }
    async fn pager(
        &self,
        a: &Account,
        prefix: &str,
        page: usize,
        length: usize,
        plan: Option<Id>,
        owner: bool,
    ) -> Result<Vec<(String, String)>> {
        let mut buttons = Vec::new();
        if page > 0 {
            buttons.push(
                self.button(
                    a,
                    &format!("{prefix}.{}", page - 1),
                    None,
                    plan,
                    owner,
                    &tr(&a.locale, "page-previous"),
                )
                .await?,
            );
        }
        if page.saturating_add(1).saturating_mul(PAGE) < length {
            buttons.push(
                self.button(
                    a,
                    &format!("{prefix}.{}", page + 1),
                    None,
                    plan,
                    owner,
                    &tr(&a.locale, "page-next"),
                )
                .await?,
            );
        }
        Ok(buttons)
    }
    pub(super) async fn secrets_page(
        &self,
        a: &Account,
        plan_id: Id,
        page: usize,
        message: Option<i64>,
    ) -> Result<()> {
        let view = self.engine.overview(a.id).await?;
        let plan = view
            .own
            .filter(|p| p.id == plan_id)
            .ok_or(RuleError::AccessDenied)?;
        let start = page_start(page, plan.secrets.len());
        let mut rows = Vec::new();
        for secret in plan.secrets.iter().skip(start).take(PAGE) {
            rows.push(vec![
                self.button(
                    a,
                    "secret-card",
                    Some(secret.id),
                    Some(plan.id),
                    true,
                    &format!(
                        "{} · {}",
                        secret_label(a, secret),
                        crate::localization::state(&a.locale, &secret.state)
                    ),
                )
                .await?,
            ]);
        }
        let pager = self
            .pager(
                a,
                "secrets-page",
                start / PAGE,
                plan.secrets.len(),
                Some(plan.id),
                true,
            )
            .await?;
        if !pager.is_empty() {
            rows.push(pager);
        }
        if let Some(d) = self.active_draft(a).await? {
            rows.push(vec![
                self.draft_button(a, &d, "continue", "continue-draft")
                    .await?,
            ]);
        } else if plan.can_create_draft {
            rows.push(vec![
                self.b(a, "new-secret", None, Some(plan.id), true).await?,
            ]);
        } else {
            rows.push(vec![
                self.b(a, "participants", None, Some(plan.id), true).await?,
            ]);
        }
        rows.push(vec![self.back(a, "home").await?]);
        let text = if plan.secrets.is_empty() {
            tr(&a.locale, "no-secrets")
        } else {
            format!(
                "{}\n{}: {} / {}",
                tr(&a.locale, "secrets"),
                tr(&a.locale, "ready-secrets"),
                plan.ready_secrets,
                plan.secrets.len()
            )
        };
        self.screen_grid(a, &text, rows, message).await
    }
    async fn secret_card(&self, a: &Account, id: Id, message: Option<i64>) -> Result<()> {
        let view = self.engine.overview(a.id).await?;
        let plan = view.own.ok_or(RuleError::AccessDenied)?;
        let secret = plan
            .secrets
            .iter()
            .find(|s| s.id == id)
            .ok_or(RuleError::AccessDenied)?;
        let l = &a.locale;
        let mut text = format!(
            "{}\n{}: {}\n\n{}: {} / {}\n{}: {} / {}\n{}: {} / {} / {}",
            secret_label(a, secret),
            tr(l, "status"),
            crate::localization::state(l, &secret.state),
            tr(l, "codes-saved"),
            secret.guardians.iter().filter(|g| g.ready).count(),
            secret.guardians.len(),
            tr(l, "threshold-label"),
            secret.threshold,
            secret.guardians.len(),
            tr(l, "timing-label"),
            secret.timing.reminder_seconds / DAY,
            secret.timing.inactivity_seconds / DAY,
            secret.timing.release_delay_seconds / DAY
        );
        let contacts = self.engine.contacts(a.id, plan.id).await?;
        if plan.state == domain::PlanState::Paused {
            text.push_str(&format!("\n\n{}", tr(l, "paused-explained")));
        }
        if !plan.operational.ready {
            text.push_str(&format!("\n\n{}", tr(l, "service-delayed")));
            if let Some(at) = plan.operational.hold_until {
                text.push_str(&format!(
                    "\n{}: {}",
                    tr(l, "hold-until"),
                    self.date(a, at).await?
                ));
            }
        }
        let names = |ids: Vec<Id>| {
            ids.into_iter()
                .map(|id| {
                    contacts
                        .iter()
                        .find(|p| p.account_id == id)
                        .map(|p| contact_label(a, p))
                        .unwrap_or_else(|| tr(l, "person-unavailable"))
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        text.push_str(&format!(
            "\n{}: {}\n{}: {}",
            tr(l, "guardians-label"),
            names(secret.guardians.iter().map(|g| g.account_id).collect()),
            tr(l, "recipients-label"),
            names(secret.recipients.clone())
        ));
        if let Some(at) = secret.provisioning_expires_at {
            text.push_str(&format!(
                "\n{}: {}",
                tr(l, "codes-before"),
                self.date(a, at).await?
            ));
        }
        if let Some(at) = secret.release_at {
            text.push_str(&format!(
                "\n{}: {}",
                tr(l, "release-not-before"),
                self.date(a, at).await?
            ));
        }
        if plan.state == domain::PlanState::Active && secret.can_stop {
            text.push_str(&format!(
                "\n{}: {}",
                tr(l, "confirm-before"),
                self.date(a, secret.inactivity_at).await?
            ));
        }
        if secret.state == SecretState::SetupFailed {
            text.push_str(&format!("\n\n{}", tr(l, "setup-failed-help")));
        }
        if secret.total_parts > 0 {
            text.push_str(&format!(
                "\n{}: {} / {}\n{}: {}",
                tr(l, "parts-sent"),
                secret.sent_parts,
                secret.total_parts,
                tr(l, "parts-unknown"),
                secret.unknown_parts
            ));
        }
        let mut rows = Vec::new();
        if secret.can_stop {
            rows.push(vec![
                self.b(a, "stop-secret", Some(id), Some(plan.id), true)
                    .await?,
            ]);
        }
        if secret.can_resume {
            rows.push(vec![
                self.b(a, "resume-secret-review", Some(id), Some(plan.id), true)
                    .await?,
            ]);
        }
        rows.push(vec![
            self.b(a, "secret-options", Some(id), Some(plan.id), true)
                .await?,
        ]);
        rows.push(vec![
            self.button(a, "secrets", None, Some(plan.id), true, &tr(l, "back"))
                .await?,
        ]);
        self.screen_grid(a, &text, rows, message).await
    }
    async fn secret_options(
        &self,
        a: &Account,
        plan: Id,
        id: Id,
        message: Option<i64>,
    ) -> Result<()> {
        let view = self.engine.overview(a.id).await?;
        let plan = view
            .own
            .as_ref()
            .filter(|p| p.id == plan)
            .ok_or(RuleError::AccessDenied)?;
        let secret = plan
            .secrets
            .iter()
            .find(|s| s.id == id)
            .ok_or(RuleError::AccessDenied)?;
        let mut rows = vec![vec![
            self.button(
                a,
                "rename-secret",
                Some(id),
                Some(plan.id),
                true,
                &tr(&a.locale, "rename"),
            )
            .await?,
        ]];
        if plan.can_create_draft {
            rows.push(vec![
                self.button(
                    a,
                    "new-secret",
                    None,
                    Some(plan.id),
                    true,
                    &tr(&a.locale, "create-another"),
                )
                .await?,
            ]);
        }
        rows.push(vec![
            self.b(a, "delete", Some(id), Some(plan.id), true).await?,
        ]);
        rows.push(vec![
            self.button(
                a,
                "secret-card",
                Some(id),
                Some(plan.id),
                true,
                &tr(&a.locale, "back"),
            )
            .await?,
        ]);
        self.screen_grid(
            a,
            &format!(
                "{}\n\n{}",
                secret_label(a, secret),
                tr(&a.locale, "secret-options")
            ),
            rows,
            message,
        )
        .await
    }
    pub(super) async fn people_page(
        &self,
        a: &Account,
        plan: Id,
        page: usize,
        message: Option<i64>,
    ) -> Result<()> {
        let mut people = self.engine.contacts(a.id, plan).await?;
        people.sort_by_key(|p| (p.archived, p.confirmed));
        let start = page_start(page, people.len());
        futures_util::future::join_all(
            people
                .iter()
                .skip(start)
                .take(PAGE)
                .map(|p| self.refresh_person_display(p)),
        )
        .await;
        let mut people = self.engine.contacts(a.id, plan).await?;
        people.sort_by_key(|p| (p.archived, p.confirmed));
        let start = page_start(page, people.len());
        let mut rows = Vec::new();
        for person in people.iter().skip(start).take(PAGE) {
            let mark = if person.archived {
                "—"
            } else if person.confirmed {
                "✓"
            } else {
                "○"
            };
            rows.push(vec![
                self.button(
                    a,
                    "person-card",
                    Some(person.id),
                    Some(plan),
                    true,
                    &format!("{mark} {}", contact_label(a, person)),
                )
                .await?,
            ]);
        }
        let pager = self
            .pager(
                a,
                "people-page",
                start / PAGE,
                people.len(),
                Some(plan),
                true,
            )
            .await?;
        if !pager.is_empty() {
            rows.push(pager);
        }
        if people.iter().any(|p| p.confirmed && !p.archived) {
            rows.push(vec![
                self.b(a, "continue-setup", None, Some(plan), true).await?,
            ]);
        }
        rows.push(vec![self.b(a, "invite", None, Some(plan), true).await?]);
        if !self.engine.invitations(a.id, plan).await?.is_empty() {
            rows.push(vec![
                self.b(a, "invitations", None, Some(plan), true).await?,
            ]);
        }
        rows.push(vec![self.back(a, "home").await?]);
        let text = if people.is_empty() {
            tr(&a.locale, "no-people")
        } else {
            tr(&a.locale, "people-explained")
        };
        self.screen_grid(
            a,
            &format!("{}\n\n{text}", tr(&a.locale, "participants")),
            rows,
            message,
        )
        .await
    }
    pub(super) async fn refresh_person_display(&self, person: &ContactView) {
        if person.display_name.is_some()
            || self
                .engine
                .limit(&format!("display-lookup:{}", person.account_id), 1, DAY)
                .await
                .is_err()
            || self
                .engine
                .limit("display-lookup:global", 30, 60)
                .await
                .is_err()
        {
            return;
        }
        if let Some(identity) = self.telegram.chat_identity(person.telegram_id).await {
            let _ = self
                .engine
                .update_account_display(
                    person.account_id,
                    Some(&identity.first_name),
                    identity.last_name.as_deref(),
                    identity.username.as_deref(),
                )
                .await;
        }
    }
    pub(super) async fn person_card(
        &self,
        a: &Account,
        plan: Id,
        id: Id,
        message: Option<i64>,
    ) -> Result<()> {
        let people = self.engine.contacts(a.id, plan).await?;
        let person = people
            .iter()
            .find(|p| p.id == id)
            .ok_or(RuleError::AccessDenied)?;
        self.refresh_person_display(person).await;
        let mut people = self.engine.contacts(a.id, plan).await?;
        people.sort_by_key(|p| (p.archived, p.confirmed));
        let position = people
            .iter()
            .position(|p| p.id == id)
            .ok_or(RuleError::AccessDenied)?;
        let p = &people[position];
        let l = &a.locale;
        let mut text = contact_label(a, p);
        if let Some(name) = &p.username {
            text.push_str(&format!("\n@{name}"));
        }
        if !p.confirmed {
            text.push_str(&format!("\nTelegram ID: {}", p.telegram_id));
        }
        text.push_str(&format!(
            "\n\n{}",
            tr(
                l,
                if p.archived {
                    "contact-archived"
                } else if p.confirmed {
                    "contact-confirmed"
                } else {
                    "confirm-person-explained"
                }
            )
        ));
        let mut rows = Vec::new();
        if !p.confirmed && !p.archived {
            rows.push(vec![
                self.b(a, "confirm-person", Some(id), Some(plan), true)
                    .await?,
            ]);
            rows.push(vec![
                self.b(a, "reject-person", Some(id), Some(plan), true)
                    .await?,
            ]);
        } else {
            if p.confirmed && !p.archived {
                rows.push(vec![
                    self.b(a, "continue-setup", None, Some(plan), true).await?,
                ]);
            }
            rows.push(vec![
                self.b(a, "person-details", Some(id), Some(plan), true)
                    .await?,
            ]);
        }
        rows.push(vec![
            self.button(
                a,
                &format!("people-page.{}", position / PAGE),
                None,
                Some(plan),
                true,
                &tr(l, "back"),
            )
            .await?,
        ]);
        self.screen_grid(a, &text, rows, message).await
    }
    pub(super) async fn person_confirmed(
        &self,
        a: &Account,
        plan: Id,
        id: Id,
        message: Option<i64>,
    ) -> Result<()> {
        let people = self.engine.contacts(a.id, plan).await?;
        let person = people
            .iter()
            .find(|p| p.id == id)
            .ok_or(RuleError::AccessDenied)?;
        self.screen_grid(
            a,
            &format!(
                "✅ {}\n\n{}",
                contact_label(a, person),
                tr(&a.locale, "person-confirmed-next")
            ),
            vec![
                vec![self.b(a, "continue-setup", None, Some(plan), true).await?],
                vec![
                    self.button(
                        a,
                        "participants",
                        None,
                        Some(plan),
                        true,
                        &tr(&a.locale, "back-to-people"),
                    )
                    .await?,
                ],
            ],
            message,
        )
        .await
    }
    pub(super) async fn label_saved(&self, a: &Account, plan: Id, target: Id) -> Result<()> {
        let mut tx = self.engine.db.begin().await?;
        tx.remove(Kind::Dialog, a.id).await?;
        let person = tx.get(Kind::Participant, target).await?.is_some();
        let draft = tx.get(Kind::Draft, target).await?.is_some();
        tx.commit().await?;
        if person {
            self.person_card(a, plan, target, None).await
        } else if draft {
            self.continue_setup(a, None).await
        } else {
            self.secret_card(a, target, None).await
        }
    }
    async fn person_details(
        &self,
        a: &Account,
        plan: Id,
        id: Id,
        message: Option<i64>,
    ) -> Result<()> {
        let people = self.engine.contacts(a.id, plan).await?;
        let p = people
            .iter()
            .find(|p| p.id == id)
            .ok_or(RuleError::AccessDenied)?;
        let l = &a.locale;
        let mut text = format!("{}\n\nTelegram ID: {}", contact_name(a, p), p.telegram_id);
        if let Some(name) = &p.username {
            text.push_str(&format!("\n@{name}"));
        }
        if p.delivery_failed {
            text.push_str(&format!("\n\n{}", tr(l, "contact-delivery-failed")));
        }
        if !p.dependent_secrets.is_empty() {
            text.push_str(&format!(
                "\n\n{}: {}\n{}",
                tr(l, "dependent-secrets"),
                p.dependent_secrets.len(),
                tr(l, "archive-explained")
            ));
        }
        let mut rows = vec![vec![
            self.button(
                a,
                "rename-person",
                Some(id),
                Some(plan),
                true,
                &tr(l, "rename"),
            )
            .await?,
        ]];
        if !p.archived {
            rows.push(vec![
                self.b(a, "archive-person", Some(id), Some(plan), true)
                    .await?,
            ]);
        }
        if !p.dependent_secrets.is_empty() {
            rows.push(vec![self.b(a, "secrets", None, Some(plan), true).await?]);
        }
        rows.push(vec![
            self.button(a, "person-card", Some(id), Some(plan), true, &tr(l, "back"))
                .await?,
        ]);
        self.screen_grid(a, &text, rows, message).await
    }
    async fn invitations_page(
        &self,
        a: &Account,
        plan: Id,
        page: usize,
        message: Option<i64>,
    ) -> Result<()> {
        let invitations = self.engine.invitations(a.id, plan).await?;
        let start = page_start(page, invitations.len());
        let mut rows = Vec::new();
        for i in invitations.iter().skip(start).take(PAGE) {
            if i.status == InvitationStatus::Pending {
                rows.push(vec![
                    self.button(
                        a,
                        "revoke-invite",
                        Some(i.id),
                        Some(plan),
                        true,
                        &format!("{} · {}", tr(&a.locale, "revoke-invite"), reference(i.id)),
                    )
                    .await?,
                ]);
            }
        }
        let pager = self
            .pager(
                a,
                "invitations-page",
                start / PAGE,
                invitations.len(),
                Some(plan),
                true,
            )
            .await?;
        if !pager.is_empty() {
            rows.push(pager);
        }
        rows.push(vec![
            self.button(
                a,
                "participants",
                None,
                Some(plan),
                true,
                &tr(&a.locale, "back"),
            )
            .await?,
        ]);
        let mut text = tr(&a.locale, "invitations");
        for i in invitations.iter().skip(start).take(PAGE) {
            text.push_str(&format!(
                "\n{} · {}",
                reference(i.id),
                tr(&a.locale, invitation_key(i.status))
            ));
        }
        if invitations.is_empty() {
            text.push_str(&format!("\n{}", tr(&a.locale, "no-items")));
        }
        self.screen_grid(a, &text, rows, message).await
    }
    pub(super) async fn invitation_screen(
        &self,
        a: &Account,
        id: Id,
        message: Option<i64>,
    ) -> Result<()> {
        let invite = self.engine.invitation(a.id, id).await?;
        let mut rows = Vec::new();
        let text = if invite.status == InvitationStatus::Pending {
            rows.push(vec![
                self.button(
                    a,
                    "accept-invite",
                    Some(id),
                    None,
                    false,
                    &tr(&a.locale, "accept-invite"),
                )
                .await?,
                self.button(
                    a,
                    "decline-invite",
                    Some(id),
                    None,
                    false,
                    &tr(&a.locale, "decline-invite"),
                )
                .await?,
            ]);
            format!(
                "{}\n\n{}: {}\n{}: {}",
                tr(&a.locale, "invitation-explained"),
                tr(&a.locale, "owner-label"),
                invite
                    .owner_display_name
                    .as_deref()
                    .map(short_label)
                    .unwrap_or_else(|| invite.owner_telegram_id.to_string()),
                tr(&a.locale, "invitation-until"),
                self.date(a, invite.expires_at).await?
            )
        } else {
            tr(&a.locale, invitation_key(invite.status))
        };
        rows.push(vec![self.nav(a, "home").await?]);
        self.screen_grid(a, &text, rows, message).await
    }
    pub(super) async fn inbox_page(
        &self,
        a: &Account,
        page: usize,
        message: Option<i64>,
    ) -> Result<()> {
        let mut view = self.engine.overview(a.id).await?;
        view.guardian
            .sort_by_key(|g| (!g.can_submit, !g.can_resend_code, g.secret_id));
        let start = page_start(page, view.guardian.len());
        let mut rows = Vec::new();
        for g in view.guardian.iter().skip(start).take(PAGE) {
            let key = if g.can_submit {
                "request-needs-action"
            } else if !g.code_ready {
                "codes-pending"
            } else {
                "contact-confirmed"
            };
            rows.push(vec![
                self.button(
                    a,
                    "guardian-card",
                    Some(g.secret_id),
                    Some(g.plan_id),
                    false,
                    &format!(
                        "{} · {} · {}",
                        g.owner_display_name
                            .as_deref()
                            .map(short_label)
                            .unwrap_or_else(|| g.owner_telegram_id.to_string()),
                        reference(g.secret_id),
                        tr(&a.locale, key)
                    ),
                )
                .await?,
            ]);
        }
        let pager = self
            .pager(
                a,
                "inbox-page",
                start / PAGE,
                view.guardian.len(),
                None,
                false,
            )
            .await?;
        if !pager.is_empty() {
            rows.push(pager);
        }
        rows.push(vec![
            self.nav(a, "receiving").await?,
            self.nav(a, "history").await?,
        ]);
        rows.push(vec![self.back(a, "home").await?]);
        let text = if view.guardian.is_empty() {
            tr(&a.locale, "inbox-empty")
        } else {
            tr(&a.locale, "inbox-explained")
        };
        self.screen_grid(a, &text, rows, message).await
    }
    async fn guardian_card(
        &self,
        a: &Account,
        id: Id,
        more: bool,
        message: Option<i64>,
    ) -> Result<()> {
        let view = self.engine.overview(a.id).await?;
        let g = view
            .guardian
            .iter()
            .find(|g| g.secret_id == id)
            .ok_or(RuleError::AccessDenied)?;
        let l = &a.locale;
        let mut text = format!(
            "{}: {}\n{}: {}\n{}: {}",
            tr(l, "owner-label"),
            g.owner_display_name
                .as_deref()
                .map(short_label)
                .unwrap_or_else(|| g.owner_telegram_id.to_string()),
            tr(l, "secret-reference"),
            reference(id),
            tr(l, "status"),
            crate::localization::state(l, &g.state)
        );
        let mut rows = Vec::new();
        if g.can_submit {
            text.push_str(&format!("\n\n{}", tr(l, "guardian-request")));
            rows.push(vec![
                self.b(a, "submit-code", g.case_id, Some(g.plan_id), false)
                    .await?,
            ]);
        } else if g.confirmed {
            text.push_str(&format!("\n{}", tr(l, "code-accepted")));
        }
        if g.can_resend_code {
            rows.push(vec![
                self.b(a, "resend-code", g.grant_id, Some(g.plan_id), false)
                    .await?,
            ]);
        }
        if let Some(at) = g.provisioning_expires_at.filter(|_| !g.code_ready) {
            text.push_str(&format!(
                "\n{}: {}",
                tr(l, "codes-before"),
                self.date(a, at).await?
            ));
        }
        if let Some(at) = g.case_expires_at {
            text.push_str(&format!(
                "\n{}: {}",
                tr(l, "request-until"),
                self.date(a, at).await?
            ));
        }
        if more {
            text.push_str(&format!("\n\n{}", tr(l, "cancellation-explained")));
            rows.push(vec![
                self.button(
                    a,
                    "request-cancel",
                    Some(id),
                    Some(g.plan_id),
                    false,
                    &tr(l, "cancel-secret"),
                )
                .await?,
            ]);
            rows.push(vec![
                self.button(
                    a,
                    "request-cancel",
                    None,
                    Some(g.plan_id),
                    false,
                    &tr(l, "cancel-plan"),
                )
                .await?,
            ]);
            let mut tx = self.engine.db.begin().await?;
            let cancellations = list::<Cancellation>(&mut *tx, Some(g.plan_id)).await?;
            tx.commit().await?;
            for c in cancellations.into_iter().filter(|c| {
                c.state == "open"
                    && c.members.contains(&a.id)
                    && c.secret_id.is_none_or(|sid| sid == id)
            }) {
                text.push_str(&format!(
                    "\n{}: {} / {}",
                    tr(l, "cancellation-votes"),
                    c.votes.len(),
                    c.members.len()
                ));
                if !c.votes.contains(&a.id) {
                    rows.push(vec![
                        self.b(a, "vote-cancel", Some(c.id), Some(g.plan_id), false)
                            .await?,
                    ]);
                }
            }
        } else {
            rows.push(vec![
                self.button(
                    a,
                    "guardian-more",
                    Some(id),
                    Some(g.plan_id),
                    false,
                    &tr(l, "more"),
                )
                .await?,
            ]);
        }
        rows.push(vec![
            self.back(a, "guardians").await?,
            self.nav(a, "home").await?,
        ]);
        self.screen_grid(a, &text, rows, message).await
    }
    async fn receiving_page(&self, a: &Account, page: usize, message: Option<i64>) -> Result<()> {
        let view = self.engine.overview(a.id).await?;
        let start = page_start(page, view.receiving.len());
        let mut rows = Vec::new();
        for r in view.receiving.iter().skip(start).take(PAGE) {
            rows.push(vec![
                self.button(
                    a,
                    "receiving-card",
                    Some(r.secret_id),
                    Some(r.plan_id),
                    false,
                    &format!(
                        "{} · {} · {}/{}",
                        r.owner_display_name
                            .as_deref()
                            .map(short_label)
                            .unwrap_or_else(|| r.owner_telegram_id.to_string()),
                        reference(r.secret_id),
                        r.parts
                            .iter()
                            .filter(|p| p.state == PartState::Sent)
                            .count(),
                        r.parts.len()
                    ),
                )
                .await?,
            ]);
        }
        let pager = self
            .pager(
                a,
                "receiving-page",
                start / PAGE,
                view.receiving.len(),
                None,
                false,
            )
            .await?;
        if !pager.is_empty() {
            rows.push(pager);
        }
        rows.push(vec![
            self.back(a, "guardians").await?,
            self.nav(a, "home").await?,
        ]);
        self.screen_grid(
            a,
            &tr(
                &a.locale,
                if view.receiving.is_empty() {
                    "receiving-empty"
                } else {
                    "receiving-explained"
                },
            ),
            rows,
            message,
        )
        .await
    }
    async fn receiving_card(
        &self,
        a: &Account,
        id: Id,
        page: usize,
        message: Option<i64>,
    ) -> Result<()> {
        let view = self.engine.overview(a.id).await?;
        let r = view
            .receiving
            .iter()
            .find(|r| r.secret_id == id)
            .ok_or(RuleError::AccessDenied)?;
        let start = page_start(page, r.parts.len());
        let l = &a.locale;
        let mut text = format!(
            "{}: {}\n{}: {}\n\n{}: {} / {}\n{}",
            tr(l, "owner-label"),
            r.owner_display_name
                .as_deref()
                .map(short_label)
                .unwrap_or_else(|| r.owner_telegram_id.to_string()),
            tr(l, "secret-reference"),
            reference(id),
            tr(l, "parts-sent"),
            r.parts
                .iter()
                .filter(|p| p.state == PartState::Sent)
                .count(),
            r.parts.len(),
            tr(l, "sent-not-read")
        );
        let mut rows = Vec::new();
        if r.parts.is_empty() {
            text.push_str(&format!("\n{}", tr(l, "receiving-waiting")));
        }
        for p in r.parts.iter().skip(start).take(PAGE) {
            text.push_str(&format!(
                "\n{} {} · {}",
                tr(l, "part"),
                p.index + 1,
                tr(l, part_key(p.state))
            ));
            if p.can_retry {
                rows.push(vec![
                    self.button(
                        a,
                        "retry-delivery",
                        Some(p.id),
                        Some(r.plan_id),
                        false,
                        &format!("{} · {}", tr(l, "retry-delivery"), p.index + 1),
                    )
                    .await?,
                ]);
            }
        }
        if let Some(at) = r.retry_until {
            text.push_str(&format!(
                "\n{}: {}",
                tr(l, "retry-until"),
                self.date(a, at).await?
            ));
        }
        if start > 0 {
            rows.push(vec![
                self.button(
                    a,
                    &format!("receiving-parts.{}", start / PAGE - 1),
                    Some(id),
                    Some(r.plan_id),
                    false,
                    &tr(l, "page-previous"),
                )
                .await?,
            ]);
        }
        if start + PAGE < r.parts.len() {
            rows.push(vec![
                self.button(
                    a,
                    &format!("receiving-parts.{}", start / PAGE + 1),
                    Some(id),
                    Some(r.plan_id),
                    false,
                    &tr(l, "page-next"),
                )
                .await?,
            ]);
        }
        rows.push(vec![
            self.back(a, "receiving").await?,
            self.nav(a, "home").await?,
        ]);
        self.screen_grid(a, &text, rows, message).await
    }
    pub(super) async fn history_page(
        &self,
        a: &Account,
        page: usize,
        message: Option<i64>,
    ) -> Result<()> {
        let view = self.engine.overview(a.id).await?;
        let start = page_start(page, view.receipts.len());
        let mut text = tr(&a.locale, "history-explained");
        for receipt in view.receipts.iter().skip(start).take(PAGE) {
            let operation = serde_json::to_value(receipt.operation).map_err(|_| Error::Internal)?;
            let key = format!(
                "event-{}",
                operation.as_str().ok_or(Error::Internal)?.replace('_', "-")
            );
            text.push_str(&format!(
                "\n\n{} · {}\n{}: {}",
                self.date(a, receipt.at).await?,
                tr(&a.locale, &key),
                tr(&a.locale, "operation-reference"),
                reference(receipt.id)
            ));
            if receipt.status == ReceiptStatus::CleanupPending {
                text.push_str(&format!("\n{}", tr(&a.locale, "cleanup-pending")));
            }
        }
        if view.receipts.is_empty() {
            text.push_str(&format!("\n{}", tr(&a.locale, "no-items")));
        }
        let mut rows = Vec::new();
        let pager = self
            .pager(
                a,
                "history-page",
                start / PAGE,
                view.receipts.len(),
                None,
                false,
            )
            .await?;
        if !pager.is_empty() {
            rows.push(pager);
        }
        rows.push(vec![
            self.nav(a, "help").await?,
            self.back(a, "home").await?,
        ]);
        self.screen_grid(a, &text, rows, message).await
    }
    async fn timezone_screen(&self, a: &Account, message: Option<i64>) -> Result<()> {
        let mut rows = Vec::new();
        for minutes in [0, 120, 180] {
            rows.push(vec![
                self.button(
                    a,
                    &format!("timezone.{minutes}"),
                    None,
                    None,
                    false,
                    &offset_label(minutes),
                )
                .await?,
            ]);
        }
        rows.push(vec![self.nav(a, "timezone-custom").await?]);
        rows.push(vec![self.back(a, "settings").await?]);
        self.screen_grid(
            a,
            &format!(
                "{}\n\n{}: {}",
                tr(&a.locale, "timezone-explained"),
                tr(&a.locale, "selected"),
                offset_label(self.engine.utc_offset_minutes(a.id).await?)
            ),
            rows,
            message,
        )
        .await
    }
    pub(super) async fn workspace_callback(
        &self,
        a: &Account,
        action: &Action,
        message: Option<i64>,
    ) -> Result<bool> {
        let plan = action.plan_id;
        let target = action.target;
        if matches!(
            action.name.as_str(),
            "person-card" | "person-details" | "secret-card" | "secret-options"
        ) {
            let mut tx = self.engine.db.begin().await?;
            tx.remove(Kind::Dialog, a.id).await?;
            tx.commit().await?;
        }
        match action.name.as_str() {
            "stop-cancel" => {
                let mut tx = self.engine.db.begin().await?;
                let mut confirmation: Action =
                    get(&mut *tx, target.ok_or(Error::InvalidInput)?).await?;
                if confirmation.actor_id != a.id
                    || confirmation.plan_id != plan
                    || !matches!(
                        confirmation.name.as_str(),
                        "stop-confirmed" | "stop-secret-confirmed"
                    )
                {
                    return Err(RuleError::AccessDenied.into());
                }
                confirmation.used = true;
                put(&mut *tx, plan, &confirmation).await?;
                tx.commit().await?;
                let result = if let Some(id) = confirmation.target {
                    self.secret_card(a, id, message).await
                } else {
                    self.menu(a, Menu::Home, message).await
                };
                match result {
                    Err(Error::Rule(RuleError::QuotaExceeded) | Error::RateLimited) => {
                        self.screen(a, &tr(&a.locale, "stop-cancelled"), vec![], message)
                            .await?
                    }
                    result => result?,
                }
            }
            "secret-card" => {
                self.secret_card(a, target.ok_or(Error::InvalidInput)?, message)
                    .await?
            }
            "person-card" => {
                self.person_card(
                    a,
                    plan.ok_or(Error::InvalidInput)?,
                    target.ok_or(Error::InvalidInput)?,
                    message,
                )
                .await?
            }
            "secret-options" => {
                self.secret_options(
                    a,
                    plan.ok_or(Error::InvalidInput)?,
                    target.ok_or(Error::InvalidInput)?,
                    message,
                )
                .await?;
            }
            "person-details" => {
                self.person_details(
                    a,
                    plan.ok_or(Error::InvalidInput)?,
                    target.ok_or(Error::InvalidInput)?,
                    message,
                )
                .await?;
            }
            "rename-secret" | "rename-person" => {
                let mut d = self.dialog(a, "label", plan, None).await?;
                d.case_id = target;
                self.store_dialog(&d).await?;
                self.screen(
                    a,
                    &tr(&a.locale, "label-prompt"),
                    vec![
                        self.button(
                            a,
                            if action.name == "rename-person" {
                                "person-details"
                            } else {
                                "secret-options"
                            },
                            target,
                            plan,
                            true,
                            &tr(&a.locale, "back"),
                        )
                        .await?,
                    ],
                    message,
                )
                .await?;
            }
            "reject-person" => {
                self.engine
                    .reject_participant(
                        a.id,
                        plan.ok_or(Error::InvalidInput)?,
                        target.ok_or(Error::InvalidInput)?,
                    )
                    .await?;
                self.people_page(a, plan.unwrap(), 0, message).await?;
            }
            "archive-person" => {
                self.engine
                    .archive_participant(
                        a.id,
                        plan.ok_or(Error::InvalidInput)?,
                        target.ok_or(Error::InvalidInput)?,
                    )
                    .await?;
                self.people_page(a, plan.unwrap(), 0, message).await?;
            }
            "invitations" => {
                self.invitations_page(a, plan.ok_or(Error::InvalidInput)?, 0, message)
                    .await?
            }
            "revoke-invite" => {
                self.engine
                    .revoke_invitation(a.id, target.ok_or(Error::InvalidInput)?)
                    .await?;
                self.invitations_page(a, plan.ok_or(Error::InvalidInput)?, 0, message)
                    .await?;
            }
            "accept-invite" => {
                self.engine
                    .accept_invite(a.id, target.ok_or(Error::InvalidInput)?)
                    .await?;
                self.screen(
                    a,
                    &tr(&a.locale, "joined"),
                    vec![self.nav(a, "home").await?],
                    message,
                )
                .await?;
            }
            "decline-invite" => {
                self.engine
                    .decline_invitation(a.id, target.ok_or(Error::InvalidInput)?)
                    .await?;
                self.invitation_screen(a, target.unwrap(), message).await?;
            }
            "guardian-card" | "guardian-more" => {
                self.guardian_card(
                    a,
                    target.ok_or(Error::InvalidInput)?,
                    action.name == "guardian-more",
                    message,
                )
                .await?
            }
            "receiving" => self.receiving_page(a, 0, message).await?,
            "receiving-card" => {
                self.receiving_card(a, target.ok_or(Error::InvalidInput)?, 0, message)
                    .await?
            }
            "history" => self.history_page(a, 0, message).await?,
            "help" => {
                self.screen(
                    a,
                    &tr(&a.locale, "help-text"),
                    vec![
                        self.nav(a, "service-status").await?,
                        self.nav(a, "privacy").await?,
                        self.back(a, "home").await?,
                    ],
                    message,
                )
                .await?
            }
            "privacy" => {
                self.screen(
                    a,
                    &tr(&a.locale, "privacy-text"),
                    vec![self.back(a, "help").await?],
                    message,
                )
                .await?
            }
            "service-status" => {
                let mut tx = self.engine.db.begin().await?;
                let status = tx.operational_status(None).await?;
                tx.commit().await?;
                self.screen(
                    a,
                    &tr(
                        &a.locale,
                        if status.ready {
                            "service-available"
                        } else {
                            "service-delayed"
                        },
                    ),
                    vec![self.nav(a, "history").await?, self.back(a, "help").await?],
                    message,
                )
                .await?;
            }
            "timezone" => self.timezone_screen(a, message).await?,
            "timezone-custom" => {
                self.dialog(a, "timezone", None, None).await?;
                self.screen(
                    a,
                    &tr(&a.locale, "timezone-prompt"),
                    vec![self.back(a, "settings").await?],
                    message,
                )
                .await?;
            }
            "resume-review" | "resume-secret-review" => {
                let name = if target.is_some() {
                    "resume-secret"
                } else {
                    "resume"
                };
                self.screen(
                    a,
                    &tr(&a.locale, "resume-explained"),
                    vec![
                        self.b(a, name, target, plan, true).await?,
                        self.back(a, "home").await?,
                    ],
                    message,
                )
                .await?;
            }
            "stop-secret" => {
                self.stop_review(a, plan.ok_or(Error::InvalidInput)?, target, message)
                    .await?
            }
            "stop-secret-confirmed" | "resume-secret" => {
                let id = target.ok_or(Error::InvalidInput)?;
                let op = if action.name == "stop-secret-confirmed" {
                    Control::StopSecret { secret_id: id }
                } else {
                    Control::RearmSecret { secret_id: id }
                };
                self.engine
                    .control(a.id, plan.ok_or(Error::InvalidInput)?, action.id, op)
                    .await?;
                if action.name == "stop-secret-confirmed" {
                    self.control_feedback(a, plan, action.id, "secret-stopped")
                        .await?;
                } else {
                    self.secret_card(a, id, message).await?;
                }
            }
            name if name.starts_with("timezone.") => {
                self.engine
                    .set_utc_offset_minutes(
                        a.id,
                        name[9..].parse().map_err(|_| Error::InvalidInput)?,
                    )
                    .await?;
                self.timezone_screen(a, message).await?;
            }
            name if name.contains("-page.") || name.starts_with("receiving-parts.") => {
                let (name, page) = name.rsplit_once('.').ok_or(Error::InvalidInput)?;
                let page = page.parse::<usize>().map_err(|_| Error::InvalidInput)?;
                match name {
                    "secrets-page" => {
                        self.secrets_page(a, plan.ok_or(Error::InvalidInput)?, page, message)
                            .await?
                    }
                    "people-page" => {
                        self.people_page(a, plan.ok_or(Error::InvalidInput)?, page, message)
                            .await?
                    }
                    "invitations-page" => {
                        self.invitations_page(a, plan.ok_or(Error::InvalidInput)?, page, message)
                            .await?
                    }
                    "inbox-page" => self.inbox_page(a, page, message).await?,
                    "receiving-page" => self.receiving_page(a, page, message).await?,
                    "receiving-parts" => {
                        self.receiving_card(a, target.ok_or(Error::InvalidInput)?, page, message)
                            .await?
                    }
                    "history-page" => self.history_page(a, page, message).await?,
                    _ => return Ok(false),
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}
fn invitation_key(status: InvitationStatus) -> &'static str {
    match status {
        InvitationStatus::Pending => "invitation-pending",
        InvitationStatus::Accepted => "joined",
        InvitationStatus::Declined => "invitation-declined",
        InvitationStatus::Revoked => "invitation-revoked",
        InvitationStatus::Expired => "invitation-expired",
        InvitationStatus::Unavailable => "invitation-unavailable",
    }
}
fn part_key(state: PartState) -> &'static str {
    match state {
        PartState::Sent => "part-sent",
        PartState::Unknown => "part-unknown",
        PartState::PermanentFailed => "part-failed",
        PartState::Cancelled => "part-cancelled",
        _ => "part-waiting",
    }
}
pub(super) fn parse_offset(text: &str) -> Option<i16> {
    let text = text.trim().strip_prefix("UTC").unwrap_or(text.trim());
    if text == "0" {
        return Some(0);
    }
    let sign = match text.as_bytes().first()? {
        b'+' => 1i16,
        b'-' => -1,
        _ => return None,
    };
    let (hours, minutes) = text[1..].split_once(':')?;
    if hours.len() != 2
        || minutes.len() != 2
        || !hours
            .bytes()
            .chain(minutes.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let hours = hours.parse::<i16>().ok()?;
    let minutes = minutes.parse::<i16>().ok()?;
    if !(0..60).contains(&minutes) || !(0..=14).contains(&hours) {
        return None;
    }
    let offset = sign * (hours * 60 + minutes);
    ((-720..=840).contains(&offset) && offset % 15 == 0).then_some(offset)
}
fn offset_label(minutes: i16) -> String {
    if minutes == 0 {
        "UTC".into()
    } else {
        format!(
            "UTC{}{:02}:{:02}",
            if minutes < 0 { "−" } else { "+" },
            minutes.abs() / 60,
            minutes.abs() % 60
        )
    }
}
fn format_date(at: i64, minutes: i16) -> String {
    let offset = time::UtcOffset::from_whole_seconds(i32::from(minutes) * 60);
    match time::OffsetDateTime::from_unix_timestamp(at)
        .ok()
        .zip(offset.ok())
    {
        Some((date, offset)) => {
            let date = date.to_offset(offset);
            format!(
                "{:02}.{:02}.{} {:02}:{:02} {}",
                date.day(),
                u8::from(date.month()),
                date.year(),
                date.hour(),
                date.minute(),
                offset_label(minutes)
            )
        }
        None => "—".into(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_labels_bound_utf16_without_splitting_emoji() {
        for text in ["Nadia Melnyk".to_owned(), "a".repeat(60), "😀".repeat(30)] {
            assert_eq!(short_label(&text), text);
        }
        for text in [
            "a".repeat(61),
            "😀".repeat(31),
            format!("{}😀bc", "a".repeat(57)),
            format!("{}😀b", "a".repeat(58)),
        ] {
            let short = short_label(&text);
            assert!(short.encode_utf16().count() <= 60);
            let prefix = short.strip_suffix('…').unwrap();
            assert!(text.starts_with(prefix));
            assert!(prefix.len() < text.len());
        }
        assert_eq!(short_label(&"a".repeat(61)), format!("{}…", "a".repeat(59)));
        assert_eq!(
            short_label(&format!("{}😀bc", "a".repeat(57))),
            format!("{}😀…", "a".repeat(57))
        );
    }
    #[test]
    fn utc_offsets_validate_boundaries_and_preserve_instant() {
        assert_eq!(parse_offset("UTC+05:45"), Some(345));
        assert_eq!(parse_offset("-12:00"), Some(-720));
        assert_eq!(parse_offset("+14:00"), Some(840));
        for value in [
            "+14:15",
            "-12:15",
            "+02:60",
            "+02:12",
            "+99:00",
            "UTC",
            "Europe/Kyiv",
            "+1:00",
        ] {
            assert_eq!(parse_offset(value), None, "{value}");
        }
        assert_eq!(format_date(0, 120), "01.01.1970 02:00 UTC+02:00");
        assert_eq!(page_start(usize::MAX, 7), 6);
    }
}
