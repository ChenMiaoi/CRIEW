CREATE TABLE IF NOT EXISTS reply_draft (
    id INTEGER PRIMARY KEY,
    thread_id INTEGER NOT NULL,
    mail_id INTEGER NOT NULL,
    from_addr TEXT NOT NULL DEFAULT '',
    to_addrs TEXT NOT NULL DEFAULT '',
    cc_addrs TEXT NOT NULL DEFAULT '',
    subject TEXT NOT NULL DEFAULT '',
    in_reply_to TEXT NOT NULL DEFAULT '',
    references_text TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    preview_confirmed_at TEXT,
    status TEXT NOT NULL DEFAULT 'draft',
    draft_path TEXT,
    last_error TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (thread_id) REFERENCES thread(id) ON DELETE CASCADE,
    FOREIGN KEY (mail_id) REFERENCES mail(id) ON DELETE CASCADE,
    UNIQUE (thread_id, mail_id)
);

CREATE INDEX IF NOT EXISTS idx_reply_draft_status
ON reply_draft (status, updated_at DESC, id DESC);

CREATE INDEX IF NOT EXISTS idx_reply_draft_thread
ON reply_draft (thread_id, updated_at DESC, id DESC);
