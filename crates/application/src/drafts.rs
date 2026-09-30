use crate::*;
use domain::{Block, DAY, Id, RuleError, validate_blocks};
use zeroize::Zeroizing;

impl Engine {
    pub async fn remove_draft_block(
        &self,
        actor: Id,
        id: Id,
        revision: i64,
        index: usize,
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let (mut draft, _) = self.editable(&mut *tx, actor, id).await?;
        if draft.revision != revision {
            return Err(RuleError::StaleAction.into());
        }
        let mut blocks: Vec<Block> =
            serde_json::from_slice(&self.crypto.unwrap("draft", id, &draft.payload)?)
                .map_err(|_| Error::Crypto)?;
        if index >= blocks.len() {
            return Err(Error::InvalidInput);
        }
        let removed = blocks.remove(index);
        if let Block::File { file, .. } = &removed {
            let mut object: FileObject = get(&mut *tx, file.id).await?;
            if object.draft_id != id || object.state != "draft" {
                return Err(Error::Internal);
            }
            object.state = "gc".into();
            object.due_at = tx.now().await?;
            put(&mut *tx, Some(draft.plan_id), &object).await?;
        }
        self.store_draft_blocks(&mut *tx, &mut draft, &blocks)
            .await?;
        tx.commit().await
    }

    pub async fn move_draft_block(
        &self,
        actor: Id,
        id: Id,
        revision: i64,
        from: usize,
        to: usize,
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let (mut draft, _) = self.editable(&mut *tx, actor, id).await?;
        if draft.revision != revision {
            return Err(RuleError::StaleAction.into());
        }
        let mut blocks: Vec<Block> =
            serde_json::from_slice(&self.crypto.unwrap("draft", id, &draft.payload)?)
                .map_err(|_| Error::Crypto)?;
        if from >= blocks.len() || to >= blocks.len() {
            return Err(Error::InvalidInput);
        }
        let block = blocks.remove(from);
        blocks.insert(to, block);
        self.store_draft_blocks(&mut *tx, &mut draft, &blocks)
            .await?;
        tx.commit().await
    }

    async fn store_draft_blocks(
        &self,
        tx: &mut dyn Transaction,
        draft: &mut Draft,
        blocks: &[Block],
    ) -> Result<()> {
        if !blocks.is_empty() {
            validate_blocks(blocks)?;
        }
        let bytes = Zeroizing::new(serde_json::to_vec(blocks).map_err(|_| Error::Internal)?);
        draft.payload = self.crypto.wrap("draft", draft.id, &bytes)?;
        draft.revision += 1;
        draft.expires_at = (tx.now().await? + 900).min(draft.created_at + 3600);
        put(tx, Some(draft.plan_id), draft).await
    }

    pub async fn cancel_draft(&self, actor: Id, id: Id, revision: i64) -> Result<()> {
        let mut tx = self.db.begin().await?;
        if let Some(raw) = tx.get(Kind::OperationReceipt, id).await? {
            let receipt: OperationReceipt =
                serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            return if receipt.actor_id == actor
                && receipt.operation == OperationKind::DraftCancelled
            {
                Ok(())
            } else {
                Err(RuleError::StaleAction.into())
            };
        }
        let (draft, _) = self.editable(&mut *tx, actor, id).await?;
        if draft.revision != revision {
            return Err(RuleError::StaleAction.into());
        }
        crate::control::remove_draft(&mut *tx, &draft).await?;
        if let Some(raw) = tx.get(Kind::DraftSession, actor).await? {
            let session: DraftSession = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            if session.dialog.draft_id == Some(id) {
                tx.remove(Kind::DraftSession, actor).await?;
            }
        }
        let now = tx.now().await?;
        Self::record_receipt(
            &mut *tx,
            OperationReceipt {
                id,
                actor_id: actor,
                plan_id: Some(draft.plan_id),
                secret_id: None,
                operation: OperationKind::DraftCancelled,
                status: ReceiptStatus::Completed,
                at: now,
                due_at: now + 7 * DAY,
                pending_objects: Default::default(),
            },
        )
        .await?;
        tx.commit().await
    }
}
