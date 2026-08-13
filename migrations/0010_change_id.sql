ALTER TABLE mail ADD COLUMN change_id TEXT;
CREATE INDEX IF NOT EXISTS idx_mail_change_id ON mail(change_id);
