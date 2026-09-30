//! Navigation is a projection of Engine state, never an authorization source.
use super::*;
use crate::telegram::MenuButton;

#[derive(Clone, Copy)]
pub(super) enum Menu {
    Home,
    Plan,
    Settings,
    Language,
    Recovery,
}

impl BotUi {
    pub(super) async fn nav(&self, a: &Account, name: &str) -> Result<(String, String)> {
        self.b(a, name, None, None, false).await
    }
    pub(super) async fn back(&self, a: &Account, destination: &str) -> Result<(String, String)> {
        self.button(a, destination, None, None, false, &tr(&a.locale, "back"))
            .await
    }
    pub(super) async fn menu(&self, a: &Account, menu: Menu, message: Option<i64>) -> Result<()> {
        // A sensitive prompt is cancelled on navigation; the separate draft session survives.
        let mut tx = self.engine.db.begin().await?;
        tx.remove(Kind::Dialog, a.id).await?;
        tx.commit().await?;
        let overview = self.engine.overview(a.id).await?;
        let mut rows = Vec::new();
        let text = match menu {
            Menu::Home | Menu::Plan => {
                if let Some(plan) = &overview.own {
                    if let Some(draft) = self.active_draft(a).await? {
                        rows.push(vec![
                            self.draft_button(a, &draft, "continue", "continue-draft")
                                .await?,
                        ]);
                    } else {
                        let next = match plan.next_action {
                            NextAction::SaveRecovery | NextAction::ResolveRecovery => {
                                "recovery-options"
                            }
                            NextAction::PreparePeople => "participants",
                            NextAction::CreateSecret if plan.can_create_draft => "new-secret",
                            NextAction::CreateSecret => "secrets",
                            NextAction::AwaitCodes => "secrets",
                            NextAction::ResumePlan if plan.can_resume => "resume-review",
                            NextAction::None
                                if plan.ready_secrets == 0 && !plan.secrets.is_empty() =>
                            {
                                "secrets"
                            }
                            _ => "checkin",
                        };
                        rows.push(vec![self.b(a, next, None, Some(plan.id), true).await?]);
                    }
                    rows.push(vec![
                        self.button(
                            a,
                            "secrets",
                            None,
                            Some(plan.id),
                            true,
                            &format!("{} · {}", tr(&a.locale, "secrets"), plan.secrets.len()),
                        )
                        .await?,
                        self.button(
                            a,
                            "participants",
                            None,
                            Some(plan.id),
                            true,
                            &format!(
                                "{} · {}",
                                tr(&a.locale, "participants"),
                                plan.confirmed_people
                            ),
                        )
                        .await?,
                    ]);
                    rows.push(vec![
                        self.nav(a, "guardians").await?,
                        self.nav(a, "settings").await?,
                    ]);
                    rows.push(vec![self.b(a, "stop", None, Some(plan.id), true).await?]);
                    self.plan_text(a, plan).await?
                } else {
                    if !overview.guardian.is_empty() || !overview.receiving.is_empty() {
                        rows.push(vec![
                            self.nav(a, "guardians").await?,
                            self.nav(a, "receiving").await?,
                        ]);
                    }
                    rows.push(vec![self.nav(a, "create").await?]);
                    rows.push(vec![
                        self.nav(a, "recovery-options").await?,
                        self.nav(a, "help").await?,
                    ]);
                    rows.push(vec![self.nav(a, "settings").await?]);
                    tr(&a.locale, "welcome")
                }
            }
            Menu::Settings => {
                rows.push(vec![
                    self.nav(a, "language").await?,
                    self.nav(a, "timezone").await?,
                ]);
                rows.push(vec![self.nav(a, "recovery-options").await?]);
                rows.push(vec![
                    self.nav(a, "history").await?,
                    self.nav(a, "help").await?,
                ]);
                if let Some(plan) = &overview.own {
                    rows.push(vec![
                        self.button(
                            a,
                            "delete",
                            None,
                            Some(plan.id),
                            true,
                            &tr(&a.locale, "delete-profile"),
                        )
                        .await?,
                    ]);
                }
                rows.push(vec![self.back(a, "home").await?]);
                tr(&a.locale, "settings")
            }
            Menu::Language => {
                for (locale, label) in [("uk", "Українська"), ("en", "English")] {
                    let label = if a.locale == locale {
                        format!("✓ {label}")
                    } else {
                        label.into()
                    };
                    rows.push(vec![
                        self.button(a, &format!("lang-{locale}"), None, None, false, &label)
                            .await?,
                    ]);
                }
                rows.push(vec![self.back(a, "settings").await?]);
                tr(&a.locale, "choose-language")
            }
            Menu::Recovery => {
                let mut text = tr(&a.locale, "recovery-explained");
                if let Some(plan) = &overview.own {
                    if plan.pending_claim.is_some() {
                        text.push_str(&format!(
                            "\n\n{}",
                            tr(&a.locale, "recovery-pending-guidance")
                        ));
                        if let Some(at) = plan.recovery_expires_at {
                            text.push_str(&format!(
                                "\n{}: {}",
                                tr(&a.locale, "request-until"),
                                self.date(a, at).await?
                            ));
                        }
                    } else {
                        if !plan.recovery_saved {
                            text.push_str(&format!(
                                "\n\n{}",
                                tr(&a.locale, "recovery-save-guidance")
                            ));
                        }
                        rows.push(vec![
                            self.b(a, "rotate-recovery", None, Some(plan.id), true)
                                .await?,
                        ]);
                    }
                }
                rows.push(vec![self.nav(a, "recover").await?]);
                rows.push(vec![self.nav(a, "recoverstop").await?]);
                rows.push(vec![self.back(a, "settings").await?]);
                text
            }
        };
        self.screen_grid(a, &text, rows, message).await
    }
    pub(super) async fn plan_text(&self, a: &Account, p: &PlanOverview) -> Result<String> {
        let l = &a.locale;
        let mut lines = vec![
            format!("Zapovit · {}", tr(l, "plan-menu")),
            format!(
                "{}: {}",
                tr(l, "status"),
                crate::localization::state(l, &p.state)
            ),
        ];
        if p.state == domain::PlanState::Setup {
            lines.push(format!("\n{}", tr(l, "setup-checklist")));
            for (complete, key) in [
                (p.recovery_saved, "setup-recovery"),
                (p.confirmed_people > 0, "setup-people"),
                (!p.secrets.is_empty(), "setup-secret"),
                (p.ready_secrets > 0, "setup-codes"),
            ] {
                lines.push(format!(
                    "{} {}",
                    if complete { "✓" } else { "○" },
                    tr(l, key)
                ));
            }
        } else {
            lines.push(format!(
                "{}: {} / {}",
                tr(l, "ready-secrets"),
                p.ready_secrets,
                p.secrets.len()
            ));
            lines.push(format!(
                "{}: {}",
                tr(l, "last-checkin"),
                self.date(a, p.last_activity).await?
            ));
            if let Some(at) = p.nearest_inactivity {
                lines.push(format!(
                    "{}: {}",
                    tr(l, "confirm-before"),
                    self.date(a, at).await?
                ));
            }
            if let Some(at) = p.next_reminder {
                lines.push(format!(
                    "{}: {}",
                    tr(l, "next-reminder"),
                    self.date(a, at).await?
                ));
            }
        }
        if p.state == domain::PlanState::Paused {
            lines.push(tr(l, "paused-explained"));
        }
        if p.pending_claim.is_some() {
            lines.push(tr(l, "recovery-pending"));
        }
        if !p.operational.ready {
            for reason in &p.operational.reasons {
                let key = match reason {
                    OperationalBlocker::Maintenance => "service-maintenance",
                    OperationalBlocker::Hold => "service-hold",
                    _ => "service-delayed",
                };
                let reason = tr(l, key);
                if !lines.contains(&reason) {
                    lines.push(reason);
                }
            }
            if let Some(at) = p.operational.hold_until {
                lines.push(format!(
                    "{}: {}",
                    tr(l, "hold-until"),
                    self.date(a, at).await?
                ));
            }
        }
        if p.blockers.contains(&ReadinessBlocker::ControlPending) {
            lines.push(tr(l, "control-pending"));
        }
        Ok(lines.join("\n"))
    }
    pub(super) async fn screen(
        &self,
        a: &Account,
        text: &str,
        buttons: Vec<(String, String)>,
        message: Option<i64>,
    ) -> Result<()> {
        self.screen_grid(
            a,
            text,
            buttons.into_iter().map(|b| vec![b]).collect(),
            message,
        )
        .await
    }
    pub(super) async fn screen_grid(
        &self,
        a: &Account,
        text: &str,
        rows: Vec<Vec<(String, String)>>,
        message: Option<i64>,
    ) -> Result<()> {
        let danger = [
            tr(&a.locale, "stop"),
            tr(&a.locale, "stop-secret"),
            tr(&a.locale, "delete-confirmed"),
            tr(&a.locale, "discard-confirmed"),
        ];
        let primary = [
            tr(&a.locale, "checkin"),
            tr(&a.locale, "create"),
            tr(&a.locale, "continue-draft"),
        ];
        let rows: Vec<Vec<MenuButton>> = rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|(text, data)| {
                        let style = if danger.contains(&text) {
                            Some("danger")
                        } else if primary.contains(&text) {
                            Some("primary")
                        } else {
                            None
                        };
                        MenuButton { text, data, style }
                    })
                    .collect()
            })
            .collect();
        if let Some(id) = message {
            match self
                .telegram
                .menu_message(a.chat_id, Some(id), text, rows.clone())
                .await
            {
                SendResult::Sent(_) => return Ok(()),
                SendResult::Permanent => {}
                _ => return Err(Error::MessageUnavailable),
            }
        }
        match self
            .telegram
            .menu_message(a.chat_id, None, text, rows)
            .await
        {
            SendResult::Sent(_) => Ok(()),
            _ => Err(Error::MessageUnavailable),
        }
    }
}
