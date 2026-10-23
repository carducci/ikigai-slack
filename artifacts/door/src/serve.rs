//! The served space: `urn:slack:thread:{key}` and its reactions as endpoints in a kernel, plus
//! the inbox space the door drops deliveries into, all behind one `Kernel` that another kernel
//! mounts over a Unix socket. The door writes the inbox through the kernel, as any caller would,
//! under the host's own root capability.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ikigai_core::{
    ArgRef, Capability, Endpoint, EndpointSpace, Error, Fallback, Invocation, Iri,
    Kernel, Representation, Request, Result, ReprType, Space, UriTemplate, Verb,
};
use roc_slack_door::{check_channel, to_json, Grants, Refusal, SlackTransport, ThreadError, Threads, READ, POST, REACT};

use roc_slack_door::Delivery;

/// The threads, shared by every endpoint and the inbox loop.
pub type Shared<T> = Arc<Mutex<Threads<T>>>;

/// The capabilities a caller holds, as the library's grants.
fn grants_of(capability: &Capability) -> Grants {
    let held = [READ, POST, REACT]
        .into_iter()
        .filter(|scope| capability.allows(scope));
    Grants::new(held)
}

/// A library refusal or failure, as the kernel's error taxonomy names it.
fn to_error(error: ThreadError) -> Error {
    match error {
        ThreadError::Refused(Refusal::BadKey) => Error::InvalidArgument {
            name: "key".to_owned(),
            detail: "a thread key is 1 to 80 letters, digits, dots, underscores or dashes".to_owned(),
        },
        ThreadError::Refused(Refusal::EmptyMessage) => Error::InvalidArgument {
            name: "content".to_owned(),
            detail: "the message text is empty".to_owned(),
        },
        ThreadError::Refused(Refusal::MessageTooLong) => Error::InvalidArgument {
            name: "content".to_owned(),
            detail: "the message text is too long".to_owned(),
        },
        ThreadError::Refused(Refusal::BadReaction) => Error::InvalidArgument {
            name: "name".to_owned(),
            detail: "a reaction name is lowercase letters, digits, underscores, dashes or pluses"
                .to_owned(),
        },
        ThreadError::Refused(Refusal::NoThread) => {
            Error::NotFound("no thread is open under that key".to_owned())
        }
        ThreadError::Refused(refusal @ Refusal::NoChannel(_)) => {
            let Refusal::NoChannel(name) = &refusal else { unreachable!() };
            Error::NotFound(format!(
                "{}: `{name}` is not a channel this door serves",
                refusal.code()
            ))
        }
        ThreadError::Refused(Refusal::NotGranted(scope)) => {
            Error::Denied(format!("this call needs `{scope}`"))
        }
        ThreadError::Unavailable { retry_after_secs } => Error::Unavailable(match retry_after_secs {
            Some(secs) => format!("Slack is busy; retry after {secs}s"),
            None => "Slack is unreachable; retry".to_owned(),
        }),
        ThreadError::Transport(why) => Error::Endpoint(format!("Slack refused: {why:?}")),
        ThreadError::State(why) => Error::Endpoint(format!("the thread map: {why}")),
    }
}

/// The text a sink carries: `content`, or `in` when the caller piped it that way.
fn text_of(inv: &Invocation<'_>) -> Result<String> {
    inv.inline_str("content")
        .or_else(|_| inv.inline_str("in"))
        .map(str::to_owned)
        .map_err(|_| Error::MissingArgument("content".to_owned()))
}

fn ok_text(message: &str) -> Representation {
    Representation::new(
        ReprType::new("text/plain").with_param("charset", "utf-8"),
        format!("{message}\n").into_bytes(),
    )
}

/// `urn:slack:thread:{key}`: sourcing reads the conversation, sinking posts to it.
pub struct ThreadEndpoint<T: SlackTransport> {
    threads: Shared<T>,
}

