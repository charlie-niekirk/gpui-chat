//! Lists a channel's third-party emotes per provider.
//!
//! ```text
//! cargo run -p gpui-chat-emotes --example list -- <twitch-login-or-user-id> [--global]
//! ```
//!
//! A login is resolved to a user id through `api.ivr.fi` (public, no auth); a numeric
//! argument is used as the id directly. `--global` also lists the global sets.

use std::process::ExitCode;
use std::time::Instant;

use gpui_chat_emotes::{EmoteLoader, EmoteSet, HttpClient, ProviderResults, ReqwestHttp};

const IVR_USER: &str = "https://api.ivr.fi/v2/twitch/user?login=";

#[tokio::main]
async fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let show_global = args.iter().any(|a| a == "--global");
    args.retain(|a| a != "--global");
    let [target] = args.as_slice() else {
        eprintln!("usage: list <twitch-login-or-user-id> [--global]");
        return ExitCode::from(2);
    };
    match run(target, show_global).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(target: &str, show_global: bool) -> Result<(), String> {
    let http = ReqwestHttp::new().map_err(|e| e.to_string())?;
    let id = if target.bytes().all(|b| b.is_ascii_digit()) {
        target.to_owned()
    } else {
        resolve_login(&http, target).await?
    };
    println!("channel {target} (Twitch user id {id})");

    let loader = EmoteLoader::with_default_http().map_err(|e| e.to_string())?;
    if show_global {
        let started = Instant::now();
        print_results("global", &loader.load_global().await, started);
    }
    let started = Instant::now();
    print_results("channel", &loader.load_channel(&id).await, started);
    Ok(())
}

/// `login` → Twitch user id via IVR (`[]` when the login does not exist).
async fn resolve_login(http: &ReqwestHttp, login: &str) -> Result<String, String> {
    let login = login.trim_start_matches('#').to_ascii_lowercase();
    if login.is_empty()
        || !login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(format!("{login:?} is not a valid Twitch login"));
    }
    let response = http
        .get(&format!("{IVR_USER}{login}"))
        .await
        .map_err(|e| format!("looking up {login}: {e}"))?;
    if response.status != 200 {
        return Err(format!("looking up {login}: HTTP {}", response.status));
    }
    let users: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|e| format!("looking up {login}: {e}"))?;
    users
        .get(0)
        .and_then(|u| u.get("id"))
        .and_then(|id| id.as_str())
        .map(str::to_owned)
        .ok_or_else(|| format!("no Twitch user named {login}"))
}

fn print_results(scope: &str, results: &ProviderResults, started: Instant) {
    println!("\n== {scope} emotes ({:.1?}) ==", started.elapsed());
    for (provider, result) in results.iter() {
        match result {
            Ok(set) => print_set(provider.name(), set),
            Err(error) => println!("\n{}: ERROR {error}", provider.name()),
        }
    }
}

fn print_set(name: &str, set: &EmoteSet) {
    let animated = set.iter().filter(|e| e.animated).count();
    let zero_width = set.iter().filter(|e| e.zero_width).count();
    println!(
        "\n{name}: {} emotes ({animated} animated, {zero_width} zero-width)",
        set.len()
    );
    for entry in set.emotes() {
        let e = &entry.emote;
        let flags = format!(
            "{}{}",
            if e.animated { "A" } else { "-" },
            if e.zero_width { "Z" } else { "-" }
        );
        let alias = entry
            .original_name
            .as_deref()
            .map(|o| format!(" (alias of {o})"))
            .unwrap_or_default();
        let owner = entry.owner.as_deref().unwrap_or("-");
        println!("  {flags} {}{alias}  by {owner}  id={}", e.name, e.id);
    }
}
