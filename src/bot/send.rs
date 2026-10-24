use anyhow::Result;
use grammers_client::{
    message::{InputMessage, Message, ReplyMarkup},
    peer::Peer,
};

use super::Bot;

/// The person behind an update.
#[derive(Clone, Debug)]
pub struct Who {
    pub id: i64,
    pub first_name: String,
    pub last_name: Option<String>,
    pub username: Option<String>,
    pub lang_code: Option<String>,
}

impl Who {
    pub fn from_peer(peer: Option<&Peer>) -> Option<Self> {
        let Some(Peer::User(user)) = peer else {
            return None;
        };
        Some(Self {
            id: user.id().bare_id()?,
            first_name: user.first_name().unwrap_or("").to_string(),
            last_name: user.last_name().map(str::to_string),
            username: user.username().map(str::to_string),
            lang_code: user.lang_code().map(str::to_string),
        })
    }

    pub fn mention(&self) -> String {
        format!(
            "<a href=\"tg://user?id={}\">{}</a>",
            self.id,
            escape(&self.first_name)
        )
    }
}

pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn html(text: &str, markup: Option<ReplyMarkup>) -> InputMessage {
    let message = InputMessage::new().html(text).link_preview(false);
    match markup {
        Some(markup) => message.reply_markup(markup),
        None => message,
    }
}

impl Bot {
    pub async fn reply(
        &self,
        to: &Message,
        text: &str,
        markup: Option<ReplyMarkup>,
    ) -> Result<Message> {
        Ok(to.reply(html(text, markup)).await?)
    }

    /// Sends to a user by id; fails if the bot has never seen that user.
    pub async fn send_to(
        &self,
        user_id: i64,
        text: &str,
        markup: Option<ReplyMarkup>,
    ) -> Result<Message> {
        let peer = self
            .user_ref(user_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("user {user_id} is not known to the bot"))?;
        Ok(self.client.send_message(peer, html(text, markup)).await?)
    }

    /// Tells every owner; one unreachable owner does not stop the others.
    pub async fn notify_owners(&self, key: &str, args_for: impl Fn(&str) -> Vec<(String, String)>) {
        for &owner in &self.cfg.owner_ids {
            let lang = self.lang(owner).await;
            let owned = args_for(&lang);
            let args: Vec<(&str, &str)> = owned
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let text = self.t(&lang, key, &args);
            if let Err(error) = self.send_to(owner, &text, None).await {
                tracing::warn!(owner, %error, "could not notify an owner");
            }
        }
    }
}
