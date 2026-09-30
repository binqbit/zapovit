-- A routine backup owns only its maintenance session. Recovery remains a
-- separate, explicitly witnessed procedure and must never be cleared by a stale backup.
ALTER TABLE maintenance ADD COLUMN backup_session uuid;
ALTER TABLE maintenance ADD COLUMN backup_started_at timestamptz;
ALTER TABLE maintenance ADD COLUMN backup_drained boolean NOT NULL DEFAULT false;
ALTER TABLE maintenance ADD COLUMN backup_checkpoint jsonb;
ALTER TABLE maintenance ADD COLUMN restore_required boolean NOT NULL DEFAULT false;
ALTER TABLE maintenance ADD COLUMN last_backup_at timestamptz;
ALTER TABLE maintenance ADD COLUMN restore_checkpoint jsonb;
ALTER TABLE maintenance ADD CONSTRAINT backup_session_requires_maintenance
  CHECK (backup_session IS NULL OR (enabled AND backup_started_at IS NOT NULL));
