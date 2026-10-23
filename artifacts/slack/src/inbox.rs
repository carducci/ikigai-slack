//! The inbox: what allowed people say in the configured channel, turned into deliveries for a
//! host to act on. A delivery names the thread it belongs to by the key the caller chose, never
//! by a Slack id, and a message outside any thread the door opened carries no key.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::conversation::utc;
use crate::{Event, SlackTransport, Threads};

/// The file in the state directory that lists the ids of events already delivered.
const DELIVERED_FILE: &str = "delivered.txt";

/// One thing the inbox delivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// A message by an allowed person. `key` is the thread it replies in, when the door opened
    /// that thread; a message outside any such thread has no key.
    Message {
        key: Option<String>,
        user: String,
        /// The time in UTC, as `2023-11-14T22:13:20Z`.
        time: String,
        text: String,
    },
    /// A reaction by an allowed person on the opening message of the thread `key` names.
    Reaction {
        key: String,
        user: String,
        name: String,
    },
}

/// The filter between Slack's events and the inbox: only allowed people, in the configured
/// channel, never the door's own bot, and each event at most once across restarts.
#[derive(Debug)]
pub struct Inbox {
    channel: String,
    bot_user: String,
    allowed: BTreeSet<String>,
    state_file: PathBuf,
    delivered: BTreeSet<String>,
}

