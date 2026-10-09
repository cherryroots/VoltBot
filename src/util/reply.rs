//! A bot reply that may span several Discord messages and changes over time, like a chat
//! answer streaming in.
//!
//! [`LiveReply::show`] takes the full list of parts each time (from
//! [`split_message`](super::split::split_message)) and makes Discord match it: it edits the
//! parts whose text changed, sends the new ones, and deletes the ones that are no longer
//! needed. Parts that didn't change cost no API call.

use serenity::all::{
    ChannelId, CreateAllowedMentions, CreateMessage, EditMessage, Http, MessageFlags, MessageId,
};

use super::split::{DISCORD_LIMIT, split_message};

/// What has to happen to Discord message `n` to show the new parts.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Change {
    Edit(usize),
    Send(usize),
    Delete(usize),
}

/// The changes that turn the `shown` parts into the `wanted` ones, in the order to apply them.
fn changes(shown: &[String], wanted: &[String]) -> Vec<Change> {
    let mut changes = Vec::new();
    for (n, text) in wanted.iter().enumerate() {
        match shown.get(n) {
            Some(old) if old == text => {}
            Some(_) => changes.push(Change::Edit(n)),
            None => changes.push(Change::Send(n)),
        }
    }
    // Delete from the end, so the message numbers that are left stay valid.
    for n in (wanted.len()..shown.len()).rev() {
        changes.push(Change::Delete(n));
    }
    changes
}

pub struct LiveReply {
    channel: ChannelId,
    /// The message the first part replies to.
    reply_to: Option<MessageId>,
    /// The messages sent so far and the text each one shows.
    sent: Vec<(MessageId, String)>,
}

impl LiveReply {
    pub fn new(channel: ChannelId, reply_to: Option<MessageId>) -> LiveReply {
        LiveReply {
            channel,
            reply_to,
            sent: Vec::new(),
        }
    }

    /// Updates Discord to show `parts`, one message each. Each part must fit in a message.
    pub async fn show(&mut self, http: &Http, parts: &[String]) -> anyhow::Result<()> {
        let shown: Vec<String> = self.sent.iter().map(|(_, text)| text.clone()).collect();
        for change in changes(&shown, parts) {
            match change {
                Change::Edit(n) => {
                    let edit = EditMessage::new()
                        .content(&parts[n])
                        .allowed_mentions(mentions());
                    self.channel
                        .edit_message(http, self.sent[n].0, edit)
                        .await?;
                    self.sent[n].1 = parts[n].clone();
                }
                Change::Send(n) => {
                    let mut message = CreateMessage::new()
                        .content(&parts[n])
                        .allowed_mentions(mentions())
                        // Links in answers would otherwise each get a big preview.
                        .flags(MessageFlags::SUPPRESS_EMBEDS);
                    if let (0, Some(reply_to)) = (n, self.reply_to) {
                        message = message.reference_message((self.channel, reply_to));
                    }
                    let sent = self.channel.send_message(http, message).await?;
                    self.sent.push((sent.id, parts[n].clone()));
                }
                Change::Delete(n) => {
                    self.channel.delete_message(http, self.sent[n].0).await?;
                    self.sent.pop();
                }
            }
        }
        Ok(())
    }

    /// Splits `text` and shows it.
    pub async fn show_text(&mut self, http: &Http, text: &str) -> anyhow::Result<()> {
        self.show(http, &split_message(text, DISCORD_LIMIT)).await
    }

    /// The Discord messages that make up the reply, in order.
    pub fn message_ids(&self) -> Vec<MessageId> {
        self.sent.iter().map(|(id, _)| *id).collect()
    }
}

/// Bot replies ping the person they reply to, and nobody else, whatever the text says.
fn mentions() -> CreateAllowedMentions {
    CreateAllowedMentions::new().replied_user(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn only_changed_parts_are_touched() {
        let shown = texts(&["a", "b"]);
        assert_eq!(changes(&shown, &texts(&["a", "b"])), []);
        assert_eq!(
            changes(&shown, &texts(&["a", "b2", "c"])),
            [Change::Edit(1), Change::Send(2)]
        );
        assert_eq!(changes(&[], &texts(&["a"])), [Change::Send(0)]);
    }

    #[test]
    fn extra_parts_are_deleted_from_the_end() {
        let shown = texts(&["a", "b", "c"]);
        assert_eq!(
            changes(&shown, &texts(&["a2"])),
            [Change::Edit(0), Change::Delete(2), Change::Delete(1)]
        );
    }
}
