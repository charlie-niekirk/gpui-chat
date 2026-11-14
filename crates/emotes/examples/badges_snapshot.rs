//! Regenerates the bundled global-badge snapshot, `assets/badges/global.json`.
//!
//! ```text
//! cargo run -p gpui-chat-emotes --example badges_snapshot              # rewrites the file
//! cargo run -p gpui-chat-emotes --example badges_snapshot -- --check   # exit 1 if it is stale
//! cargo run -p gpui-chat-emotes --example badges_snapshot -- <path>    # writes elsewhere
//! ```
//!
//! Fetches `https://api.ivr.fi/v2/twitch/badges/global` (public, no auth), keeps each badge's set,
//! version, title and the id of its image on Twitch's CDN, and writes them in the compact format
//! of `gpui_chat_emotes::badges::snapshot`. Run it when Twitch adds global badges you want to
//! see offline (the app only looks badges up by set and version; images are always loaded from
//! Twitch's CDN, never bundled). Commit the result.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use gpui_chat_emotes::badges::{ivr, snapshot};
use gpui_chat_emotes::{HttpClient, ReqwestHttp};

/// Where the snapshot lives, relative to this crate.
const DEFAULT_PATH: &str = "../../assets/badges/global.json";

#[tokio::main]
async fn main() -> ExitCode {
    let mut check = false;
    let mut path = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--check" => check = true,
            other if !other.starts_with('-') && path.is_none() => path = Some(PathBuf::from(other)),
            _ => {
                eprintln!("usage: badges_snapshot [--check] [path]");
                return ExitCode::from(2);
            }
        }
    }
    let path = path.unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_PATH));
    match run(&path, check).await {
        Ok(code) => code,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(path: &Path, check: bool) -> Result<ExitCode, String> {
    let http = ReqwestHttp::new().map_err(|e| e.to_string())?;
    let url = ivr::global_url(ivr::BASE_URL);
    let response = http.get(&url).await.map_err(|e| format!("{url}: {e}"))?;
    if !(200..300).contains(&response.status) {
        return Err(format!("{url}: HTTP {}", response.status));
    }
    let set = ivr::parse(&response.body).map_err(|e| e.to_string())?;
    if set.is_empty() {
        return Err("the API returned no badges; refusing to write an empty snapshot".into());
    }
    let json = snapshot::encode(&set).map_err(|e| e.to_string())? + "\n";
    if check {
        let current = std::fs::read_to_string(path).unwrap_or_default();
        return if current == json {
            println!("{} is up to date ({} badges)", path.display(), set.len());
            Ok(ExitCode::SUCCESS)
        } else {
            println!("{} is stale: regenerate it", path.display());
            Ok(ExitCode::FAILURE)
        };
    }
    std::fs::write(path, &json).map_err(|e| format!("{}: {e}", path.display()))?;
    println!(
        "wrote {} badges ({} bytes) to {}",
        set.len(),
        json.len(),
        path.display()
    );
    Ok(ExitCode::SUCCESS)
}
