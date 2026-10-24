-- Owners come from OWNER_IDS in .env. Admins are added and removed by owners from /admin.
CREATE TABLE bot_admins (
    user_id BIGINT PRIMARY KEY,
    added_by BIGINT NOT NULL,
    added_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
