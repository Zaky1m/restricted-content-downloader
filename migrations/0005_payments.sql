-- One row per Stars payment, keyed by Telegram's charge id, so a repeated delivery never extends premium twice.
CREATE TABLE payments (
    charge_id TEXT PRIMARY KEY,
    user_id BIGINT NOT NULL,
    plan TEXT NOT NULL,
    amount BIGINT NOT NULL,
    currency TEXT NOT NULL,
    payload TEXT NOT NULL,
    paid_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX payments_user_idx ON payments (user_id);
