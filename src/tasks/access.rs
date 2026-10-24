//! Turning a link into something a user account can read from: a peer and a message.

use grammers_client::{
    Client, InvocationError,
    message::Message,
    session::types::{PeerAuth, PeerId, PeerRef},
    tl,
};

use crate::{
    links::{Chat, MessageLink},
    telegram::accounts::Account,
};

#[derive(Debug)]
pub enum AccessError {
    /// The account cannot see the chat: private or not a member.
    Private,
    /// No chat has this username.
    NotFound,
    Rpc(InvocationError),
}

/// Same words Python checked in the error text to decide a source is private.
pub fn is_private_access_error(error: &InvocationError) -> bool {
    let text = error.to_string().to_lowercase();
    [
        "channel_private",
        "channel private",
        "channel_invalid",
        "peer_id_invalid",
        "chat_id_invalid",
        "user_not_participant",
        "not participant",
        "not a member",
        "id not found",
        "access_hash_invalid",
        "forbidden",
        "private",
    ]
    .iter()
    .any(|p| text.contains(p))
}

/// The account's reference for a chat. A user account cannot use a channel without its access hash, and a
/// freshly linked account has none cached yet; like Pyrogram, its chat list is read once to learn them (grammers
/// stores every peer it sees, so this happens only the first time).
pub async fn peer_for(account: &Account, id: PeerId) -> PeerRef {
    if let Some(found) = account.session.peer_ref_of(id).await {
        return found;
    }
    let fallback = PeerRef {
        id,
        auth: PeerAuth::default(),
    };
    if id.kind() != grammers_client::session::types::PeerKind::Channel {
        return fallback;
    }
    let client = account.client().await;
    let mut dialogs = client.iter_dialogs();
    loop {
        match dialogs.next().await {
            Ok(Some(dialog)) if dialog.peer_id() == id => return dialog.peer_ref(),
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(account = %account.label, %error, "chat list could not be read");
                break;
            }
        }
    }
    account.session.peer_ref_of(id).await.unwrap_or(fallback)
}

pub async fn resolve_chat(account: &Account, chat: &Chat) -> Result<PeerRef, AccessError> {
    match chat {
        Chat::Username(name) => {
            if let Some(peer_ref) = account.session.username(name).await {
                return Ok(peer_ref);
            }
            let client = account.client().await;
            match client.resolve_username(name).await {
                Ok(Some(peer)) => match peer.to_ref().await {
                    Ok(Some(peer_ref)) => {
                        account.session.remember_username(name, peer_ref.id).await;
                        Ok(peer_ref)
                    }
                    _ => Err(AccessError::Private),
                },
                Ok(None) => Err(AccessError::NotFound),
                Err(error) if is_private_access_error(&error) => Err(AccessError::Private),
                Err(error) => Err(AccessError::Rpc(error)),
            }
        }
        Chat::Id(dialog_id) => {
            let Some(id) = PeerId::from_bot_api_dialog_id(*dialog_id) else {
                return Err(AccessError::Private);
            };
            Ok(peer_for(account, id).await)
        }
        Chat::Invite(_) => Err(AccessError::Private),
    }
}

pub async fn get_message(
    client: &Client,
    peer: PeerRef,
    message_id: i64,
) -> Result<Option<Message>, InvocationError> {
    let id = i32::try_from(message_id).unwrap_or(0);
    Ok(client
        .get_messages_by_id(peer, &[id])
        .await?
        .into_iter()
        .next()
        .flatten())
}

/// The discussion-group copy of a channel post and the comment inside it.
pub async fn resolve_comment(
    client: &Client,
    peer: PeerRef,
    message_id: i64,
    comment_id: i64,
) -> Result<(PeerRef, i64), String> {
    let request = tl::functions::messages::GetDiscussionMessage {
        peer: peer.into(),
        msg_id: message_id as i32,
    };
    let tl::enums::messages::DiscussionMessage::Message(discussion) =
        client.invoke(&request).await.map_err(|e| e.to_string())?;
    for chat in &discussion.chats {
        let (id, hash) = match chat {
            tl::enums::Chat::Channel(c) => (PeerId::channel(c.id), c.access_hash.unwrap_or(0)),
            tl::enums::Chat::Chat(c) => (PeerId::chat(c.id), 0),
            _ => continue,
        };
        if let Some(id) = id {
            return Ok((
                PeerRef {
                    id,
                    auth: PeerAuth::from_hash(hash),
                },
                comment_id,
            ));
        }
    }
    Err("No discussion group was found".to_string())
}

/// The messages of the album a message belongs to, in order.
pub async fn media_group(
    client: &Client,
    peer: PeerRef,
    message: &Message,
) -> Result<Vec<Message>, InvocationError> {
    let Some(group) = message.grouped_id() else {
        return Ok(vec![message.clone()]);
    };
    let center = message.id();
    let ids: Vec<i32> = (center.saturating_sub(9).max(1)..=center + 9).collect();
    let mut found: Vec<Message> = client
        .get_messages_by_id(peer, &ids)
        .await?
        .into_iter()
        .flatten()
        .filter(|m| m.grouped_id() == Some(group))
        .collect();
    found.sort_by_key(|m| m.id());
    Ok(found)
}

pub fn link_chat(link: &MessageLink) -> &Chat {
    &link.chat
}

fn ids_of(messages: tl::enums::messages::Messages) -> Vec<i32> {
    use tl::enums::messages::Messages as M;
    let list = match messages {
        M::Messages(m) => m.messages,
        M::Slice(m) => m.messages,
        M::ChannelMessages(m) => m.messages,
        M::NotModified(_) => Vec::new(),
    };
    list.iter()
        .filter_map(|m| match m {
            tl::enums::Message::Message(m) => Some(m.id),
            tl::enums::Message::Service(m) => Some(m.id),
            tl::enums::Message::Empty(_) => None,
        })
        .collect()
}

/// Id of the newest post (offset 0) or of the oldest one (`oldest`), if the chat has any.
pub async fn edge_id(
    client: &Client,
    peer: PeerRef,
    oldest: bool,
) -> Result<Option<i64>, InvocationError> {
    let request = tl::functions::messages::GetHistory {
        peer: peer.into(),
        offset_id: if oldest { 1 } else { 0 },
        offset_date: 0,
        add_offset: if oldest { -1 } else { 0 },
        limit: 1,
        max_id: 0,
        min_id: 0,
        hash: 0,
    };
    let ids = ids_of(client.invoke(&request).await?);
    Ok(ids.first().map(|id| i64::from(*id)))
}

/// A hundred ids per request instead of one: the same messages with far fewer calls.
pub async fn get_many(
    client: &Client,
    peer: PeerRef,
    ids: &[i32],
) -> Result<Vec<Option<Message>>, InvocationError> {
    client.get_messages_by_id(peer, ids).await
}

pub struct Dead;

impl Dead {
    /// The account's session is gone (revoked, deactivated or duplicated).
    pub fn is_dead(error: &InvocationError) -> bool {
        crate::telegram::accounts::Accounts::is_dead(error)
    }
}
