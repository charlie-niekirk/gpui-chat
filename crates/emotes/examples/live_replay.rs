//! A local stand-in for 7TV's EventAPI and BTTV's socket that replays a script of frames, to see
//! live emote updates in the app without waiting for an editor to change a set.
//!
//! ```text
//! cargo run -p gpui-chat-emotes --example live_replay -- fixtures/emotes/live-demo.jsonl
//! GPUI_CHAT_7TV_EVENTS_URL=ws://127.0.0.1:7071 GPUI_CHAT_BTTV_SOCKET_URL=ws://127.0.0.1:7072 \
//!     cargo run -p gpui-chat
//! ```
//!
//! Each script line is `{"at_ms": 4000, "to": "7tv" | "bttv", "frame": {...}}`: `frame` is sent
//! `at_ms` after the app's *first subscription* to that socket. In 7TV frames the string `$SET` is
//! replaced by the emote set id the app subscribed to and `$USER` by the 7TV user id it listens
//! to for set switches, in BTTV frames `$ROOM` by the room id it joined. The 7TV server says `Hello` as the real one does. Ctrl-C stops it.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

const SEVEN_TV: &str = "127.0.0.1:7071";
const BTTV: &str = "127.0.0.1:7072";

struct Step {
    at: Duration,
    to: String,
    frame: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: live_replay <script.jsonl>")?;
    let mut steps = Vec::new();
    for line in std::fs::read_to_string(path)?
        .lines()
        .filter(|l| !l.trim().is_empty())
    {
        let value: Value = serde_json::from_str(line)?;
        steps.push(Step {
            at: Duration::from_millis(value["at_ms"].as_u64().unwrap_or(0)),
            to: value["to"].as_str().unwrap_or("").to_owned(),
            frame: value["frame"].to_string(),
        });
    }
    let steps = std::sync::Arc::new(steps);
    let seven = serve(SEVEN_TV, "7tv", steps.clone());
    let bttv = serve(BTTV, "bttv", steps);
    println!("7TV EventAPI on ws://{SEVEN_TV}, BTTV on ws://{BTTV}");
    tokio::try_join!(seven, bttv)?;
    Ok(())
}

async fn serve(
    addr: &'static str,
    name: &'static str,
    steps: std::sync::Arc<Vec<Step>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(addr).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let steps = steps.clone();
        tokio::spawn(async move {
            let Ok(ws) = accept_async(stream).await else {
                return;
            };
            let (mut write, mut read) = ws.split();
            if name == "7tv" {
                let hello = r#"{"op":1,"d":{"heartbeat_interval":45000,"session_id":"replay"}}"#;
                let _ = write.send(Message::text(hello)).await;
            }
            let mut started = false;
            let user = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            loop {
                tokio::select! {
                    message = read.next() => {
                        let Some(Ok(Message::Text(text))) = message else {
                            if matches!(message, Some(Ok(_))) { continue } else { return }
                        };
                        println!("[{name}] <- {text}");
                        let Ok(frame) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                        if name == "7tv"
                            && frame["d"]["type"] == "user.update"
                            && let Some(id) = frame["d"]["condition"]["object_id"].as_str()
                        {
                            *user.lock().unwrap() = id.to_owned();
                        }
                        // The first subscription starts the script.
                        let id = if name == "7tv" {
                            (frame["op"] == 35 && frame["d"]["type"] == "emote_set.update")
                                .then(|| frame["d"]["condition"]["object_id"].as_str().map(str::to_owned))
                                .flatten()
                        } else {
                            (frame["name"] == "join_channel").then(|| {
                                frame["data"]["name"].as_str().and_then(|n| n.strip_prefix("twitch:")).map(str::to_owned)
                            }).flatten()
                        };
                        if let Some(id) = id && !started {
                            started = true;
                            for step in steps.iter().filter(|s| s.to == name) {
                                let (tx, at) = (tx.clone(), step.at);
                                let frame = step.frame.replace("$SET", &id).replace("$ROOM", &id);
                                let user = user.clone();
                                tokio::spawn(async move {
                                    tokio::time::sleep(at).await;
                                    let frame = frame.replace("$USER", &user.lock().unwrap());
                                    let _ = tx.send(frame);
                                });
                            }
                        }
                    }
                    Some(frame) = rx.recv() => {
                        println!("[{name}] -> {frame}");
                        if write.send(Message::text(frame)).await.is_err() { return }
                    }
                }
            }
        });
    }
}
