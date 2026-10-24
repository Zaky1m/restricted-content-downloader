//! Once a minute: drop expired waiting states and close linked accounts nobody is using.

use std::{sync::Arc, time::Duration};

use super::Bot;
use crate::store::states;

const INTERVAL: Duration = Duration::from_secs(60);

async fn users_with_open_tasks(bot: &Bot) -> std::collections::HashSet<i64> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        "SELECT DISTINCT user_id FROM task_queue WHERE status IN ('queued', 'running')",
    )
    .fetch_all(&bot.pg)
    .await
    .unwrap_or_default();
    rows.into_iter().map(|(id,)| id).collect()
}

pub async fn run(bot: Arc<Bot>) {
    loop {
        tokio::time::sleep(INTERVAL).await;
        if let Err(error) = states::purge_expired(&bot.pg).await {
            tracing::warn!(%error, "could not purge expired states");
        }
        if let Err(error) = states::prune_history(&bot.pg).await {
            tracing::warn!(%error, "could not prune old rows");
        }
        super::uploadch::leave_expired(&bot).await;
        bot.accounts.refresh_pool().await;
        let open = users_with_open_tasks(&bot).await;
        bot.accounts.close_idle(|user| open.contains(&user)).await;
    }
}
