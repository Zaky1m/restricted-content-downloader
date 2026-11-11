-- The "task resumed" notice is sent once per task, not again on every later restart.
ALTER TABLE active_tasks ADD COLUMN IF NOT EXISTS resume_noticed BOOLEAN NOT NULL DEFAULT FALSE;
