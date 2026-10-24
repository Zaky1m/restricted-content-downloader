mod common;

use std::collections::BTreeSet;

use sqlx::PgPool;

async fn columns(pool: &PgPool, schema: &str) -> BTreeSet<(String, String, String, String)> {
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text, data_type::text, is_nullable::text \
         FROM information_schema.columns WHERE table_schema = $1 AND table_name <> '_sqlx_migrations'",
    )
    .bind(schema)
    .fetch_all(pool)
    .await
    .unwrap();
    rows.into_iter().collect()
}

#[tokio::test]
async fn migration_matches_the_live_schema() {
    let Some(temp) = common::TempSchema::create().await else {
        eprintln!("TEST_DATABASE_URL not set, skipping");
        return;
    };
    rcd::db::migrate(&temp.pool).await.unwrap();
    let ours = columns(&temp.pool, temp.name()).await;
    let live = columns(&temp.pool, "public").await;
    // A fresh database (CI) has no live schema to compare against.
    if live.is_empty() {
        temp.finish().await;
        eprintln!("public schema is empty, nothing to compare");
        return;
    }
    let only_live: Vec<_> = live.difference(&ours).collect();
    // Tables added by later migrations do not exist in the live (Python) schema.
    const NEW_TABLES: [&str; 5] = [
        "account_sessions",
        "peer_cache",
        "bot_admins",
        "payments",
        "username_cache",
    ];
    let only_ours: Vec<_> = ours
        .difference(&live)
        .filter(|c| !NEW_TABLES.contains(&c.0.as_str()))
        .collect();
    temp.finish().await;
    assert!(
        only_live.is_empty(),
        "missing from migration: {only_live:?}"
    );
    assert!(only_ours.is_empty(), "not in live schema: {only_ours:?}");
}

#[tokio::test]
async fn migration_is_idempotent() {
    let Some(temp) = common::TempSchema::create().await else {
        return;
    };
    rcd::db::migrate(&temp.pool).await.unwrap();
    rcd::db::migrate(&temp.pool).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM bot_stats")
        .fetch_one(&temp.pool)
        .await
        .unwrap();
    temp.finish().await;
    assert_eq!(n, 7);
}
