use crate::*;
use domain::Id;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// Plaintext exists only while updating or projecting authenticated account data.
#[derive(Serialize, Deserialize, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
struct AccountDisplay {
    first_name: String,
    last_name: String,
    username: Option<String>,
}

fn name(value: Option<&str>) -> String {
    value
        .unwrap_or("")
        .chars()
        .filter(|character| {
            !matches!(
                character,
                '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            )
        })
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(128)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

impl Engine {
    /// Call with the authenticated Telegram sender's fields. Missing first_name
    /// means an incomplete update: it must not erase previously observed names.
    pub async fn update_account_display(
        &self,
        actor: Id,
        first_name: Option<&str>,
        last_name: Option<&str>,
        username: Option<&str>,
    ) -> Result<()> {
        if first_name.is_none() {
            return Ok(());
        }
        let metadata = AccountDisplay {
            first_name: name(first_name),
            last_name: name(last_name),
            username: username
                .map(str::trim)
                .filter(|value| {
                    !value.is_empty()
                        && value.len() <= 64
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                })
                .map(str::to_owned),
        };
        let mut tx = self.db.begin().await?;
        tx.lock(Kind::Account, actor).await?;
        let mut account: Account = get(&mut *tx, actor).await?;
        if let Some(envelope) = &account.display {
            let previous: AccountDisplay = serde_json::from_slice(&self.crypto.unwrap(
                "account-display",
                account.id,
                envelope,
            )?)
            .map_err(|_| Error::Crypto)?;
            if previous == metadata {
                return Ok(());
            }
        }
        let bytes = Zeroizing::new(serde_json::to_vec(&metadata).map_err(|_| Error::Internal)?);
        account.display = Some(self.crypto.wrap("account-display", account.id, &bytes)?);
        put(&mut *tx, None, &account).await?;
        tx.commit().await
    }

    /// Project an account already loaded within the caller's authorized scope.
    /// The resulting name is display-only; use numeric IDs for every permission.
    pub fn account_display_name(&self, account: &Account) -> Result<Option<String>> {
        Ok(self.account_display_parts(account)?.0)
    }

    pub(crate) fn account_display_parts(
        &self,
        account: &Account,
    ) -> Result<(Option<String>, Option<String>)> {
        let Some(envelope) = &account.display else {
            return Ok((None, None));
        };
        let metadata: AccountDisplay = serde_json::from_slice(&self.crypto.unwrap(
            "account-display",
            account.id,
            envelope,
        )?)
        .map_err(|_| Error::Crypto)?;
        let display = [&metadata.first_name, &metadata.last_name]
            .into_iter()
            .filter(|value| !value.is_empty())
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        let display = if display.is_empty() {
            metadata
                .username
                .as_ref()
                .map(|username| format!("@{username}"))
        } else {
            Some(display)
        };
        Ok((display, metadata.username.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_accounts_without_display_metadata_remain_readable() {
        let account: Account = serde_json::from_value(serde_json::json!({
            "id": Id::from_u128(1001), "telegram_id": 1001, "chat_id": 1001, "locale": "uk"
        }))
        .unwrap();
        assert!(account.display.is_none());
        assert_eq!(account.telegram_id, 1001);
        assert_eq!(account.chat_id, 1001);
        assert!(
            serde_json::to_value(account)
                .unwrap()
                .get("display")
                .is_none()
        );
    }
}