impl Inbox {
    /// An inbox for `channel`, delivering only what the Slack users in `allowed` say, and never
    /// what `bot_user` posts. The ids of delivered events are kept in `state_dir`.
    pub fn open<I, S>(state_dir: &Path, channel: &str, bot_user: &str, allowed: I) -> io::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        fs::create_dir_all(state_dir)?;
        let state_file = state_dir.join(DELIVERED_FILE);
        let delivered = match fs::read_to_string(&state_file) {
            Ok(text) => text.lines().map(str::to_owned).collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeSet::new(),
            Err(e) => return Err(e),
        };
        Ok(Inbox {
            channel: channel.to_owned(),
            bot_user: bot_user.to_owned(),
            allowed: allowed.into_iter().map(Into::into).collect(),
            state_file,
            delivered,
        })
    }

    /// Decides what, if anything, an event delivers. A redelivery of an event already delivered
    /// delivers nothing. `threads` says which thread an event's timestamp opens, so a reply in a
    /// thread the door opened carries its key.
    ///
    /// The id is recorded before the delivery is returned: a crash after this point loses the
    /// delivery rather than repeating it, which is the at-most-once the spec asks for.
    pub fn deliver<T: SlackTransport>(
        &mut self,
        event: Event,
        threads: &Threads<T>,
    ) -> io::Result<Option<Delivery>> {
        let event_id = event.id().to_owned();
        if self.delivered.contains(&event_id) {
            return Ok(None);
        }
        let Some(delivery) = self.decide(event, threads) else {
            return Ok(None);
        };
        self.remember(&event_id)?;
        Ok(Some(delivery))
    }

    fn decide<T: SlackTransport>(&self, event: Event, threads: &Threads<T>) -> Option<Delivery> {
        match event {
            Event::Message {
                channel,
                user,
                ts,
                thread,
                text,
                ..
            } => {
                if !self.may_speak(&channel, &user) {
                    return None;
                }
                let key = thread.as_deref().and_then(|root| threads.key_for(&channel, root));
                Some(Delivery::Message {
                    key,
                    user,
                    time: utc(&ts),
                    text,
                })
            }
            Event::Reaction {
                channel,
                user,
                message,
                name,
                ..
            } => {
                if !self.may_speak(&channel, &user) {
                    return None;
                }
                let key = threads.key_for(&channel, &message)?;
                Some(Delivery::Reaction { key, user, name })
            }
        }
    }

    /// Whether a person in a channel may be delivered: the configured channel, an allowed
    /// person, and not the door's own bot. A message with no user came from a bot.
    fn may_speak(&self, channel: &str, user: &str) -> bool {
        channel == self.channel
            && !user.is_empty()
            && user != self.bot_user
            && self.allowed.contains(user)
    }

    /// Appends the id to the delivered file, synced, before it is remembered in memory.
    fn remember(&mut self, event_id: &str) -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.state_file)?;
        writeln!(file, "{event_id}")?;
        file.sync_all()?;
        self.delivered.insert(event_id.to_owned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FakeSlack, Grants};

    const CHANNEL: &str = "C1";
    const BOT: &str = "UBOT";

    /// A state directory, empty, for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("roc-slack-door-inbox-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// Threads with one thread, `carducci-54`, opened by the door, and that thread's timestamp.
    fn with_thread(dir: &Path) -> (Threads<FakeSlack>, String) {
        let mut threads = Threads::open(dir, CHANNEL, FakeSlack::new()).unwrap();
        threads
            .write(&Grants::all(), "carducci-54", "Blocked on a token")
            .unwrap();
        let root = threads.transport().posts()[0].ts.clone();
        (threads, root)
    }

    fn inbox(dir: &Path) -> Inbox {
        Inbox::open(dir, CHANNEL, BOT, ["U1"]).unwrap()
    }

    fn reply(channel: &str, user: &str, thread: Option<&str>, text: &str) -> Event {
        Event::Message {
            id: "Ev1".to_owned(),
            channel: channel.to_owned(),
            user: user.to_owned(),
            ts: "1700000100.000001".to_owned(),
            thread: thread.map(str::to_owned),
            text: text.to_owned(),
        }
    }

    #[test]
    fn an_allowed_reply_in_a_thread_lands_with_its_key() {
        let dir = scratch("reply");
        let (threads, root) = with_thread(&dir);
        let event = reply(CHANNEL, "U1", Some(&root), "Use the fine-grained token");
        let delivery = inbox(&dir).deliver(event, &threads).unwrap();
        assert_eq!(
            delivery,
            Some(Delivery::Message {
                key: Some("carducci-54".to_owned()),
                user: "U1".to_owned(),
                time: "2023-11-14T22:15:00Z".to_owned(),
                text: "Use the fine-grained token".to_owned(),
            })
        );
    }

    #[test]
    fn a_stranger_is_not_delivered() {
        let dir = scratch("stranger");
        let (threads, root) = with_thread(&dir);
        let event = reply(CHANNEL, "U9", Some(&root), "Hi");
        assert_eq!(inbox(&dir).deliver(event, &threads).unwrap(), None);
    }

    #[test]
    fn the_doors_own_posts_are_not_delivered() {
        let dir = scratch("own");
        let (threads, root) = with_thread(&dir);
        let event = reply(CHANNEL, BOT, Some(&root), "Hi");
        assert_eq!(inbox(&dir).deliver(event, &threads).unwrap(), None);
    }

    #[test]
    fn a_bot_message_with_no_user_is_not_delivered() {
        let dir = scratch("bot");
        let (threads, root) = with_thread(&dir);
        let event = reply(CHANNEL, "", Some(&root), "Hi");
        assert_eq!(inbox(&dir).deliver(event, &threads).unwrap(), None);
    }

    #[test]
    fn a_message_in_another_channel_is_not_delivered() {
        let dir = scratch("channel");
        let (threads, root) = with_thread(&dir);
        let event = reply("C2", "U1", Some(&root), "Hi");
        assert_eq!(inbox(&dir).deliver(event, &threads).unwrap(), None);
    }

    #[test]
    fn a_top_level_message_lands_with_no_key() {
        let dir = scratch("top-level");
        let (threads, _) = with_thread(&dir);
        let event = reply(CHANNEL, "U1", None, "What's blocked today?");
        let delivery = inbox(&dir).deliver(event, &threads).unwrap();
        assert!(matches!(delivery, Some(Delivery::Message { key: None, .. })));
    }

    #[test]
    fn a_reply_in_a_thread_the_door_did_not_open_lands_with_no_key() {
        let dir = scratch("not-ours");
        let (threads, _) = with_thread(&dir);
        let event = reply(CHANNEL, "U1", Some("1700000000.999999"), "Hi");
        let delivery = inbox(&dir).deliver(event, &threads).unwrap();
        assert!(matches!(delivery, Some(Delivery::Message { key: None, .. })));
    }

    #[test]
    fn an_allowed_reaction_on_an_opening_message_lands_with_the_key() {
        let dir = scratch("reaction");
        let (threads, root) = with_thread(&dir);
        let event = Event::Reaction {
            id: "Ev1".to_owned(),
            channel: CHANNEL.to_owned(),
            user: "U1".to_owned(),
            message: root,
            name: "+1".to_owned(),
        };
        assert_eq!(
            inbox(&dir).deliver(event, &threads).unwrap(),
            Some(Delivery::Reaction {
                key: "carducci-54".to_owned(),
                user: "U1".to_owned(),
                name: "+1".to_owned(),
            })
        );
    }

    #[test]
    fn a_reaction_on_a_message_the_door_did_not_open_is_not_delivered() {
        let dir = scratch("reaction-other");
        let (threads, _) = with_thread(&dir);
        let event = Event::Reaction {
            id: "Ev1".to_owned(),
            channel: CHANNEL.to_owned(),
            user: "U1".to_owned(),
            message: "1700000000.999999".to_owned(),
            name: "+1".to_owned(),
        };
        assert_eq!(inbox(&dir).deliver(event, &threads).unwrap(), None);
    }

    #[test]
    fn a_redelivered_event_is_delivered_once_even_across_a_restart() {
        let dir = scratch("redeliver");
        let (threads, root) = with_thread(&dir);
        let event = reply(CHANNEL, "U1", Some(&root), "Which token?");
        let mut before = inbox(&dir);
        assert!(before.deliver(event.clone(), &threads).unwrap().is_some());
        assert_eq!(before.deliver(event.clone(), &threads).unwrap(), None);
        drop(before);

        let mut after = inbox(&dir);
        assert_eq!(after.deliver(event, &threads).unwrap(), None);
    }
}
