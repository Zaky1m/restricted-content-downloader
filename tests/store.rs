mod common;

use std::path::Path;

use rcd::{
    i18n::Locales,
    ratelimit::Kind,
    store::{limits, settings, states, users},
};

fn locales() -> Locales {
    Locales::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("locales")).unwrap()
}

#[tokio::test]
async fn registration_and_referral_rewards() {
    let Some(t) = common::TempSchema::create().await else {
        return;
    };
    rcd::db::migrate(&t.pool).await.unwrap();
    let l = locales();
    let inviter = users::Profile {
        user_id: 1,
        first_name: "A",
        last_name: None,
        username: Some("a"),
    };
    let first = users::register(&t.pool, &l, &inviter, Some("ar"), None)
        .await
        .unwrap();
    assert!(first.is_new && first.inviter.is_none());
    assert_eq!(first.lang, "ar");

    let guest = users::Profile {
        user_id: 2,
        first_name: "B",
        last_name: Some("C"),
        username: None,
    };
    let joined = users::register(&t.pool, &l, &guest, Some("zz"), Some(1))
        .await
        .unwrap();
    let inv = joined.inviter.unwrap();
    assert_eq!((inv.id, inv.reward, inv.bonus), (1, 200, 50));
    assert_eq!(
        joined.lang, "en",
        "unknown Telegram language falls back to English"
    );

    let points: (i32, i32, Option<i64>) = sqlx::query_as(
        "SELECT (SELECT referral_points FROM users WHERE user_id = 1), (SELECT referral_points FROM users WHERE user_id = 2), \
         (SELECT referred_by FROM users WHERE user_id = 2)",
    )
    .fetch_one(&t.pool)
    .await
    .unwrap();
    assert_eq!(points, (200, 50, Some(1)));

    // Seeing the same user again changes nothing but the name.
    let again = users::register(&t.pool, &l, &guest, None, Some(1))
        .await
        .unwrap();
    assert!(!again.is_new && again.inviter.is_none());
    let count: i32 = sqlx::query_scalar("SELECT referral_count FROM users WHERE user_id = 1")
        .fetch_one(&t.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    // A premium inviter earns double; an unknown inviter and self-invites earn nothing.
    sqlx::query("UPDATE users SET is_premium = TRUE WHERE user_id = 1")
        .execute(&t.pool)
        .await
        .unwrap();
    let third = users::Profile {
        user_id: 3,
        first_name: "D",
        last_name: None,
        username: None,
    };
    let r = users::register(&t.pool, &l, &third, None, Some(1))
        .await
        .unwrap()
        .inviter
        .unwrap();
    assert_eq!((r.reward, r.bonus), (400, 200));
    let fourth = users::Profile {
        user_id: 4,
        first_name: "E",
        last_name: None,
        username: None,
    };
    assert!(
        users::register(&t.pool, &l, &fourth, None, Some(999))
            .await
            .unwrap()
            .inviter
            .is_none()
    );
    let fifth = users::Profile {
        user_id: 5,
        first_name: "F",
        last_name: None,
        username: None,
    };
    assert!(
        users::register(&t.pool, &l, &fifth, None, Some(5))
            .await
            .unwrap()
            .inviter
            .is_none()
    );
    assert_eq!(users::total(&t.pool).await.unwrap(), 5);
    users::set_lang(&t.pool, &l, 2, "fr").await.unwrap();
    assert_eq!(users::lang(&t.pool, &l, 2).await, "fr");
    t.finish().await;
}

#[tokio::test]
async fn states_expire_and_are_taken_once() {
    let Some(t) = common::TempSchema::create().await else {
        return;
    };
    rcd::db::migrate(&t.pool).await.unwrap();
    states::set(&t.pool, 7, "demo", &serde_json::json!({"n": 1}), 600)
        .await
        .unwrap();
    let got: serde_json::Value = states::get(&t.pool, 7, "demo").await.unwrap().unwrap();
    assert_eq!(got["n"], 1);
    assert!(
        states::take::<serde_json::Value>(&t.pool, 7, "demo")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        states::take::<serde_json::Value>(&t.pool, 7, "demo")
            .await
            .unwrap()
            .is_none()
    );
    states::set(&t.pool, 7, "old", &serde_json::json!({}), -5)
        .await
        .unwrap();
    assert!(
        states::get::<serde_json::Value>(&t.pool, 7, "old")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(states::purge_expired(&t.pool).await.unwrap(), 1);
    t.finish().await;
}

#[tokio::test]
async fn limits_and_settings() {
    let Some(t) = common::TempSchema::create().await else {
        return;
    };
    rcd::db::migrate(&t.pool).await.unwrap();
    let now = 1_000_000.0;
    assert!(limits::check(&t.pool, 9, Kind::Message, now).await.0);
    let (ok, notice) = limits::check(&t.pool, 9, Kind::Message, now + 0.1).await;
    assert!(!ok && notice.is_some());
    assert_eq!(
        limits::check(&t.pool, 9, Kind::Message, now + 0.5).await,
        (false, None)
    );
    assert!(limits::check(&t.pool, 9, Kind::Message, now + 5.0).await.0);

    assert!(settings::claim(&t.pool, "k", "v").await.unwrap());
    assert!(!settings::claim(&t.pool, "k", "w").await.unwrap());
    settings::set(&t.pool, "k", Some("z")).await.unwrap();
    assert_eq!(
        settings::get(&t.pool, "k").await.unwrap().as_deref(),
        Some("z")
    );
    t.finish().await;
}
