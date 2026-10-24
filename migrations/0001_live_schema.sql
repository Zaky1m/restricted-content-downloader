-- Adopts the schema the Python bot created, so the Rust binary runs on the existing database.
CREATE TABLE IF NOT EXISTS users (
    user_id BIGINT PRIMARY KEY,
    first_name TEXT,
    last_name TEXT,
    username TEXT,
    join_date TIMESTAMPTZ DEFAULT NOW(),
    last_activity_date TIMESTAMPTZ,
    language_code VARCHAR(20),
    daily_downloads_left INTEGER DEFAULT 25,
    daily_downloads_reset_at TIMESTAMPTZ,
    referral_points INTEGER DEFAULT 0,
    referred_by BIGINT,
    referral_count INTEGER DEFAULT 0,
    is_premium BOOLEAN DEFAULT FALSE,
    premium_expiry_date TIMESTAMPTZ,
    premium_added_by_admin BOOLEAN DEFAULT FALSE,
    is_active BOOLEAN DEFAULT TRUE,
    upload_dest_id BIGINT DEFAULT NULL,
    monitor_target_id BIGINT DEFAULT NULL,
    monitor_flags TEXT DEFAULT NULL,
    caption_mode VARCHAR(20) DEFAULT 'keep_all',
    thumbnail_path TEXT DEFAULT NULL,
    thumbnail_enabled BOOLEAN DEFAULT FALSE,
    force_download_enabled BOOLEAN DEFAULT FALSE
);

CREATE TABLE IF NOT EXISTS user_sessions (
    user_id BIGINT PRIMARY KEY REFERENCES users(user_id) ON DELETE CASCADE,
    phone_number VARCHAR(25) NOT NULL,
    session_string TEXT NOT NULL,
    login_timestamp TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS fsub_channels (
    channel_id BIGINT PRIMARY KEY,
    link TEXT NOT NULL,
    limit_val INTEGER DEFAULT 0,
    joined_count INTEGER DEFAULT 0,
    date_added TIMESTAMPTZ DEFAULT NOW(),
    initial_members INTEGER
);

CREATE TABLE IF NOT EXISTS downloads (
    download_id SERIAL PRIMARY KEY,
    user_id BIGINT REFERENCES users(user_id),
    message_id BIGINT,
    chat_id BIGINT,
    media_type VARCHAR(20),
    file_size BIGINT,
    download_timestamp TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS bot_stats (
    stat_key VARCHAR(50) PRIMARY KEY,
    stat_value BIGINT DEFAULT 0
);

CREATE TABLE IF NOT EXISTS gifts (
    gift_pid VARCHAR(30) PRIMARY KEY,
    value INTEGER NOT NULL,
    link_expiry TIMESTAMPTZ,
    gift_expiry_duration_seconds BIGINT,
    allowed_users JSONB,
    excluded_users JSONB,
    is_active BOOLEAN DEFAULT TRUE,
    usage_limit INTEGER,
    usage_count INTEGER DEFAULT 0,
    created_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS claimed_gifts (
    claim_id SERIAL PRIMARY KEY,
    user_id BIGINT REFERENCES users(user_id) ON DELETE CASCADE,
    gift_pid VARCHAR(30) REFERENCES gifts(gift_pid) ON DELETE CASCADE,
    claimed_at TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE (user_id, gift_pid)
);

CREATE TABLE IF NOT EXISTS user_gift_points (
    id SERIAL PRIMARY KEY,
    user_id BIGINT REFERENCES users(user_id) ON DELETE CASCADE,
    points INTEGER NOT NULL,
    expiry_date TIMESTAMPTZ,
    added_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS active_tasks (
    task_db_id SERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL,
    task_type VARCHAR(20) NOT NULL,
    source_ref TEXT NOT NULL,
    flags TEXT,
    start_id BIGINT NOT NULL,
    end_id BIGINT NOT NULL,
    last_done_id BIGINT DEFAULT 0,
    progress_msg_id BIGINT DEFAULT 0,
    failed_ids TEXT DEFAULT '[]',
    retry_phase BOOLEAN DEFAULT FALSE,
    attempt_count INTEGER DEFAULT 0,
    created_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS bot_settings (
    setting_key VARCHAR(64) PRIMARY KEY,
    setting_value TEXT,
    updated_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS user_states (
    user_id BIGINT NOT NULL,
    kind TEXT NOT NULL,
    data JSONB NOT NULL DEFAULT '{}'::jsonb,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (user_id, kind)
);
CREATE INDEX IF NOT EXISTS user_states_expires_idx ON user_states (expires_at);

CREATE TABLE IF NOT EXISTS pending_actions (
    action_id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL,
    kind TEXT NOT NULL,
    data JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS pending_actions_expires_idx ON pending_actions (expires_at);

CREATE TABLE IF NOT EXISTS task_queue (
    queue_id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL,
    kind TEXT NOT NULL,
    spec JSONB NOT NULL DEFAULT '{}'::jsonb,
    is_premium BOOLEAN NOT NULL DEFAULT FALSE,
    status TEXT NOT NULL DEFAULT 'queued',
    notice_message_id BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS task_queue_open_idx
    ON task_queue (status, is_premium, created_at) WHERE status IN ('queued', 'running');
CREATE INDEX IF NOT EXISTS task_queue_user_idx ON task_queue (user_id, status);

CREATE TABLE IF NOT EXISTS rate_limits (
    user_id BIGINT NOT NULL,
    kind TEXT NOT NULL,
    second_start DOUBLE PRECISION NOT NULL DEFAULT 0,
    second_count INTEGER NOT NULL DEFAULT 0,
    minute_start DOUBLE PRECISION NOT NULL DEFAULT 0,
    minute_count INTEGER NOT NULL DEFAULT 0,
    blocked_until DOUBLE PRECISION NOT NULL DEFAULT 0,
    strikes INTEGER NOT NULL DEFAULT 0,
    last_strike_at DOUBLE PRECISION NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, kind)
);

CREATE TABLE IF NOT EXISTS fsub_checks (
    user_id BIGINT PRIMARY KEY,
    passed_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE IF NOT EXISTS monitor_progress (
    user_id BIGINT PRIMARY KEY,
    target_id BIGINT NOT NULL,
    last_message_id BIGINT NOT NULL DEFAULT 0,
    last_checked_at TIMESTAMPTZ,
    fail_count INTEGER NOT NULL DEFAULT 0,
    target_title TEXT,
    target_username TEXT
);

CREATE TABLE IF NOT EXISTS monitor_jobs (
    job_id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    target_id BIGINT NOT NULL,
    message_id BIGINT NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued',
    attempts INTEGER NOT NULL DEFAULT 0,
    available_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (user_id, target_id, message_id)
);
CREATE INDEX IF NOT EXISTS monitor_jobs_waiting_idx ON monitor_jobs (available_at, job_id) WHERE status = 'queued';
CREATE INDEX IF NOT EXISTS monitor_jobs_user_open_idx ON monitor_jobs (user_id, status, job_id) WHERE status IN ('queued', 'running');

INSERT INTO bot_stats (stat_key, stat_value)
SELECT k, 0 FROM unnest(ARRAY['total_downloads','total_videos','total_photos','total_audio','total_files','total_text','total_other']) AS k
ON CONFLICT (stat_key) DO NOTHING;
