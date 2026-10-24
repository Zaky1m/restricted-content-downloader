mod common;

use rcd::{queue::Turn, store::queue};
use serde_json::json;

#[tokio::test]
async fn queue_limits_and_turns() {
    let Some(t) = common::TempSchema::create().await else {
        return;
    };
    rcd::db::migrate(&t.pool).await.unwrap();

    // A free user may have one open task; a premium user five.
    let first = queue::add(&t.pool, 1, "dl", &json!({}), false)
        .await
        .unwrap()
        .unwrap();
    assert!(
        queue::add(&t.pool, 1, "dl", &json!({}), false)
            .await
            .unwrap()
            .is_none()
    );
    for _ in 0..5 {
        assert!(
            queue::add(&t.pool, 2, "dl", &json!({}), true)
                .await
                .unwrap()
                .is_some()
        );
    }
    assert!(
        queue::add(&t.pool, 2, "dl", &json!({}), true)
            .await
            .unwrap()
            .is_none()
    );

    // The first free task starts; a premium user's second task waits for their first.
    assert_eq!(queue::try_start(&t.pool, first).await.unwrap(), Turn::Start);
    assert_eq!(
        queue::status(&t.pool, first).await.unwrap().as_deref(),
        Some("running")
    );
    assert_eq!(
        queue::try_start(&t.pool, first).await.unwrap(),
        Turn::Wait(0),
        "already running"
    );
    let tasks = queue::open_tasks(&t.pool, 2).await.unwrap();
    assert_eq!(
        queue::try_start(&t.pool, tasks[0].queue_id).await.unwrap(),
        Turn::Start
    );
    assert_eq!(
        queue::try_start(&t.pool, tasks[1].queue_id).await.unwrap(),
        Turn::Wait(2)
    );

    // Five free users fill the four slots; the fifth waits first in line.
    let mut ids = Vec::new();
    for user in 10..15 {
        ids.push(
            queue::add(&t.pool, user, "dl", &json!({}), false)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    // `first` already holds one free slot, so three more can start.
    for id in &ids[..3] {
        assert_eq!(queue::try_start(&t.pool, *id).await.unwrap(), Turn::Start);
    }
    assert_eq!(
        queue::try_start(&t.pool, ids[3]).await.unwrap(),
        Turn::Wait(1)
    );
    assert_eq!(
        queue::try_start(&t.pool, ids[4]).await.unwrap(),
        Turn::Wait(2)
    );

    // Finishing frees a slot; cancel marks rows and returns them.
    queue::finish(&t.pool, first, "done").await.unwrap();
    assert_eq!(
        queue::try_start(&t.pool, ids[3]).await.unwrap(),
        Turn::Start
    );
    let cancelled = queue::cancel(&t.pool, 2, None).await.unwrap();
    assert_eq!(cancelled.len(), 5);
    assert!(queue::open_tasks(&t.pool, 2).await.unwrap().is_empty());

    queue::set_spec(&t.pool, ids[4], &json!({"a": 1}))
        .await
        .unwrap();
    queue::set_spec(&t.pool, ids[4], &json!({"b": 2}))
        .await
        .unwrap();
    let spec = queue::spec(&t.pool, ids[4], 14).await.unwrap().unwrap();
    assert_eq!((spec["a"].as_i64(), spec["b"].as_i64()), (Some(1), Some(2)));
    t.finish().await;
}
