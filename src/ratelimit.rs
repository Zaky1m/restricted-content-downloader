//! Per-user message and button limits. Pure state transitions; the caller
//! stores the state row and passes the current time in seconds.

pub const MESSAGES_PER_SECOND: u32 = 1;
pub const MESSAGES_PER_MINUTE: u32 = 20;
pub const BUTTONS_PER_MINUTE: u32 = 20;
pub const MAX_BLOCK_SECONDS: f64 = 24.0 * 60.0 * 60.0;
pub const STRIKE_MEMORY_SECONDS: f64 = 60.0 * 60.0;
const MAX_STRIKES: u32 = 17;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Kind {
    Message,
    Button,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Notice {
    PerSecond,
    PerMinute,
    Buttons,
}

impl Notice {
    pub fn locale_key(self) -> &'static str {
        match self {
            Self::PerSecond => "spam.per_second",
            Self::PerMinute => "spam.per_minute",
            Self::Buttons => "spam.buttons",
        }
    }
}

#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub struct State {
    pub second_start: f64,
    pub second_count: u32,
    pub minute_start: f64,
    pub minute_count: u32,
    pub blocked_until: f64,
    pub strikes: u32,
    pub last_strike_at: f64,
}

/// Counts one message or button press. The notice is returned only on the hit
/// that starts a block, so the user is told once and then ignored. Button
/// blocks double each time the limit is broken again within an hour.
pub fn check(state: &mut State, kind: Kind, now: f64) -> (bool, Option<Notice>) {
    if now < state.blocked_until {
        return (false, None);
    }
    if now - state.second_start >= 1.0 {
        state.second_start = now;
        state.second_count = 0;
    }
    if now - state.minute_start >= 60.0 {
        state.minute_start = now;
        state.minute_count = 0;
    }
    state.second_count += 1;
    state.minute_count += 1;

    let minute_limit = match kind {
        Kind::Message => MESSAGES_PER_MINUTE,
        Kind::Button => BUTTONS_PER_MINUTE,
    };
    if kind == Kind::Message && state.second_count > MESSAGES_PER_SECOND {
        state.blocked_until = state.second_start + 1.0;
        return (false, Some(Notice::PerSecond));
    }
    if state.minute_count > minute_limit {
        let rest_of_minute = (state.minute_start + 60.0 - now).max(1.0);
        let (block, notice) = match kind {
            Kind::Button => {
                state.strikes = if now - state.last_strike_at < STRIKE_MEMORY_SECONDS {
                    (state.strikes + 1).min(MAX_STRIKES)
                } else {
                    1
                };
                state.last_strike_at = now;
                let doubled = rest_of_minute * 2f64.powi(state.strikes as i32 - 1);
                (doubled.min(MAX_BLOCK_SECONDS), Notice::Buttons)
            }
            Kind::Message => (rest_of_minute, Notice::PerMinute),
        };
        state.blocked_until = now + block;
        return (false, Some(notice));
    }
    (true, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn second_burst_is_blocked_for_the_rest_of_the_second() {
        let mut s = State::default();
        assert_eq!(check(&mut s, Kind::Message, 1000.0), (true, None));
        assert_eq!(
            check(&mut s, Kind::Message, 1000.1),
            (false, Some(Notice::PerSecond))
        );
        assert_eq!(check(&mut s, Kind::Message, 1000.2), (false, None));
        assert!(close(s.blocked_until, 1001.0));
        assert_eq!(s.second_count, 2);
    }

    #[test]
    fn twenty_one_messages_in_a_minute_block_until_the_minute_ends() {
        let mut s = State::default();
        let results: Vec<_> = (0..22)
            .map(|i| check(&mut s, Kind::Message, 1000.0 + i as f64))
            .collect();
        assert_eq!(results[19], (true, None));
        assert_eq!(results[20], (false, Some(Notice::PerMinute)));
        assert_eq!(results[21], (false, None));
        assert!(close(s.blocked_until, 1060.0));
        assert_eq!(check(&mut s, Kind::Message, 1060.001), (true, None));
    }

    #[test]
    fn button_blocks_double_on_repeat_within_an_hour() {
        let mut s = State::default();
        let results: Vec<_> = (0..22)
            .map(|i| check(&mut s, Kind::Button, 2000.0 + i as f64 * 0.5))
            .collect();
        assert_eq!(results[19], (true, None));
        assert_eq!(results[20], (false, Some(Notice::Buttons)));
        assert_eq!(results[21], (false, None));
        assert_eq!((s.strikes, s.minute_count), (1, 21));
        assert!(close(s.blocked_until, 2060.0));

        let t = s.blocked_until + 0.01;
        let last = (0..22)
            .map(|i| check(&mut s, Kind::Button, t + i as f64 * 0.1))
            .last();
        assert_eq!(last, Some((false, None)));
        assert_eq!(s.strikes, 2);
        assert!(close(s.blocked_until, 2178.01));
    }

    #[test]
    fn strikes_are_forgotten_after_an_hour() {
        let mut s = State::default();
        for i in 0..21 {
            check(&mut s, Kind::Button, 3000.0 + i as f64 * 0.1);
        }
        assert_eq!((s.strikes, s.blocked_until), (1, 3060.0));
        let t = s.blocked_until + 3601.0;
        let mut last = (true, None);
        for i in 0..21 {
            last = check(&mut s, Kind::Button, t + i as f64 * 0.1);
        }
        assert_eq!(last, (false, Some(Notice::Buttons)));
        assert_eq!(s.strikes, 1);
        assert!(close(s.blocked_until - (t + 2.0), 58.0));
    }

    #[test]
    fn block_never_exceeds_a_day() {
        let mut s = State {
            strikes: 17,
            last_strike_at: 100.0,
            minute_start: 100.0,
            minute_count: 20,
            second_start: 100.0,
            ..State::default()
        };
        assert_eq!(
            check(&mut s, Kind::Button, 101.0),
            (false, Some(Notice::Buttons))
        );
        assert!(close(s.blocked_until, 101.0 + MAX_BLOCK_SECONDS));
    }
}
