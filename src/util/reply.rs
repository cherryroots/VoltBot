//! A bot reply that may span several Discord messages and changes over time, like a chat
//! answer streaming in.
//!
//! [`LiveReply::show`] takes the full list of parts each time (from
//! [`split_message`](super::split::split_message)) and makes Discord match it: it edits the
//! parts whose text changed, sends the new ones, and deletes the ones that are no longer
//! needed. Parts that didn't change cost no API call.

use serenity::all::{
    ChannelId, CreateAllowedMentions, CreateAttachment, CreateMessage, EditMessage, Http, Message,
    MessageFlags, MessageId,
};

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
    /// Set by [`LiveReply::resume`]: the next edits also remove the old messages' files.
    clear_files: bool,
}

impl LiveReply {
    pub fn new(channel: ChannelId, reply_to: Option<MessageId>) -> LiveReply {
        LiveReply {
            channel,
            reply_to,
            sent: Vec::new(),
            clear_files: false,
        }
    }

    /// Takes over messages that were already sent, for example to show a regenerated answer
    /// in place of the old one. The next [`LiveReply::show`] edits every message and removes
    /// their files.
    pub fn resume(
        channel: ChannelId,
        reply_to: Option<MessageId>,
        messages: &[MessageId],
    ) -> LiveReply {
        LiveReply {
            channel,
            reply_to,
            // An impossible text, so every message counts as changed.
            sent: messages.iter().map(|id| (*id, "\0".to_string())).collect(),
            clear_files: true,
        }
    }

    /// Updates Discord to show `parts`, one message each. Each part must fit in a message.
    pub async fn show(&mut self, http: &Http, parts: &[String]) -> anyhow::Result<()> {
        let shown: Vec<String> = self.sent.iter().map(|(_, text)| text.clone()).collect();
        for change in changes(&shown, parts) {
            match change {
                Change::Edit(n) => {
                    let mut edit = EditMessage::new()
                        .content(&parts[n])
                        .allowed_mentions(mentions());
                    if self.clear_files {
                        edit = edit.remove_all_attachments();
                    }
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
        self.clear_files = false;
        Ok(())
    }

    /// Adds files to the last message. Returns the message, whose attachments now have
    /// Discord links.
    pub async fn attach(
        &mut self,
        http: &Http,
        files: Vec<CreateAttachment>,
    ) -> anyhow::Result<Message> {
        let (id, _) = self
            .sent
            .last()
            .ok_or_else(|| anyhow::anyhow!("nothing was sent yet"))?;
        let mut edit = EditMessage::new();
        for file in files {
            edit = edit.new_attachment(file);
        }
        Ok(self.channel.edit_message(http, *id, edit).await?)
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
