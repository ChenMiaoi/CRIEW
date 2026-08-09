CREATE TABLE IF NOT EXISTS mail_trailer (
    mail_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    value TEXT NOT NULL,
    ord INTEGER NOT NULL,
    PRIMARY KEY (mail_id, ord),
    FOREIGN KEY (mail_id) REFERENCES mail(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_mail_trailer_kind
ON mail_trailer (kind);

CREATE INDEX IF NOT EXISTS idx_mail_trailer_mail
ON mail_trailer (mail_id, ord);
