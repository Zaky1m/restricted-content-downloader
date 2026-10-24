//! One link: find the post with the right account and deliver it (an album arrives whole).

use std::sync::Arc;

use grammers_client::{session::types::PeerKind, tl};

use super::{
    Outcome, TaskCtx,
    access::{self, AccessError},
    download::{self, Job},
};
use crate::{links::parse_message_link, telegram::accounts::Account};

async fn account_for(ctx: &TaskCtx) -> Option<Arc<Account>> {
    match ctx.bot.accounts.for_download(ctx.user_id).await {
        Ok(account) => Some(account),
        Err(error) => {
            tracing::error!(user = ctx.user_id, %error, "no download account");
            None
        }
    }
}

pub async fn download_link(ctx: &Arc<TaskCtx>, link: &str, force: bool) -> Outcome {
    let bot = &ctx.bot;
    let parsed = match parse_message_link(link) {
        Ok(parsed) => parsed,
        Err(_) => {
            ctx.report(&bot.t(&ctx.lang, "downloads.invalid_link", &[]), None)
                .await;
            return Outcome::Failed;
        }
    };
    let Some(account) = account_for(ctx).await else {
        ctx.report(&bot.t(&ctx.lang, "downloads.client_unavailable", &[]), None)
            .await;
        return Outcome::Failed;
    };
    let job = Job {
        ctx,
        account: account.clone(),
        force_download: force,
        is_batch: false,
        wait_for_choice: true,
        policy: Default::default(),
    };
    let client = account.client().await;

    let mut peer = match access::resolve_chat(&account, &parsed.chat).await {
        Ok(peer) => peer,
        Err(AccessError::Private) => return not_accessible(ctx, &account, link).await,
        Err(AccessError::NotFound) => {
            ctx.report(&bot.t(&ctx.lang, "downloads.post_not_found", &[]), None)
                .await;
            return Outcome::Failed;
        }
        Err(AccessError::Rpc(error)) => {
            tracing::error!(user = ctx.user_id, %error, "could not resolve the source");
            return Outcome::Failed;
        }
    };
    let mut message_id = parsed.message_id;
    if let Some(comment) = parsed.comment_id {
        match access::resolve_comment(&client, peer, message_id, comment).await {
            Ok((discussion, id)) => {
                peer = discussion;
                message_id = id;
            }
            Err(error) => {
                tracing::warn!(user = ctx.user_id, %error, "comment could not be resolved");
                ctx.report(
                    &bot.t(&ctx.lang, "downloads.comment_unavailable", &[]),
                    None,
                )
                .await;
                return Outcome::Failed;
            }
        }
    }

    let mut fetched = None;
    for attempt in 0..4u64 {
        match access::get_message(&client, peer, message_id).await {
            Ok(found) => {
                fetched = Some(found);
                break;
            }
            Err(error) if access::is_private_access_error(&error) => {
                return not_accessible(ctx, &account, link).await;
            }
            Err(error) => {
                tracing::warn!(user = ctx.user_id, attempt, %error, "could not fetch the post");
                tokio::time::sleep(std::time::Duration::from_secs(2 + attempt)).await;
            }
        }
    }
    let Some(found) = fetched else {
        return Outcome::Failed;
    };
    // A deleted or never-existing post comes back as an empty message, like Pyrogram's `empty`.
    let Some(message) = found.filter(|m| !matches!(m.raw, tl::enums::Message::Empty(_))) else {
        ctx.report(&bot.t(&ctx.lang, "downloads.post_not_found", &[]), None)
            .await;
        return Outcome::Failed;
    };
    // A service message (a renamed channel, a pinned post) has nothing in it to deliver.
    if download::raw_message(&message).is_none() {
        ctx.report(
            &bot.t(&ctx.lang, "downloads.nothing_to_download", &[]),
            None,
        )
        .await;
        return Outcome::Skipped;
    }

    // An album arrives as a whole, in order.
    if message.grouped_id().is_some() && peer.id.kind() != PeerKind::Chat {
        return album(ctx, &job, &client, peer, &message).await;
    }
    match job.deliver(&message).await {
        Ok(delivered) => download::outcome(&delivered),
        Err(error) => {
            tracing::error!(user = ctx.user_id, %error, "delivery failed");
            ctx.report(
                &bot.t(
                    &ctx.lang,
                    "common.unexpected_error",
                    &[("error", &crate::bot::escape(&error.to_string()))],
                ),
                None,
            )
            .await;
            Outcome::Failed
        }
    }
}

async fn album(
    ctx: &Arc<TaskCtx>,
    job: &Job<'_>,
    client: &grammers_client::Client,
    peer: grammers_client::session::types::PeerRef,
    message: &grammers_client::message::Message,
) -> Outcome {
    let bot = &ctx.bot;
    let group = match access::media_group(client, peer, message).await {
        Ok(group) => group,
        Err(error) => {
            tracing::warn!(user = ctx.user_id, %error, "could not load the album");
            return Outcome::Failed;
        }
    };
    ctx.show(&bot.t(
        &ctx.lang,
        "downloads.album.preparing",
        &[("count", &group.len().to_string())],
    ))
    .await;
    let mut done = 0usize;
    let mut last = None;
    let mut mode = None;
    let total = group.len();
    for (index, item) in group.iter().enumerate() {
        ctx.show(&bot.t(
            &ctx.lang,
            "downloads.album.processing",
            &[
                ("current", &(index + 1).to_string()),
                ("total", &total.to_string()),
            ],
        ))
        .await;
        if let Ok(result) = (Job {
            ctx,
            account: job.account.clone(),
            force_download: job.force_download,
            is_batch: true,
            wait_for_choice: true,
            policy: Default::default(),
        })
        .deliver(item)
        .await
            && result.ok
        {
            done += 1;
            mode = result.mode.or(mode);
            last = result.last.or(last);
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    match (done, mode) {
        (0, _) => Outcome::Failed,
        (count, Some(mode)) => {
            let text = download::completion_text(ctx, "batch", mode, count, last.as_ref()).await;
            ctx.report(&text, None).await;
            Outcome::Downloaded
        }
        _ => Outcome::Downloaded,
    }
}

async fn not_accessible(ctx: &Arc<TaskCtx>, account: &Account, link: &str) -> Outcome {
    if account.is_linked() {
        super::notices::not_member(ctx, Some(link)).await;
        Outcome::NotMember
    } else {
        super::notices::login_required(ctx).await;
        Outcome::LoginRequired
    }
}
