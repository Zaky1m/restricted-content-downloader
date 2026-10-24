use sqlx::PgPool;

pub async fn get(pg: &PgPool, key: &str) -> sqlx::Result<Option<String>> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT setting_value FROM bot_settings WHERE setting_key = $1")
            .bind(key)
            .fetch_optional(pg)
            .await?;
    Ok(row.and_then(|(v,)| v))
}

pub async fn set(pg: &PgPool, key: &str, value: Option<&str>) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO bot_settings (setting_key, setting_value) VALUES ($1, $2) \
         ON CONFLICT (setting_key) DO UPDATE SET setting_value = EXCLUDED.setting_value, updated_at = NOW()",
    )
    .bind(key)
    .bind(value)
    .execute(pg)
    .await?;
    Ok(())
}

/// Stores the key only if it is new; true means this caller stored it.
pub async fn claim(pg: &PgPool, key: &str, value: &str) -> sqlx::Result<bool> {
    let row: Option<(String,)> = sqlx::query_as(
        "INSERT INTO bot_settings (setting_key, setting_value) VALUES ($1, $2) \
         ON CONFLICT (setting_key) DO NOTHING RETURNING setting_key",
    )
    .bind(key)
    .bind(value)
    .fetch_optional(pg)
    .await?;
    Ok(row.is_some())
}
