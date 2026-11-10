//! Spotify Web API search for the Soloist backend.
//!
//! Soloist plays only ready URIs, so a voice query ("включи radiohead") is
//! resolved to a `spotify:track:...` URI through the public Web API using
//! client credentials. The credentials live in `<state>/soloist/webapi` as
//! two lines: client id, then client secret (both from the Spotify for
//! Developers dashboard, any app registered there).
use std::{io, path::Path, sync::Mutex, time::Duration};

use tokio::time::Instant;

static TOKEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);

pub fn credentials_path(state: &Path) -> std::path::PathBuf {
    state.join("soloist").join("webapi")
}

fn credentials(state: &Path) -> io::Result<(String, String)> {
    let text = std::fs::read_to_string(credentials_path(state))?;
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let id = lines.next().unwrap_or_default();
    let secret = lines.next().unwrap_or_default();
    if id.is_empty() || secret.is_empty() {
        return Err(io::Error::other(
            "webapi: put client id and client secret on two lines in receiver-state/soloist/webapi",
        ));
    }
    Ok((id.to_owned(), secret.to_owned()))
}

fn client() -> io::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .user_agent("spotiya-webapi")
        .build()
        .map_err(io::Error::other)
}

async fn token(state: &Path) -> io::Result<String> {
    if let Ok(guard) = TOKEN.lock()
        && let Some((value, expires)) = guard.as_ref()
        && Instant::now() < *expires
    {
        return Ok(value.clone());
    }
    let (id, secret) = credentials(state)?;
    let response = client()?
        .post("https://accounts.spotify.com/api/token")
        .basic_auth(&id, Some(&secret))
        .form(&[("grant_type", "client_credentials")])
        .send()
        .await
        .map_err(io::Error::other)?;
    let status = response.status();
    let body = response.text().await.map_err(io::Error::other)?;
    if !status.is_success() {
        return Err(io::Error::other(format!("webapi: token request failed ({status})")));
    }
    let json: serde_json::Value = serde_json::from_str(&body).map_err(io::Error::other)?;
    let value = json["access_token"]
        .as_str()
        .ok_or_else(|| io::Error::other("webapi: no access_token in response"))?
        .to_owned();
    let ttl = json["expires_in"].as_u64().unwrap_or(3600).saturating_sub(60);
    if let Ok(mut guard) = TOKEN.lock() {
        *guard = Some((value.clone(), Instant::now() + Duration::from_secs(ttl)));
    }
    Ok(value)
}

/// Resolve a free-text query to the best matching track URI.
pub async fn search(state: &Path, query: &str) -> io::Result<String> {
    let access = token(state).await?;
    let response = client()?
        .get("https://api.spotify.com/v1/search")
        .bearer_auth(&access)
        .query(&[("q", query), ("type", "track"), ("limit", "1")])
        .send()
        .await
        .map_err(io::Error::other)?;
    let status = response.status();
    let body = response.text().await.map_err(io::Error::other)?;
    if status.as_u16() == 401 {
        if let Ok(mut guard) = TOKEN.lock() {
            *guard = None;
        }
    }
    if !status.is_success() {
        return Err(io::Error::other(format!("webapi: search failed ({status})")));
    }
    let json: serde_json::Value = serde_json::from_str(&body).map_err(io::Error::other)?;
    let item = &json["tracks"]["items"][0];
    let uri = item["uri"]
        .as_str()
        .ok_or_else(|| io::Error::other(format!("webapi: nothing found for \"{query}\"")))?;
    let name = item["name"].as_str().unwrap_or("");
    let artist = item["artists"][0]["name"].as_str().unwrap_or("");
    eprintln!("voice: search \"{query}\" -> {artist} - {name} ({uri})");
    Ok(uri.to_owned())
}
