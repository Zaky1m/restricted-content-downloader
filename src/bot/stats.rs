use std::{sync::Arc, time::Duration};

use anyhow::Result;
use grammers_client::update::Message;
use sysinfo::{Disks, Networks, System};

use super::{Bot, Who};

async fn counter(bot: &Bot, key: &str) -> i64 {
    let row: Result<Option<(Option<i64>,)>, _> =
        sqlx::query_as("SELECT stat_value FROM bot_stats WHERE stat_key = $1")
            .bind(key)
            .fetch_optional(&bot.pg)
            .await;
    row.ok().flatten().and_then(|r| r.0).unwrap_or(0)
}

async fn count(bot: &Bot, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(&bot.pg)
        .await
        .unwrap_or(0)
}

/// CPU needs two samples a moment apart; disk and network come from the OS counters.
async fn system_text(bot: &Bot, lang: &str) -> String {
    let mut system = System::new();
    system.refresh_cpu_usage();
    system.refresh_memory();
    tokio::time::sleep(Duration::from_millis(500)).await;
    system.refresh_cpu_usage();
    let cpu = system.global_cpu_usage();
    let ram = if system.total_memory() > 0 {
        system.used_memory() as f64 * 100.0 / system.total_memory() as f64
    } else {
        0.0
    };

    let disks = Disks::new_with_refreshed_list();
    let here = std::fs::canonicalize(&bot.cfg.data_dir)
        .or_else(|_| std::env::current_dir())
        .unwrap_or_default();
    let disk = disks
        .list()
        .iter()
        .filter(|d| here.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len());
    let (total, used) = disk.map_or((0, 0), |d| {
        (d.total_space(), d.total_space() - d.available_space())
    });
    let percent = if total > 0 {
        used as f64 * 100.0 / total as f64
    } else {
        0.0
    };

    let networks = Networks::new_with_refreshed_list();
    let sent: u64 = networks.values().map(|n| n.total_transmitted()).sum();
    let received: u64 = networks.values().map(|n| n.total_received()).sum();

    bot.t(
        lang,
        "stats.system",
        &[
            (
                "uptime",
                &bot.duration(lang, bot.started.elapsed().as_secs() as i64),
            ),
            ("disk_usage", &bot.size(lang, used)),
            ("disk_total", &bot.size(lang, total)),
            ("disk_percent", &format!("{percent:.1}")),
            ("ram_percent", &format!("{ram:.1}")),
            ("cpu_percent", &format!("{cpu:.1}")),
            ("sent", &bot.size(lang, sent)),
            ("received", &bot.size(lang, received)),
        ],
    )
}

pub async fn stats(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<()> {
    let lang = bot.lang(who.id).await;
    if !bot.is_staff(who.id).await {
        let text = bot.t(
            &lang,
            "stats.public",
            &[
                (
                    "uptime",
                    &bot.duration(&lang, bot.started.elapsed().as_secs() as i64),
                ),
                (
                    "total_downloads",
                    &counter(bot, "total_downloads").await.to_string(),
                ),
            ],
        );
        bot.reply(message, &text, None).await?;
        return Ok(());
    }
    let mut text = system_text(bot, &lang).await;
    let total_users = count(bot, "SELECT COUNT(*) FROM users").await;
    let total_premium = count(bot, "SELECT COUNT(*) FROM users WHERE is_premium = TRUE").await;
    let by_admin = count(
        bot,
        "SELECT COUNT(*) FROM users WHERE is_premium = TRUE AND premium_added_by_admin = TRUE",
    )
    .await;
    text.push_str("\n\n");
    text.push_str(&bot.t(
        &lang,
        "stats.admin",
        &[
            ("total_users", &total_users.to_string()),
            ("total_premium", &total_premium.to_string()),
            ("premium_by_admin", &by_admin.to_string()),
            ("premium_by_stars", &(total_premium - by_admin).to_string()),
        ],
    ));
    bot.reply(message, &text, None).await?;
    Ok(())
}
