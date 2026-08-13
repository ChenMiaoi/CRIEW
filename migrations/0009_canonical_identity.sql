-- Canonical identities are separate from source membership.  A single RFC
-- message may be present in the Inbox, a mailing-list archive, and a local
-- Following view at the same time; the old `mail` columns could only retain
-- the last source that happened to be synchronized.
CREATE TABLE IF NOT EXISTS mail_source (
    mail_id INTEGER NOT NULL,
    source_key TEXT NOT NULL,
    remote_uid INTEGER,
    modseq INTEGER,
    flags TEXT,
    raw_path TEXT,
    is_expunged INTEGER NOT NULL DEFAULT 0,
    first_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (mail_id, source_key),
    FOREIGN KEY (mail_id) REFERENCES mail(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_mail_source_source_active
ON mail_source(source_key, is_expunged, last_seen_at);

INSERT OR IGNORE INTO mail_source(
    mail_id, source_key, remote_uid, modseq, flags, raw_path, is_expunged
)
SELECT
    id,
    COALESCE(imap_mailbox, 'legacy'),
    imap_uid,
    modseq,
    flags,
    raw_path,
    is_expunged
FROM mail;

-- Old databases may contain duplicate UID rows after a reconnect. Keep the
-- earliest membership before adding the source/UID uniqueness guarantee.
DELETE FROM mail_source
WHERE remote_uid IS NOT NULL
  AND rowid NOT IN (
      SELECT MIN(rowid)
      FROM mail_source
      WHERE remote_uid IS NOT NULL
      GROUP BY source_key, remote_uid
  );

CREATE UNIQUE INDEX IF NOT EXISTS idx_mail_source_uid
ON mail_source(source_key, remote_uid)
WHERE remote_uid IS NOT NULL AND is_expunged = 0;

-- Thread IDs are database row IDs and must not be used as durable identities.
-- The stable key is rooted in the canonical root Message-ID and survives
-- materialization/rethreading.
ALTER TABLE thread ADD COLUMN stable_key TEXT;
UPDATE thread
SET stable_key = 'msg:' || (
    SELECT message_id FROM mail WHERE mail.id = thread.root_mail_id
)
WHERE stable_key IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_thread_stable_key
ON thread(stable_key)
WHERE stable_key IS NOT NULL;

-- Series are independent from conversation threading.  A reroll can be
-- posted in a new thread and an unthreaded series can have no usable root.
CREATE TABLE IF NOT EXISTS patch_change (
    id INTEGER PRIMARY KEY,
    change_key TEXT NOT NULL UNIQUE,
    change_id TEXT,
    subject TEXT NOT NULL DEFAULT '',
    author TEXT NOT NULL DEFAULT '',
    source_key TEXT,
    resolver TEXT NOT NULL DEFAULT 'heuristic',
    confidence TEXT NOT NULL DEFAULT 'low',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS patch_revision (
    id INTEGER PRIMARY KEY,
    change_id INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    expected_total INTEGER NOT NULL DEFAULT 0,
    cover_mail_id INTEGER,
    completeness TEXT NOT NULL DEFAULT 'discovered',
    status TEXT NOT NULL DEFAULT 'new',
    fingerprint TEXT,
    thread_key TEXT,
    first_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(change_id, revision),
    FOREIGN KEY(change_id) REFERENCES patch_change(id) ON DELETE CASCADE,
    FOREIGN KEY(cover_mail_id) REFERENCES mail(id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS patch_member (
    revision_id INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    total INTEGER NOT NULL,
    mail_id INTEGER NOT NULL,
    subject TEXT NOT NULL DEFAULT '',
    patch_id TEXT,
    is_cover INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(revision_id, seq, mail_id),
    FOREIGN KEY(revision_id) REFERENCES patch_revision(id) ON DELETE CASCADE,
    FOREIGN KEY(mail_id) REFERENCES mail(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_patch_member_revision_seq
ON patch_member(revision_id, seq);

CREATE TABLE IF NOT EXISTS patch_followup (
    mail_id INTEGER PRIMARY KEY,
    revision_id INTEGER NOT NULL,
    member_seq INTEGER,
    relation TEXT NOT NULL DEFAULT 'discussion',
    confidence TEXT NOT NULL DEFAULT 'low',
    FOREIGN KEY(mail_id) REFERENCES mail(id) ON DELETE CASCADE,
    FOREIGN KEY(revision_id) REFERENCES patch_revision(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_patch_followup_revision
ON patch_followup(revision_id, member_seq);
