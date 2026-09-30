//! Navigation stays in the Telegram adapter; each operation still goes through Engine.
use super::*;

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
        // Navigation resolves the current plan again, so Back remains usable
        // after STOP/check-in changes its epoch. Mutating actions stay scoped.
        self.b(a, name, None, None, false).await
    }

    pub(super) async fn back(&self, a: &Account, destination: &str) -> Result<(String, String)> {
        self.button(a, destination, None, None, false, &tr(&a.locale, "back"))
            .await
    }

    pub(super) async fn menu(&self, a: &Account, menu: Menu, message: Option<i64>) -> Result<()> {
        // Leaving a code/recovery prompt cancels that input step. Draft dialogs
        // are kept so opening Settings does not discard an unfinished secret.
        let mut tx = self.engine.db.begin().await?;
        if let Some(raw) = tx.get(Kind::Dialog, a.id).await? {
            let dialog: Dialog = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            if dialog.draft_id.is_none() {
                tx.remove(Kind::Dialog, a.id).await?;
            }
        }
        tx.commit().await?;
        let owned = match self.engine.own_plan(a.id).await {
            Ok((_, plan)) => Some(plan),
            Err(Error::NotFound) => None,
            Err(error) => return Err(error),
        };
        let mut buttons = Vec::new();
        let text = match menu {
            Menu::Home => {
                if let Some(plan) = &owned {
                    buttons.push(self.b(a, "checkin", None, Some(plan.id), true).await?);
                    buttons.push(self.nav(a, "plan-menu").await?);
                } else {
                    buttons.push(self.nav(a, "create").await?);
                }
                buttons.push(self.nav(a, "guardians").await?);
                buttons.push(self.nav(a, "settings").await?);
                if let Some(plan) = &owned {
                    buttons.push(self.b(a, "stop", None, Some(plan.id), true).await?);
                    tr(&a.locale, "home")
                } else {
                    tr(&a.locale, "welcome")
                }
            }
            Menu::Plan => {
                let plan = owned.as_ref().ok_or(Error::NotFound)?;
                for name in ["secrets", "new-secret", "participants"] {
                    buttons.push(self.b(a, name, None, Some(plan.id), true).await?);
                }
                if matches!(
                    plan.state,
                    domain::PlanState::Setup | domain::PlanState::Paused
                ) {
                    buttons.push(self.b(a, "resume", None, Some(plan.id), true).await?);
                }
                buttons.push(self.back(a, "home").await?);
                format!(
                    "{}\n\n{}: {}",
                    tr(&a.locale, "plan-menu"),
                    tr(&a.locale, "status"),
                    crate::localization::state(&a.locale, &plan.state)
                )
            }
            Menu::Settings => {
                buttons.push(self.nav(a, "language").await?);
                buttons.push(self.nav(a, "recovery-options").await?);
                if let Some(plan) = &owned {
                    buttons.push(self.b(a, "delete", None, Some(plan.id), true).await?);
                }
                buttons.push(self.back(a, "home").await?);
                tr(&a.locale, "settings")
            }
            Menu::Language => {
                for (locale, label) in [("uk", "Українська"), ("en", "English")] {
                    let label = if a.locale == locale {
                        format!("✓ {label}")
                    } else {
                        label.into()
                    };
                    buttons.push(
                        self.button(a, &format!("lang-{locale}"), None, None, false, &label)
                            .await?,
                    );
                }
                buttons.push(self.back(a, "settings").await?);
                tr(&a.locale, "choose-language")
            }
            Menu::Recovery => {
                if let Some(plan) = &owned {
                    buttons.push(
                        self.b(a, "rotate-recovery", None, Some(plan.id), true)
                            .await?,
                    );
                }
                buttons.push(self.nav(a, "recover").await?);
                buttons.push(self.nav(a, "recoverstop").await?);
                buttons.push(self.back(a, "settings").await?);
                tr(&a.locale, "recovery-options")
            }
        };
        if let Some(message) = message {
            match self
                .telegram
                .edit_menu(a.chat_id, message, &text, buttons.clone())
                .await
            {
                SendResult::Sent(_) => return Ok(()),
                // A removed/uneditable menu can be reopened as a new message.
                SendResult::Permanent => {}
                SendResult::RetryAfter(_) | SendResult::Unknown => {
                    return Err(Error::MessageUnavailable);
                }
            }
        }
        self.text(a, &text, buttons).await?;
        Ok(())
    }
}