#[async_trait]
impl<T: SlackTransport + Send + 'static> Endpoint for ThreadEndpoint<T> {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let key = inv.bindings.get("key").unwrap_or_default();
        let grants = grants_of(inv.capability);
        match inv.request.verb {
            Verb::Source => {
                let entries = self
                    .threads
                    .lock()
                    .map_err(|_| Error::Endpoint("the thread map is poisoned".to_owned()))?
                    .read(&grants, key)
                    .map_err(to_error)?;
                Ok(Representation::new(
                    ReprType::new("application/json"),
                    to_json(&entries).into_bytes(),
                ))
            }
            Verb::Sink => {
                let text = text_of(inv)?;
                self.threads
                    .lock()
                    .map_err(|_| Error::Endpoint("the thread map is poisoned".to_owned()))?
                    .write(&grants, key, &text)
                    .map_err(to_error)?;
                Ok(ok_text("posted"))
            }
            Verb::Exists => {
                let open = self
                    .threads
                    .lock()
                    .map_err(|_| Error::Endpoint("the thread map is poisoned".to_owned()))?
                    .exists(&grants, key)
                    .map_err(to_error)?;
                Ok(ok_text(if open { "true" } else { "false" }))
            }
            other => Err(Error::Endpoint(format!(
                "urn:slack:thread takes source, sink and exists, not {other:?}"
            ))),
        }
    }

    fn name(&self) -> &str {
        "urn:slack:thread"
    }
}

/// `urn:slack:thread:{key}:reaction:{name}`: sinking adds the reaction, deleting removes it.
pub struct ReactionEndpoint<T: SlackTransport> {
    threads: Shared<T>,
}

#[async_trait]
impl<T: SlackTransport + Send + 'static> Endpoint for ReactionEndpoint<T> {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let key = inv.bindings.get("key").unwrap_or_default();
        let name = inv.bindings.get("name").unwrap_or_default();
        let add = match inv.request.verb {
            Verb::Sink => true,
            Verb::Delete => false,
            other => {
                return Err(Error::Endpoint(format!(
                    "a thread reaction takes sink and delete, not {other:?}"
                )))
            }
        };
        let grants = grants_of(inv.capability);
        self.threads
            .lock()
            .map_err(|_| Error::Endpoint("the thread map is poisoned".to_owned()))?
            .react(&grants, key, name, add)
            .map_err(to_error)?;
        Ok(ok_text(if add { "reacted" } else { "unreacted" }))
    }

    fn name(&self) -> &str {
        "urn:slack:thread:reaction"
    }
}

/// `urn:slack:channel:{rest}`: every name under a channel. A channel the host does not declare is
/// refused as `no-channel` before Slack is called; a declared one is not served yet.
pub struct ChannelEndpoint {
    declared: Vec<String>,
}

#[async_trait]
impl Endpoint for ChannelEndpoint {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let rest = inv.bindings.get("rest").unwrap_or_default();
        let name = rest.split(':').next().unwrap_or_default();
        check_channel(&self.declared, name)
            .map_err(|refusal| to_error(ThreadError::Refused(refusal)))?;
        Err(Error::NotFound(format!("`{rest}` is not served yet")))
    }

    fn name(&self) -> &str {
        "urn:slack:channel"
    }
}

/// The Slack resources. The reaction template is bound first: the thread template's final
/// variable would otherwise take the whole reaction IRI as a key.
pub fn slack_space<T: SlackTransport + Send + 'static>(
    threads: Shared<T>,
    declared: Vec<String>,
) -> EndpointSpace {
    EndpointSpace::new()
        .bind(
            UriTemplate::parse("urn:slack:channel:{rest}").expect("a fixed template"),
            ChannelEndpoint { declared },
        )
        .bind(
            UriTemplate::parse("urn:slack:thread:{key}:reaction:{name}").expect("a fixed template"),
            ReactionEndpoint {
                threads: Arc::clone(&threads),
            },
        )
        .bind(
            UriTemplate::parse("urn:slack:thread:{key}").expect("a fixed template"),
            ThreadEndpoint { threads },
        )
}

/// The kernel that serves both spaces: the Slack resources, and the intray spaces under `root`.
pub fn kernel<T: SlackTransport + Send + 'static>(
    threads: Shared<T>,
    root: std::path::PathBuf,
    declared: Vec<String>,
) -> Kernel {
    let spaces: Vec<Arc<dyn Space>> = vec![
        Arc::new(slack_space(threads, declared)),
        Arc::new(ikigai_intray::space(root)),
    ];
    Kernel::new(Arc::new(Fallback::new(spaces)))
}

