//! Threads: a caller's key mapped to the conversation it opened, kept durably in the state
//! directory so the key names the same thread after the door restarts.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::{
    check_key, check_reaction, check_text, Entry, Grants, Refusal, SlackTransport, TransportError,
    POST, READ, REACT,
};

/// The map's file in the state directory: one thread per line, `key TAB channel TAB ts`.
const MAP_FILE: &str = "threads.tsv";

/// A conversation the door has opened: the channel it is in and its opening message's timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    pub channel: String,
    pub ts: String,
}

/// Why a write to a thread did not happen.
#[derive(Debug)]
pub enum ThreadError {
    /// The key is not a valid thread key; nothing was posted.
    Refused(Refusal),
    /// Slack would not post the text, for a reason the caller must fix (a token or a permission).
    Transport(TransportError),
    /// Slack is throttling the door or cannot be reached; retry, after `retry_after_secs` when
    /// Slack gave one.
    Unavailable { retry_after_secs: Option<u64> },
    /// The map could not be read or saved.
    State(io::Error),
}

/// Slack's transient failures are unavailable, carrying its retry-after; anything else is an
/// error that names what Slack refused.
fn from_transport(error: TransportError) -> ThreadError {
    match error {
        TransportError::RateLimited { retry_after_secs } => ThreadError::Unavailable {
            retry_after_secs: Some(retry_after_secs),
        },
        TransportError::Unavailable(_) => ThreadError::Unavailable {
            retry_after_secs: None,
        },
        rejected @ TransportError::Rejected(_) => ThreadError::Transport(rejected),
    }
}

/// The threads a door holds, over one transport and one configured channel.
pub struct Threads<T: SlackTransport> {
    state_dir: PathBuf,
    /// The declared channels as (name, Slack id); the first is the one the unscoped calls use.
    channels: Vec<(String, String)>,
    transport: T,
    /// Keyed by channel id and then key, so a key is unique within its channel.
    threads: BTreeMap<(String, String), Thread>,
}

impl<T: SlackTransport> Threads<T> {
    /// Opens the map in `state_dir`, creating the directory and an empty map when there is none.
    pub fn open(state_dir: &Path, channel: &str, transport: T) -> io::Result<Self> {
        Self::open_channels(state_dir, &[(String::new(), channel.to_owned())], transport)
    }

