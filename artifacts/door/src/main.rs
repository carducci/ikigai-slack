//! `roc-slack-door`: the door. It reads the host's configuration, holds the Socket Mode
//! connection, serves the Slack resources over a Unix socket in the state directory, and writes
//! the inbox through its own kernel (slack-resources 4.x).

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

mod config;
mod serve;
mod socket;
mod web_api;

use config::Config;
use roc_slack_door::{Inbox, SlackTransport, Threads};

/// How often the inbox loop takes what Socket Mode has heard.
const POLL: Duration = Duration::from_millis(200);

fn main() -> ExitCode {
    // The one argument is the host's configuration file: `roc-slack-door --config PATH`.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let file: BTreeMap<String, String> = match args.as_slice() {
        [flag, path] if flag == "--config" => match std::fs::read_to_string(path) {
            Ok(text) => config::parse_file(&text),
            Err(e) => {
                eprintln!("roc-slack-door: cannot read {path}: {e}");
                return ExitCode::from(2);
            }
        },
        [] => BTreeMap::new(),
        _ => {
            eprintln!("usage: roc-slack-door [--config PATH]");
            return ExitCode::from(2);
        }
    };
    match Config::resolve(&file, |name| std::env::var(name).ok()) {
        Ok(config) => run(config),
        Err(refused) => {
            for problem in &refused.0 {
                eprintln!("roc-slack-door: refusing to start: {problem}");
            }
            ExitCode::from(2)
        }
    }
}

/// Starts the door and runs it until the process stops: the Socket Mode connection, the Web API,
/// the kernel on `door.sock` in the state directory, and the inbox loop that writes deliveries
/// through that kernel.
fn run(config: Config) -> ExitCode {
    if let Err(e) = std::fs::create_dir_all(&config.state_dir) {
        eprintln!("roc-slack-door: cannot create the state directory: {e}");
        return ExitCode::from(2);
    }

    // Until the per-channel resources land (tasks 1.2 to 3.1), the door serves the first
    // declared channel, and its inbox space is named for that channel.
    let channel = config.channels[0].clone();

    let (events_tx, events_rx) = mpsc::channel();
    let app_token = config.app_token.clone();
    thread::spawn(move || socket::run(&app_token, &events_tx));

    let api = web_api::WebApi::new(&config.bot_token, events_rx);
    let named: Vec<(String, String)> = config
        .channels
        .iter()
        .map(|c| (c.name.clone(), c.id.clone()))
        .collect();
    let threads = match Threads::open_channels(&config.state_dir, &named, api) {
        Ok(threads) => Arc::new(Mutex::new(threads)),
        Err(e) => {
            eprintln!("roc-slack-door: cannot open the thread map: {e}");
            return ExitCode::from(2);
        }
    };
    let mut inbox = match Inbox::open(&config.state_dir, &channel.id, "", channel.speakers.clone()) {
        Ok(inbox) => inbox,
        Err(e) => {
            eprintln!("roc-slack-door: cannot open the inbox: {e}");
            return ExitCode::from(2);
        }
    };

    let declared = config.channels.iter().map(|c| c.name.clone()).collect();
    let kernel = Arc::new(serve::kernel(
        Arc::clone(&threads),
        config.state_dir.join("spaces"),
        declared,
    ));
    let socket_path = config.state_dir.join("door.sock");
    println!("roc-slack-door: serving {}", socket_path.display());
    let serving = Arc::clone(&kernel);
    let serve_path = socket_path.clone();
    thread::spawn(move || {
        if let Err(e) = ikigai_ipc::serve_shared(serving, &serve_path) {
            eprintln!("roc-slack-door: the socket stopped serving: {e}");
        }
    });

    loop {
        let deliveries = {
            let mut threads = match threads.lock() {
                Ok(threads) => threads,
                Err(_) => {
                    eprintln!("roc-slack-door: the thread map is poisoned; stopping");
                    return ExitCode::from(1);
                }
            };
            let events = threads.transport_mut().take_events();
            events
                .into_iter()
                .filter_map(|event| match inbox.deliver(event, &*threads) {
                    Ok(delivery) => delivery,
                    Err(e) => {
                        eprintln!("roc-slack-door: the inbox could not record an event: {e}");
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        for delivery in &deliveries {
            if let Err(why) = serve::drop_delivery(&kernel, &channel.name, delivery) {
                eprintln!("roc-slack-door: a delivery was not written to the inbox: {why}");
            }
        }
        thread::sleep(POLL);
    }
}
