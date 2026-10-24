-- Access hashes each session has seen; Telegram refuses to address a user or channel without them.
CREATE TABLE peer_cache (
    session_key TEXT NOT NULL,
    dialog_id BIGINT NOT NULL,
    access_hash BIGINT NOT NULL,
    PRIMARY KEY (session_key, dialog_id)
);