    /// Opens the map for the declared `channels`, each a (name, Slack id) pair.
    pub fn open_channels(
        state_dir: &Path,
        channels: &[(String, String)],
        transport: T,
    ) -> io::Result<Self> {
        fs::create_dir_all(state_dir)?;
        let threads = match fs::read_to_string(state_dir.join(MAP_FILE)) {
            Ok(text) => parse(&text)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            state_dir: state_dir.to_owned(),
            channels: channels.to_vec(),
            transport,
            threads,
        })
    }

    /// The Slack id of the declared channel `name`, or `no-channel`.
    fn id_of(&self, name: &str) -> Result<String, ThreadError> {
        self.channels
            .iter()
            .find(|(declared, _)| declared == name)
            .map(|(_, id)| id.clone())
            .ok_or_else(|| ThreadError::Refused(Refusal::NoChannel(name.to_owned())))
    }

    /// The id of the channel the unscoped calls use.
    fn default_id(&self) -> String {
        self.channels[0].1.clone()
    }

    /// `write` in the declared channel `channel`.
    pub fn write_in(
        &mut self,
        grants: &Grants,
        channel: &str,
        key: &str,
        text: &str,
    ) -> Result<(), ThreadError> {
        let id = self.id_of(channel)?;
        self.write_at(grants, &id, key, text)
    }

    /// `read` in the declared channel `channel`.
    pub fn read_in(
        &mut self,
        grants: &Grants,
        channel: &str,
        key: &str,
    ) -> Result<Vec<Entry>, ThreadError> {
        let id = self.id_of(channel)?;
        self.read_at(grants, &id, key)
    }

    /// Writes `text` to the thread `key` names. A key with no thread opens one as a new top-level
    /// message and records it; a key with a thread replies in it.
    pub fn write(&mut self, grants: &Grants, key: &str, text: &str) -> Result<(), ThreadError> {
        let id = self.default_id();
        self.write_at(grants, &id, key, text)
    }

    fn write_at(
        &mut self,
        grants: &Grants,
        channel: &str,
        key: &str,
        text: &str,
    ) -> Result<(), ThreadError> {
        grants.require(POST).map_err(ThreadError::Refused)?;
        check_key(key).map_err(ThreadError::Refused)?;
        check_text(text).map_err(ThreadError::Refused)?;
        let parent = self.threads.get(&slot(channel, key)).map(|thread| thread.ts.clone());
        let ts = self
            .transport
            .post(channel, parent.as_deref(), text)
            .map_err(from_transport)?;
        if parent.is_none() {
            let thread = Thread {
                channel: channel.to_owned(),
                ts,
            };
            self.threads.insert(slot(channel, key), thread);
            self.save().map_err(ThreadError::State)?;
        }
        Ok(())
    }

    /// Adds the reaction `name` to the opening message of the thread `key` names, or removes it
    /// when `add` is false. A key with no thread is refused before Slack is called.
    pub fn react(
        &mut self,
        grants: &Grants,
        key: &str,
        name: &str,
        add: bool,
    ) -> Result<(), ThreadError> {
        grants.require(REACT).map_err(ThreadError::Refused)?;
        check_key(key).map_err(ThreadError::Refused)?;
        check_reaction(name).map_err(ThreadError::Refused)?;
        let thread = self
            .threads
            .get(&slot(&self.default_id(), key))
            .ok_or(ThreadError::Refused(Refusal::NoThread))?;
        self.transport
            .react(&thread.channel, &thread.ts, name, add)
            .map_err(from_transport)
    }

    /// The key of the thread in channel id `channel` whose opening message has timestamp `ts`, if
    /// the door opened one: timestamps are unique only within a channel.
    pub fn key_for(&self, channel: &str, ts: &str) -> Option<String> {
        self.threads
            .iter()
            .find(|(_, thread)| thread.channel == channel && thread.ts == ts)
            .map(|((_, key), _)| key.clone())
    }

    /// Whether the key names a thread the door has opened.
    pub fn exists(&self, grants: &Grants, key: &str) -> Result<bool, ThreadError> {
        grants.require(READ).map_err(ThreadError::Refused)?;
        check_key(key).map_err(ThreadError::Refused)?;
        Ok(self.threads.contains_key(&slot(&self.default_id(), key)))
    }

    /// The conversation the key names, the opening message first.
    pub fn read(&mut self, grants: &Grants, key: &str) -> Result<Vec<Entry>, ThreadError> {
        let id = self.default_id();
        self.read_at(grants, &id, key)
    }

    fn read_at(&mut self, grants: &Grants, channel: &str, key: &str) -> Result<Vec<Entry>, ThreadError> {
        grants.require(READ).map_err(ThreadError::Refused)?;
        check_key(key).map_err(ThreadError::Refused)?;
        let thread = self
            .threads
            .get(&slot(channel, key))
            .ok_or(ThreadError::Refused(Refusal::NoThread))?;
        let messages = self
            .transport
            .replies(&thread.channel, &thread.ts)
            .map_err(from_transport)?;
        Ok(messages.iter().map(Entry::from).collect())
    }

    /// The transport the threads post through.
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// The transport, mutably: a test feeds it the events a person would have caused.
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Writes the whole map to a temporary file and renames it over the old one, so a crash
    /// leaves either the old map or the new one.
    fn save(&self) -> io::Result<()> {
        let tmp = self.state_dir.join("threads.tsv.tmp");
        let mut file = fs::File::create(&tmp)?;
        for ((_, key), thread) in &self.threads {
            writeln!(file, "{key}\t{}\t{}", thread.channel, thread.ts)?;
        }
        file.sync_all()?;
        fs::rename(tmp, self.state_dir.join(MAP_FILE))
    }
}

/// The map's index for a key in a channel.
fn slot(channel: &str, key: &str) -> (String, String) {
    (channel.to_owned(), key.to_owned())
}

