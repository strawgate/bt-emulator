-- Attachment uploads (POST /attachment). `data` is the base64 content once uploaded;
-- `token` authorizes the signed upload/download URL; `status` is the JSON upload status.
CREATE TABLE attachments (
    key TEXT PRIMARY KEY,
    filename TEXT NOT NULL,
    content_type TEXT NOT NULL,
    token TEXT NOT NULL,
    status TEXT NOT NULL,
    data TEXT
);
