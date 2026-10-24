-- Encrypted Telegram sessions: the pool accounts, the bot itself and every user's linked account.
CREATE TABLE account_sessions (
    session_id BIGSERIAL PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('bot', 'pool', 'linked')),
    label TEXT NOT NULL,
    owner_id BIGINT,
    phone TEXT,
    telegram_user_id BIGINT NOT NULL,
    nonce BYTEA NOT NULL,
    ciphertext BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (kind, label)
);
CREATE INDEX account_sessions_owner_idx ON account_sessions (owner_id) WHERE owner_id IS NOT NULL;
