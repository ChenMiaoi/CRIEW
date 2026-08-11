CREATE TABLE IF NOT EXISTS mail_recipient (
    mail_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    address TEXT NOT NULL,
    ord INTEGER NOT NULL,
    PRIMARY KEY (mail_id, kind, ord),
    FOREIGN KEY (mail_id) REFERENCES mail(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_mail_recipient_address_kind
ON mail_recipient (address, kind, mail_id);

CREATE TABLE IF NOT EXISTS following_update (
    id INTEGER PRIMARY KEY,
    mail_id INTEGER NOT NULL,
    thread_id INTEGER,
    kind TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    seen_at TEXT,
    UNIQUE (mail_id, kind),
    FOREIGN KEY (mail_id) REFERENCES mail(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_following_update_thread
ON following_update (thread_id, occurred_at DESC, id DESC);

CREATE INDEX IF NOT EXISTS idx_following_update_kind
ON following_update (kind, occurred_at DESC, id DESC);
