//! Prints live chat for a channel using the real client.
//!
//! ```text
//! cargo run -p gpui-chat-twitch --example dump -- <channel> [--seconds N] [--raw]
//! ```
//!
//! * default: one line per event (`login: text`, then a fragment summary).
//! * `--raw`: print every line received (`< ...`) and sent (`> ...`) instead, the
//!   source of the lines under `fixtures/irc/`. Tags come out unescaped-then-escaped
//!   and sorted by the IRC library, which is fine for fixtures.
//! * `--seconds N`: stop after N seconds (default: run until interrupted).

use std::time::Duration;

use gpui_chat_core::{ChatEvent, Fragment, Message};
use gpui_chat_twitch::{ClientConfig, TwitchClient};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let mut channel = None;
    let mut seconds = None;
    let mut raw = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--raw" => raw = true,
            "--seconds" => seconds = args.next().and_then(|v| v.parse::<u64>().ok()),
            _ if channel.is_none() => channel = Some(arg),
            _ => usage(),
        }
    }
    let Some(channel) = channel else { usage() };

    if raw {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("gpui_chat_twitch::raw=trace"))
            .with_writer(std::io::stdout)
            .with_ansi(false)
            .with_level(false)
            .with_target(false)
            .without_time()
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("gpui_chat_twitch=info")),
            )
            .with_writer(std::io::stderr)
            .init();
    }

    let channel = channel.as_str().into();
    let (client, mut events) = TwitchClient::spawn(ClientConfig::default());
    client.join(&channel);

    let deadline = async {
        match seconds {
            Some(s) => tokio::time::sleep(Duration::from_secs(s)).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            () = &mut deadline => break,
            event = events.recv() => {
                let Some(event) = event else { break };
                if !raw {
                    print_event(&event);
                }
            }
        }
    }
    client.shutdown();
}

fn usage() -> ! {
    eprintln!("usage: dump <channel> [--seconds N] [--raw]");
    std::process::exit(2);
}

fn print_event(event: &ChatEvent) {
    match event {
        ChatEvent::Message { channel, message } => {
            let m: &Message = message;
            let who = m.author.as_ref().map_or("-", |a| a.login.as_str());
            let colour = m
                .author
                .as_ref()
                .and_then(|a| a.color)
                .map_or_else(|| "none".to_owned(), |c| c.to_hex());
            let sep = if m.kind == gpui_chat_core::MessageKind::Action {
                "*"
            } else {
                ":"
            };
            println!(
                "#{channel} [{:?}] {who} ({colour}){sep} {}",
                m.kind,
                m.plain_text()
            );
            if let Some(sys) = &m.system_text {
                println!("    system: {sys}");
            }
            println!("    {}", summarize(&m.fragments));
        }
        other => println!("{other:?}"),
    }
}

fn summarize(fragments: &[Fragment]) -> String {
    let mut parts = Vec::new();
    let mut words = 0;
    for fragment in fragments {
        match fragment {
            Fragment::Text(t) => words += t.split_whitespace().count(),
            Fragment::Emote { emote, .. } => parts.push(format!("emote({})", emote.name)),
            Fragment::Emoji { text } => parts.push(format!("emoji({text})")),
            Fragment::Mention { login, .. } => parts.push(format!("mention({login})")),
            Fragment::Link { url, .. } => parts.push(format!("link({url})")),
            Fragment::Cheer { prefix, bits } => parts.push(format!("cheer({prefix}{bits})")),
        }
    }
    format!("fragments: {words} words; {}", parts.join(" "))
}
