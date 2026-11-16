//! A local websocket server for the live-update tests: records what clients send, pushes what the
//! test tells it to, drops the connection on "drop".

use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

pub(crate) struct Harness {
    pub url: String,
    /// What the server received, per accepted connection (index = connection number).
    pub received: Arc<Mutex<Vec<Vec<String>>>>,
    /// Frames to push to the newest connection.
    pub push: mpsc::UnboundedSender<String>,
    pub accepted: Arc<Mutex<usize>>,
}

/// A local websocket server that records frames; every connection closes when told "drop".
pub(crate) async fn server_greeting(greeting: Option<String>) -> Harness {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let received: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let accepted = Arc::new(Mutex::new(0usize));
    let (push, mut pushed) = mpsc::unbounded_channel::<String>();
    let (rec, acc) = (received.clone(), accepted.clone());
    let greeting = greeting.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(ws) = accept_async(stream).await else {
                continue;
            };
            let index = {
                let mut n = acc.lock().unwrap();
                *n += 1;
                *n - 1
            };
            rec.lock().unwrap().push(Vec::new());
            let (mut write, mut read) = ws.split();
            if let Some(text) = &greeting {
                let _ = write.send(Message::text(text.clone())).await;
            }
            loop {
                tokio::select! {
                    msg = read.next() => match msg {
                        Some(Ok(Message::Text(t))) => rec.lock().unwrap()[index].push(t.to_string()),
                        Some(Ok(_)) => {}
                        _ => break,
                    },
                    out = pushed.recv() => match out {
                        Some(text) if text == "drop" => break,
                        Some(text) => { let _ = write.send(Message::text(text)).await; }
                        None => return,
                    },
                }
            }
        }
    });
    Harness {
        url,
        received,
        push,
        accepted,
    }
}

pub(crate) async fn server() -> Harness {
    server_greeting(None).await
}
