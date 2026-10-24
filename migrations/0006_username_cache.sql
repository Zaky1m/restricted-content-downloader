-- Which chat a public username named, per session. Pyrogram kept the same for eight hours; resolving a
-- username every time makes Telegram answer FLOOD_WAIT for minutes.
CREATE TABLE username_cache (
    session_key TEXT NOT NULL,
    username TEXT NOT NULL,
    dialog_id BIGINT NOT NULL,
    resolved_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (session_key, username)
);
