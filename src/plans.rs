//! Premium plans, durations, and how download points are spent.

pub const DAILY_DOWNLOAD_LIMIT: i64 = 25;
pub const REFERRAL_REWARD: i64 = 200;
pub const NEW_USER_REFERRAL_BONUS: i64 = 50;
pub const PREMIUM_REFERRAL_REWARD: i64 = 400;
pub const NEW_USER_PREMIUM_REFERRAL_BONUS: i64 = 200;

const DAY: i64 = 86_400;

/// Admin duration argument in seconds: `30d`, or `DDHHMM` (two digits each).
pub fn parse_duration(value: &str) -> Option<i64> {
    if let Some(days) = value.strip_suffix('d') {
        let days: i64 = days.parse().ok()?;
        return (1..=36_500).contains(&days).then_some(days * DAY);
    }
    if value.len() != 6 || !value.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (days, hours, minutes): (i64, i64, i64) = (
        value[0..2].parse().ok()?,
        value[2..4].parse().ok()?,
        value[4..6].parse().ok()?,
    );
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(days * DAY + hours * 3_600 + minutes * 60)
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Plan {
    pub name: &'static str,
    pub days: i64,
    pub stars: i64,
}

pub const PLANS: [Plan; 2] = [
    Plan {
        name: "monthly",
        days: 30,
        stars: 200,
    },
    Plan {
        name: "yearly",
        days: 365,
        stars: 2000,
    },
];

/// Plan named in the invoice payload (`premium_<plan>_...`), or the plan with
/// the same price when the payload names none.
pub fn plan_from_payment(payload: &str, amount: i64) -> Option<Plan> {
    PLANS
        .iter()
        .find(|p| payload.contains(&format!("premium_{}_", p.name)))
        .or_else(|| PLANS.iter().find(|p| p.stars == amount))
        .copied()
}

/// New expiry: days are added to the time left, or to now when expired.
pub fn extend_premium(current_expiry: Option<i64>, now: i64, seconds: i64) -> i64 {
    let start = current_expiry.filter(|expiry| *expiry > now).unwrap_or(now);
    start + seconds
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct GiftBucket {
    pub id: i64,
    pub points: i64,
    pub expires_at: Option<i64>,
}

#[derive(Debug, PartialEq, Eq, Default)]
pub struct Wallet {
    pub daily: i64,
    pub referral: i64,
    pub gifts: Vec<GiftBucket>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Spent {
    pub daily: i64,
    pub gifts: Vec<(i64, i64)>,
    pub referral: i64,
}

/// Order: daily points, then gift buckets (soonest expiry first, no expiry
/// last, then id), then referral points. `None` when the total is short.
pub fn spend_points(wallet: &Wallet, cost: i64, now: i64) -> Option<Spent> {
    let mut remaining = cost;
    let daily = remaining.min(wallet.daily.max(0));
    remaining -= daily;

    let mut buckets: Vec<&GiftBucket> = wallet
        .gifts
        .iter()
        .filter(|g| g.points > 0 && g.expires_at.is_none_or(|at| at > now))
        .collect();
    buckets.sort_by_key(|g| (g.expires_at.is_none(), g.expires_at, g.id));
    let mut gifts = Vec::new();
    for bucket in buckets {
        if remaining <= 0 {
            break;
        }
        let take = remaining.min(bucket.points);
        gifts.push((bucket.id, take));
        remaining -= take;
    }

    let referral = remaining.min(wallet.referral.max(0));
    remaining -= referral;
    (remaining <= 0).then_some(Spent {
        daily,
        gifts,
        referral,
    })
}

/// `(reward for the inviter, bonus for the new user)`; premium inviters earn double.
pub fn referral_amounts(inviter_is_premium: bool) -> (i64, i64) {
    if inviter_is_premium {
        (PREMIUM_REFERRAL_REWARD, NEW_USER_PREMIUM_REFERRAL_BONUS)
    } else {
        (REFERRAL_REWARD, NEW_USER_REFERRAL_BONUS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_durations_match_the_python_bot() {
        let cases: [(&str, Option<i64>); 12] = [
            ("30d", Some(30 * DAY)),
            ("0d", None),
            ("36500d", Some(36_500 * DAY)),
            ("36501d", None),
            ("-5d", None),
            ("abd", None),
            ("010203", Some(DAY + 2 * 3_600 + 3 * 60)),
            ("000000", Some(0)),
            ("992359", Some(99 * DAY + 23 * 3_600 + 59 * 60)),
            ("012400", None),
            ("010260", None),
            ("12345", None),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_duration(input), expected, "{input:?}");
        }
        assert_eq!(parse_duration("1234567"), None);
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("d"), None);
    }

    #[test]
    fn payment_plan_comes_from_the_payload_then_the_amount() {
        assert_eq!(
            plan_from_payment("premium_yearly_42", 200).map(|p| p.name),
            Some("yearly")
        );
        assert_eq!(
            plan_from_payment("premium_monthly_42", 2000).map(|p| p.name),
            Some("monthly")
        );
        assert_eq!(plan_from_payment("", 2000).map(|p| p.name), Some("yearly"));
        assert_eq!(plan_from_payment("other", 200).map(|p| p.days), Some(30));
        assert_eq!(plan_from_payment("other", 5), None);
    }

    #[test]
    fn premium_days_stack_on_the_time_left() {
        assert_eq!(extend_premium(Some(1_000), 500, 100), 1_100);
        assert_eq!(extend_premium(Some(400), 500, 100), 600);
        assert_eq!(extend_premium(Some(500), 500, 100), 600);
        assert_eq!(extend_premium(None, 500, 100), 600);
    }

    #[test]
    fn points_are_spent_daily_then_gifts_then_referral() {
        let wallet = Wallet {
            daily: 2,
            referral: 10,
            gifts: vec![
                GiftBucket {
                    id: 3,
                    points: 5,
                    expires_at: None,
                },
                GiftBucket {
                    id: 1,
                    points: 5,
                    expires_at: Some(2_000),
                },
                GiftBucket {
                    id: 2,
                    points: 5,
                    expires_at: Some(1_000),
                },
                GiftBucket {
                    id: 4,
                    points: 5,
                    expires_at: Some(50),
                },
            ],
        };
        let spent = spend_points(&wallet, 9, 100).expect("enough points");
        assert_eq!(spent.daily, 2);
        assert_eq!(spent.gifts, vec![(2, 5), (1, 2)]);
        assert_eq!(spent.referral, 0);

        let spent = spend_points(&wallet, 22, 100).expect("enough points");
        assert_eq!(spent.gifts, vec![(2, 5), (1, 5), (3, 5)]);
        assert_eq!(spent.referral, 5);

        assert_eq!(spend_points(&wallet, 28, 100), None);
        assert_eq!(spend_points(&wallet, 27, 100).map(|s| s.referral), Some(10));
    }
}
