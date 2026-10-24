//! Which queued task may start now. The caller holds the queue's advisory lock
//! and passes the counts read inside the same transaction.

pub const FREE_CONCURRENT_TASKS: i64 = 4;
pub const FREE_OPEN_TASKS_PER_USER: i64 = 1;
pub const PREMIUM_OPEN_TASKS_PER_USER: i64 = 5;

#[derive(Debug, PartialEq, Eq)]
pub enum Turn {
    Start,
    /// 1-based position shown to the user.
    Wait(i64),
}

pub struct Counts {
    pub is_premium: bool,
    /// The user's own running tasks, queued tasks created earlier, and running monitor deliveries.
    pub ahead_of_own: i64,
    pub running_free: i64,
    pub queued_free_before: i64,
}

pub fn next_turn(counts: &Counts) -> Turn {
    if counts.is_premium {
        if counts.ahead_of_own > 0 {
            return Turn::Wait(counts.ahead_of_own + 1);
        }
        return Turn::Start;
    }
    if counts.ahead_of_own > 0
        || counts.running_free + counts.queued_free_before >= FREE_CONCURRENT_TASKS
    {
        return Turn::Wait(counts.queued_free_before + 1);
    }
    Turn::Start
}

/// Open-task cap per user at the time a task is added.
pub fn open_task_limit(is_premium: bool) -> i64 {
    if is_premium {
        PREMIUM_OPEN_TASKS_PER_USER
    } else {
        FREE_OPEN_TASKS_PER_USER
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(is_premium: bool, ahead: i64, running: i64, before: i64) -> Counts {
        Counts {
            is_premium,
            ahead_of_own: ahead,
            running_free: running,
            queued_free_before: before,
        }
    }

    #[test]
    fn premium_runs_one_after_another_and_never_waits_for_others() {
        assert_eq!(next_turn(&counts(true, 0, 4, 10)), Turn::Start);
        assert_eq!(next_turn(&counts(true, 2, 0, 0)), Turn::Wait(3));
    }

    #[test]
    fn four_free_users_share_the_slots_in_arrival_order() {
        assert_eq!(next_turn(&counts(false, 0, 3, 0)), Turn::Start);
        assert_eq!(next_turn(&counts(false, 0, 4, 0)), Turn::Wait(1));
        assert_eq!(next_turn(&counts(false, 0, 2, 2)), Turn::Wait(3));
        assert_eq!(next_turn(&counts(false, 1, 0, 0)), Turn::Wait(1));
    }

    #[test]
    fn open_task_limits() {
        assert_eq!(open_task_limit(false), 1);
        assert_eq!(open_task_limit(true), 5);
    }
}