fn parse(text: &str) -> io::Result<BTreeMap<(String, String), Thread>> {
    text.lines()
        .map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            match fields.as_slice() {
                [key, channel, ts] => Ok((
                    slot(channel, key),
                    Thread {
                        channel: (*channel).to_owned(),
                        ts: (*ts).to_owned(),
                    },
                )),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad line in {MAP_FILE}: {line:?}"),
                )),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Event, FakeSlack, MAX_MESSAGE_LEN};

    /// A fresh, empty state directory for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("roc-slack-door-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn all() -> Grants {
        Grants::all()
    }

    #[test]
    fn key_for_tells_equal_timestamps_in_two_channels_apart() {
        let dir = scratch("key-for");
        let channels = [("a".to_owned(), "C1".to_owned()), ("b".to_owned(), "C2".to_owned())];
        let mut threads = Threads::open_channels(&dir, &channels, FakeSlack::new()).unwrap();
        for (channel, key) in [("C1", "one"), ("C2", "two")] {
            let thread = Thread { channel: channel.to_owned(), ts: "1.0".to_owned() };
            threads.threads.insert(slot(channel, key), thread);
        }
        assert_eq!(threads.key_for("C1", "1.0").as_deref(), Some("one"));
        assert_eq!(threads.key_for("C2", "1.0").as_deref(), Some("two"));
    }

    #[test]
    fn a_first_write_opens_a_top_level_thread() {
        let dir = scratch("first-write");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        threads.write(&all(), "carducci-54", "Blocked on a token").unwrap();
        let posts = threads.transport().posts();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].thread, None);
        assert_eq!(posts[0].text, "Blocked on a token");
    }

    #[test]
    fn a_later_write_replies_in_the_thread() {
        let dir = scratch("reply");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        threads.write(&all(), "carducci-54", "Opening").unwrap();
        threads.write(&all(), "carducci-54", "Here is the command").unwrap();
        let posts = threads.transport().posts();
        assert_eq!(posts[1].thread.as_deref(), Some(posts[0].ts.as_str()));
    }

    #[test]
    fn the_thread_survives_a_restart() {
        let dir = scratch("restart");
        let mut before = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        before.write(&all(), "carducci-54", "Opening").unwrap();
        let opening = before.transport().posts()[0].ts.clone();
        drop(before);

        let mut after = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        after.write(&all(), "carducci-54", "Still blocked").unwrap();
        assert_eq!(after.transport().posts()[0].thread.as_deref(), Some(opening.as_str()));
    }

    #[test]
    fn empty_and_whitespace_text_is_refused_and_nothing_is_posted() {
        let dir = scratch("empty-text");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        let err = threads.write(&all(), "carducci-54", "  \n\t ").unwrap_err();
        assert!(matches!(err, ThreadError::Refused(Refusal::EmptyMessage)));
        assert!(threads.transport().posts().is_empty());
    }

    #[test]
    fn text_over_the_limit_is_refused_and_nothing_is_posted() {
        let dir = scratch("long-text");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        let too_long = "x".repeat(MAX_MESSAGE_LEN + 1);
        let err = threads.write(&all(), "carducci-54", &too_long).unwrap_err();
        assert!(matches!(err, ThreadError::Refused(Refusal::MessageTooLong)));
        assert!(threads.transport().posts().is_empty());
    }

    #[test]
    fn text_at_the_limit_is_accepted() {
        let dir = scratch("limit-text");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        threads.write(&all(), "carducci-54", &"x".repeat(MAX_MESSAGE_LEN)).unwrap();
        assert_eq!(threads.transport().posts().len(), 1);
    }

    #[test]
    fn reading_a_thread_returns_the_opening_message_then_the_replies() {
        let dir = scratch("read");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        threads.write(&all(), "carducci-54", "Blocked on a token").unwrap();
        let root = threads.transport().posts()[0].ts.clone();
        threads.transport_mut().feed(Event::Message {
            id: "Ev1".to_owned(),
            channel: "C1".to_owned(),
            user: "U1".to_owned(),
            ts: "1700000100.000001".to_owned(),
            thread: Some(root),
            text: "Which token?".to_owned(),
        });
        let entries = threads.read(&all(), "carducci-54").unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].bot && entries[0].text == "Blocked on a token");
        assert!(!entries[1].bot && entries[1].user == "U1" && entries[1].text == "Which token?");
    }

    #[test]
    fn the_json_reading_has_no_channel_or_message_ids() {
        let dir = scratch("read-json");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        threads.write(&all(), "carducci-54", "Blocked on a token").unwrap();
        let root = threads.transport().posts()[0].ts.clone();
        let json = crate::to_json(&threads.read(&all(), "carducci-54").unwrap());
        assert!(json.contains("\"bot\":true"));
        assert!(!json.contains("C1"));
        assert!(!json.contains(&root));
    }

    #[test]
    fn a_rate_limit_is_unavailable_with_slacks_retry_after() {
        let dir = scratch("rate-limit");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        threads.transport_mut().fail_next(TransportError::RateLimited { retry_after_secs: 30 });
        let err = threads.write(&all(), "carducci-54", "Opening").unwrap_err();
        assert!(matches!(err, ThreadError::Unavailable { retry_after_secs: Some(30) }));
        assert!(threads.transport().posts().is_empty());
    }

    #[test]
    fn an_outage_is_unavailable_without_a_retry_after() {
        let dir = scratch("outage");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        threads.write(&all(), "carducci-54", "Opening").unwrap();
        threads.transport_mut().fail_next(TransportError::Unavailable("503".to_owned()));
        let err = threads.read(&all(), "carducci-54").unwrap_err();
        assert!(matches!(err, ThreadError::Unavailable { retry_after_secs: None }));
    }

    #[test]
    fn a_rejected_token_is_an_error_that_names_what_slack_said() {
        let dir = scratch("rejected");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        threads.transport_mut().fail_next(TransportError::Rejected("invalid_auth".to_owned()));
        let err = threads.write(&all(), "carducci-54", "Opening").unwrap_err();
        assert!(matches!(
            err,
            ThreadError::Transport(TransportError::Rejected(ref why)) if why == "invalid_auth"
        ));
    }

    #[test]
    fn a_reaction_marks_the_opening_message_and_can_be_removed() {
        let dir = scratch("react");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        threads.write(&all(), "carducci-54", "Opening").unwrap();
        let root = threads.transport().posts()[0].ts.clone();
        threads.react(&all(), "carducci-54", "white_check_mark", true).unwrap();
        threads.react(&all(), "carducci-54", "white_check_mark", true).unwrap();
        assert_eq!(threads.transport().reactions().len(), 1);
        assert!(threads.transport().reactions().contains(&(root, "white_check_mark".to_owned())));
        threads.react(&all(), "carducci-54", "white_check_mark", false).unwrap();
        assert!(threads.transport().reactions().is_empty());
    }

    #[test]
    fn a_reaction_on_a_key_with_no_thread_is_refused_and_nothing_is_added() {
        let dir = scratch("react-none");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        let err = threads.react(&all(), "never-opened", "white_check_mark", true).unwrap_err();
        assert!(matches!(err, ThreadError::Refused(Refusal::NoThread)));
        assert!(threads.transport().reactions().is_empty());
    }

    #[test]
    fn a_reaction_with_a_bad_name_is_refused_and_nothing_is_added() {
        let dir = scratch("react-bad-name");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        threads.write(&all(), "carducci-54", "Opening").unwrap();
        for name in ["White Check", "tada!", "", "Tada"] {
            let err = threads.react(&all(), "carducci-54", name, true).unwrap_err();
            assert!(matches!(err, ThreadError::Refused(Refusal::BadReaction)));
        }
        assert!(threads.transport().reactions().is_empty());
    }

    #[test]
    fn a_key_exists_once_its_thread_is_opened() {
        let dir = scratch("exists");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        assert!(!threads.exists(&all(), "carducci-54").unwrap());
        threads.write(&all(), "carducci-54", "Opening").unwrap();
        assert!(threads.exists(&all(), "carducci-54").unwrap());
        assert!(matches!(
            threads.exists(&all(), "item 54"),
            Err(ThreadError::Refused(Refusal::BadKey))
        ));
    }

    #[test]
    fn reading_a_key_with_no_thread_is_refused() {
        let dir = scratch("read-none");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        let err = threads.read(&all(), "never-opened").unwrap_err();
        assert!(matches!(err, ThreadError::Refused(Refusal::NoThread)));
    }

    #[test]
    fn a_bad_key_is_refused_and_nothing_is_posted() {
        let dir = scratch("bad-key");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        let err = threads.write(&all(), "item 54", "Opening").unwrap_err();
        assert!(matches!(err, ThreadError::Refused(Refusal::BadKey)));
        assert!(threads.transport().posts().is_empty());
    }

    #[test]
    fn posting_without_the_post_capability_is_refused_and_nothing_is_posted() {
        let dir = scratch("no-post");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        let read_only = Grants::new([READ]);
        let err = threads.write(&read_only, "carducci-54", "Opening").unwrap_err();
        assert!(matches!(err, ThreadError::Refused(Refusal::NotGranted(POST))));
        assert!(threads.transport().posts().is_empty());
    }

    #[test]
    fn reacting_without_the_react_capability_is_refused_and_nothing_is_added() {
        let dir = scratch("no-react");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        threads.write(&all(), "carducci-54", "Opening").unwrap();
        let read_only = Grants::new([READ]);
        let err = threads.react(&read_only, "carducci-54", "tada", true).unwrap_err();
        assert!(matches!(err, ThreadError::Refused(Refusal::NotGranted(REACT))));
        assert!(threads.transport().reactions().is_empty());
    }

    #[test]
    fn reading_without_the_read_capability_is_refused() {
        let dir = scratch("no-read");
        let mut threads = Threads::open(&dir, "C1", FakeSlack::new()).unwrap();
        threads.write(&all(), "carducci-54", "Opening").unwrap();
        let post_only = Grants::new([POST]);
        let err = threads.read(&post_only, "carducci-54").unwrap_err();
        assert!(matches!(err, ThreadError::Refused(Refusal::NotGranted(READ))));
    }
}