/// Drops one delivery into the inbox space `space`, through the kernel under the host's root
/// capability. The tuple is Turtle, so a host can match it with a SPARQL template.
pub fn drop_delivery(kernel: &Kernel, space: &str, delivery: &Delivery) -> std::result::Result<(), String> {
    let target = Iri::parse(&format!("urn:space:{space}")).map_err(|e| format!("{e:?}"))?;
    let request = Request::new(Verb::Sink, target)
        .with_arg("content", ArgRef::Inline(turtle(delivery).into_bytes()));
    futures_executor::block_on(kernel.issue(request, &Capability::root()))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// A delivery as one Turtle tuple: a blank node typed `slack:Message` or `slack:Reaction`.
pub fn turtle(delivery: &Delivery) -> String {
    let mut predicates: Vec<String> = Vec::new();
    match delivery {
        Delivery::Message {
            key,
            user,
            time,
            text,
        } => {
            predicates.push("a slack:Message".to_owned());
            if let Some(key) = key {
                predicates.push(format!("slack:key {}", literal(key)));
            }
            predicates.push(format!("slack:user {}", literal(user)));
            predicates.push(format!("slack:time {}", literal(time)));
            predicates.push(format!("slack:text {}", literal(text)));
        }
        Delivery::Reaction { key, user, name } => {
            predicates.push("a slack:Reaction".to_owned());
            predicates.push(format!("slack:key {}", literal(key)));
            predicates.push(format!("slack:user {}", literal(user)));
            predicates.push(format!("slack:name {}", literal(name)));
        }
    }
    format!(
        "@prefix slack: <urn:slack:> .\n[] {} .\n",
        predicates.join(" ;\n   ")
    )
}

/// A Turtle string literal.
fn literal(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use roc_slack_door::FakeSlack;

    fn door(name: &str) -> (Kernel, Shared<FakeSlack>) {
        let dir = std::env::temp_dir().join(format!("roc-door-serve-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let threads = Arc::new(Mutex::new(Threads::open(&dir, "C1", FakeSlack::new()).unwrap()));
        let kernel = kernel(Arc::clone(&threads), dir.join("spaces"), vec!["gonk-chat".to_owned()]);
        (kernel, threads)
    }

    fn issue(kernel: &Kernel, verb: Verb, iri: &str) -> Result<Representation> {
        let mut request = Request::new(verb, Iri::parse(iri).unwrap());
        if verb == Verb::Sink {
            request = request.with_arg("content", ArgRef::Inline(b"hello".to_vec()));
        }
        futures_executor::block_on(kernel.issue(request, &Capability::root()))
    }

    #[test]
    fn an_undeclared_channel_is_refused_on_every_verb_and_slack_is_not_called() {
        let (kernel, threads) = door("undeclared");
        for iri in [
            "urn:slack:channel:random:thread:x",
            "urn:slack:channel:random:thread:x:reaction:tada",
            "urn:slack:channel:random:inbox",
            "urn:slack:channel:random",
        ] {
            for verb in [Verb::Source, Verb::Sink, Verb::Delete] {
                let error = issue(&kernel, verb, iri).unwrap_err().to_string();
                assert!(error.contains("no-channel") && error.contains("random"), "{iri}: {error}");
            }
        }
        assert!(threads.lock().unwrap().transport().posts().is_empty());
        assert!(threads.lock().unwrap().transport().reactions().is_empty());
    }

    #[test]
    fn exists_through_the_door_answers_for_a_closed_and_an_open_thread() {
        let (kernel, _threads) = door("exists");
        let answer = |iri| {
            let reply = issue(&kernel, Verb::Exists, iri).unwrap();
            String::from_utf8(reply.bytes.to_vec()).unwrap()
        };
        assert_eq!(answer("urn:slack:thread:never-opened"), "false\n");
        issue(&kernel, Verb::Sink, "urn:slack:thread:k").unwrap();
        assert_eq!(answer("urn:slack:thread:k"), "true\n");
    }

    #[test]
    fn a_message_tuple_carries_its_key_user_time_and_text() {
        let tuple = turtle(&Delivery::Message {
            key: Some("carducci-54".to_owned()),
            user: "U1".to_owned(),
            time: "2023-11-14T22:15:00Z".to_owned(),
            text: "Use \"the\" token".to_owned(),
        });
        assert!(tuple.contains("a slack:Message"));
        assert!(tuple.contains("slack:key \"carducci-54\""));
        assert!(tuple.contains("slack:text \"Use \\\"the\\\" token\""));
    }

    #[test]
    fn a_keyless_message_has_no_key_predicate() {
        let tuple = turtle(&Delivery::Message {
            key: None,
            user: "U1".to_owned(),
            time: "2023-11-14T22:15:00Z".to_owned(),
            text: "What's blocked today?".to_owned(),
        });
        assert!(!tuple.contains("slack:key"));
    }

    #[test]
    fn a_reaction_tuple_names_the_reaction() {
        let tuple = turtle(&Delivery::Reaction {
            key: "carducci-54".to_owned(),
            user: "U1".to_owned(),
            name: "+1".to_owned(),
        });
        assert!(tuple.contains("a slack:Reaction"));
        assert!(tuple.contains("slack:name \"+1\""));
    }
}
