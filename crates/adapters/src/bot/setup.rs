//! Conversational preparation is derived from authorized Engine state. Secret
//! answers use the existing encrypted Draft and its independent DraftSession.
use super::*;

const PEOPLE_PER_PAGE: usize = 6;

impl BotUi {
    pub(super) async fn setup_recovery(&self, a: &Account, plan: Id) -> Result<()> {
        self.screen(
            a,
            &format!(
                "{}\n\n{}",
                tr(&a.locale, "setup-recovery-title"),
                tr(&a.locale, "setup-recovery-question")
            ),
            vec![
                self.button(
                    a,
                    "setup-recovery-refresh",
                    None,
                    Some(plan),
                    true,
                    &tr(&a.locale, "setup-recovery-refresh"),
                )
                .await?,
                self.button(
                    a,
                    "recovery-options",
                    None,
                    Some(plan),
                    true,
                    &tr(&a.locale, "setup-key-missing"),
                )
                .await?,
            ],
            None,
        )
        .await
    }

    pub(super) async fn setup_people(
        &self,
        a: &Account,
        plan: Id,
        message: Option<i64>,
    ) -> Result<()> {
        self.setup_people_page(a, plan, 0, message).await
    }

    async fn setup_people_page(
        &self,
        a: &Account,
        plan: Id,
        page: usize,
        message: Option<i64>,
    ) -> Result<()> {
        let contacts = self.engine.contacts(a.id, plan).await?;
        let pending: Vec<_> = contacts
            .iter()
            .filter(|p| !p.archived && !p.confirmed)
            .collect();
        let page = page.min(pending.len().saturating_sub(1) / PEOPLE_PER_PAGE);
        futures_util::future::join_all(
            pending
                .iter()
                .skip(page * PEOPLE_PER_PAGE)
                .take(PEOPLE_PER_PAGE)
                .map(|p| self.refresh_person_display(p)),
        )
        .await;
        let contacts = self.engine.contacts(a.id, plan).await?;
        let confirmed: Vec<_> = contacts
            .iter()
            .filter(|p| p.confirmed && !p.archived)
            .collect();
        let pending: Vec<_> = contacts
            .iter()
            .filter(|p| !p.confirmed && !p.archived)
            .collect();
        let mut text = format!(
            "{}\n\n{}",
            tr(&a.locale, "setup-people-title"),
            tr(&a.locale, "setup-people-question")
        );
        if confirmed.is_empty() {
            text.push_str(&format!("\n\n{}", tr(&a.locale, "setup-no-people")));
        } else {
            text.push_str(&format!(
                "\n\n{}: {}",
                tr(&a.locale, "setup-confirmed"),
                confirmed
                    .iter()
                    .take(10)
                    .map(|p| workspace::contact_label(a, p))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            if confirmed.len() > 10 {
                text.push_str(&format!(" (+{})", confirmed.len() - 10));
            }
        }
        let mut buttons = Vec::new();
        if !confirmed.is_empty() {
            buttons.push(
                self.button(
                    a,
                    "setup-continue",
                    None,
                    Some(plan),
                    true,
                    &tr(&a.locale, "setup-next"),
                )
                .await?,
            );
        }
        if !pending.is_empty() {
            text.push_str(&format!(
                "\n\n{}: {}",
                tr(&a.locale, "setup-pending"),
                pending.len()
            ));
            for person in pending
                .iter()
                .skip(page * PEOPLE_PER_PAGE)
                .take(PEOPLE_PER_PAGE)
            {
                buttons.push(
                    self.button(
                        a,
                        "setup-person",
                        Some(person.id),
                        Some(plan),
                        true,
                        &format!("○ {}", workspace::contact_label(a, person)),
                    )
                    .await?,
                );
            }
            if page > 0 {
                buttons.push(
                    self.button(
                        a,
                        &format!("setup-people.{}", page - 1),
                        None,
                        Some(plan),
                        true,
                        &tr(&a.locale, "page-previous"),
                    )
                    .await?,
                );
            }
            if (page + 1) * PEOPLE_PER_PAGE < pending.len() {
                buttons.push(
                    self.button(
                        a,
                        &format!("setup-people.{}", page + 1),
                        None,
                        Some(plan),
                        true,
                        &tr(&a.locale, "page-next"),
                    )
                    .await?,
                );
            }
        }
        buttons.push(
            self.button(
                a,
                "setup-invite",
                None,
                Some(plan),
                true,
                &tr(&a.locale, "setup-add-person"),
            )
            .await?,
        );
        buttons.push(
            self.button(
                a,
                "setup-people",
                None,
                Some(plan),
                true,
                &tr(&a.locale, "setup-check-people"),
            )
            .await?,
        );
        self.screen(a, &text, buttons, message).await
    }

    async fn setup_person(&self, a: &Account, plan: Id, person: Id) -> Result<()> {
        let contacts = self.engine.contacts(a.id, plan).await?;
        let person = contacts
            .iter()
            .find(|p| p.id == person && !p.archived)
            .ok_or(RuleError::AccessDenied)?;
        if person.confirmed {
            return self.setup_people(a, plan, None).await;
        }
        let mut text = format!(
            "{}\n\n{}",
            workspace::contact_label(a, person),
            tr(&a.locale, "setup-person-question")
        );
        if let Some(username) = &person.username {
            text.push_str(&format!("\n\n@{username}"));
        }
        text.push_str(&format!("\nTelegram ID: {}", person.telegram_id));
        self.screen(
            a,
            &text,
            vec![
                self.button(
                    a,
                    "setup-confirm",
                    Some(person.id),
                    Some(plan),
                    true,
                    &tr(&a.locale, "setup-confirm-person"),
                )
                .await?,
                self.button(
                    a,
                    "setup-reject",
                    Some(person.id),
                    Some(plan),
                    true,
                    &tr(&a.locale, "setup-reject-person"),
                )
                .await?,
                self.button(
                    a,
                    "setup-people",
                    None,
                    Some(plan),
                    true,
                    &tr(&a.locale, "back"),
                )
                .await?,
            ],
            None,
        )
        .await
    }

    pub(super) async fn setup_status(
        &self,
        a: &Account,
        plan: Id,
        message: Option<i64>,
    ) -> Result<()> {
        let view = self.engine.overview(a.id).await?;
        let owned = view
            .own
            .filter(|p| p.id == plan)
            .ok_or(RuleError::AccessDenied)?;
        let codes_pending = owned
            .secrets
            .iter()
            .any(|s| s.state == SecretState::Provisioning);
        let displayed_secret = owned
            .secrets
            .iter()
            .rev()
            .find(|s| s.state == SecretState::Provisioning)
            .or_else(|| owned.secrets.last());
        let latest_ready = displayed_secret
            .is_some_and(|secret| secret.state == SecretState::Armed && secret.blockers.is_empty());
        if !codes_pending
            && (!latest_ready
                || (!owned.can_resume
                    && !(owned.state == domain::PlanState::Active && owned.ready_secrets > 0)))
        {
            // An older ready secret cannot complete setup for a newer stopped,
            // expired, partially delivered or otherwise blocked secret.
            return self.secrets_page(a, plan, 0, message).await;
        }
        let key = if !codes_pending
            && owned.state == domain::PlanState::Active
            && owned.ready_secrets > 0
        {
            "setup-active"
        } else if !codes_pending && owned.can_resume {
            "setup-ready-question"
        } else {
            "setup-wait-codes"
        };
        let mut text = format!(
            "{}\n\n{}",
            tr(&a.locale, "setup-finish-title"),
            tr(&a.locale, key)
        );
        if let Some(secret) = displayed_secret {
            let label = secret
                .label
                .clone()
                .unwrap_or_else(|| secret.id.simple().to_string()[..8].to_owned());
            text.push_str(&format!(
                "\n\n{}: {}",
                tr(&a.locale, "secret-label"),
                workspace::short_label(&label)
            ));
            let contacts = self.engine.contacts(a.id, plan).await?;
            for guardian in &secret.guardians {
                let name = contacts
                    .iter()
                    .find(|p| p.account_id == guardian.account_id)
                    .map(|p| workspace::contact_label(a, p))
                    .unwrap_or_else(|| tr(&a.locale, "person-unavailable"));
                text.push_str(&format!(
                    "\n{} {name}",
                    if guardian.ready { "✓" } else { "○" }
                ));
            }
        }
        let mut buttons = Vec::new();
        if !codes_pending && owned.can_resume {
            buttons.push(
                self.button(
                    a,
                    "resume",
                    None,
                    Some(plan),
                    true,
                    &tr(&a.locale, "setup-enable"),
                )
                .await?,
            );
        }
        buttons.push(
            self.button(
                a,
                "setup-ready",
                None,
                Some(plan),
                true,
                &tr(&a.locale, "setup-check-readiness"),
            )
            .await?,
        );
        buttons.push(self.nav(a, "home").await?);
        self.screen(a, &text, buttons, message).await
    }

    pub(super) async fn setup_callback(
        &self,
        a: &Account,
        action: &Action,
        message: Option<i64>,
    ) -> Result<()> {
        let plan = action.plan_id.ok_or(Error::InvalidInput)?;
        let mut tx = self.engine.db.begin().await?;
        tx.remove(Kind::Dialog, a.id).await?;
        tx.commit().await?;
        match action.name.as_str() {
            "setup-continue" | "setup-recovery-refresh" => self.continue_setup(a, None).await?,
            "setup-ready" => self.setup_status(a, plan, None).await?,
            "setup-people" => self.setup_people(a, plan, None).await?,
            "setup-person" => {
                self.setup_person(a, plan, action.target.ok_or(Error::InvalidInput)?)
                    .await?
            }
            "setup-confirm" => {
                self.engine
                    .confirm_participant(a.id, plan, action.target.ok_or(Error::InvalidInput)?)
                    .await?;
                self.setup_people(a, plan, None).await?;
            }
            "setup-reject" => {
                self.engine
                    .reject_participant(a.id, plan, action.target.ok_or(Error::InvalidInput)?)
                    .await?;
                self.setup_people(a, plan, None).await?;
            }
            "setup-invite" => {
                let id = self.engine.invite_with_id(a.id, plan, action.id).await?;
                self.screen(
                    a,
                    &format!(
                        "{}\n\n{}\n\nhttps://t.me/{}?start=invite_{}",
                        tr(&a.locale, "setup-invite-title"),
                        tr(&a.locale, "setup-invite-question"),
                        self.username,
                        id.simple()
                    ),
                    vec![
                        self.button(
                            a,
                            "setup-people",
                            None,
                            Some(plan),
                            true,
                            &tr(&a.locale, "setup-check-people"),
                        )
                        .await?,
                        self.button(
                            a,
                            "setup-people",
                            None,
                            Some(plan),
                            true,
                            &tr(&a.locale, "back"),
                        )
                        .await?,
                    ],
                    None,
                )
                .await?;
            }
            name if name.starts_with("setup-people.") => {
                let page = name[13..]
                    .parse::<usize>()
                    .map_err(|_| Error::InvalidInput)?;
                return self.setup_people_page(a, plan, page, message).await;
            }
            _ => return Err(Error::InvalidInput),
        }
        if let Some(id) = message {
            let _ = self.telegram.clear_menu(a.chat_id, id).await;
        }
        Ok(())
    }
}
