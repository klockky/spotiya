use crate::{
    auth, navidrome,
    source::Source,
    voice::{self, Event, Intent},
};
use librespot::core::config::SessionConfig;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    sync::{Mutex, Semaphore, mpsc},
    time::Instant,
};

pub const STATUS: &str = "/run/spotiya.status";
#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    pub active: bool,
    pub paused: bool,
    pub volume: u16,
    pub connected: bool,
    pub voice: bool,
    pub native_guard: bool,
    pub spectrum: bool,
    pub spectrum_supported: bool,
    /// The source whose transport the buttons and voice commands control right now.
    pub source: String,
    /// The source used for searches that do not name one.
    pub default_source: String,
    pub navidrome: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u32,
    pub position_ms: u32,
    pub name: String,
}
pub fn parse(line: &str) -> Option<Intent> {
    match line.trim() {
        "RESUME" => Some(Intent::Resume),
        "PAUSE" => Some(Intent::Pause),
        "TOGGLE" => Some(Intent::Toggle),
        "NEXT" => Some(Intent::Next),
        "PREVIOUS" => Some(Intent::Previous),
        "VOICE ON" => Some(Intent::Voice(true)),
        "VOICE OFF" => Some(Intent::Voice(false)),
        "SPECTRUM ON" => Some(Intent::Spectrum(true)),
        "SPECTRUM OFF" => Some(Intent::Spectrum(false)),
        "RELEASE" => Some(Intent::Release),
        s => {
            if let Some(name) = s.strip_prefix("SOURCE ") {
                return Source::parse(name).map(Intent::Source);
            }
            if let Some(n) = s.strip_prefix("VOLUME ") {
                return n
                    .parse::<u16>()
                    .ok()
                    .filter(|n| *n <= 100)
                    .map(Intent::Volume);
            }
            let query = |q: &str| {
                let q = q.trim();
                (!q.is_empty() && q.len() <= 400 && !q.contains(['\n', '\r', '\0']))
                    .then(|| q.to_owned())
            };
            if let Some(q) = s.strip_prefix("PLAY ") {
                query(q).map(|q| Intent::Play(q, None))
            } else if let Some(q) = s.strip_prefix("NAVIDROME PLAY ") {
                query(q).map(|q| Intent::Play(q, Some(Source::Navidrome)))
            } else {
                s.strip_prefix("SPOTIFY PLAY ")
                    .and_then(query)
                    .map(|q| Intent::Play(q, Some(Source::Spotify)))
            }
        }
    }
}
/// Checks the credentials against the server before storing them.
async fn navidrome_setup(json: &str, dir: &Path) -> Result<(), String> {
    let config: navidrome::Config =
        serde_json::from_str(json).map_err(|_| "Invalid Navidrome settings".to_owned())?;
    let client = navidrome::Client::new(config)?;
    client.ping().await?;
    client.config().save(dir).map_err(|e| e.to_string())
}
pub async fn serve(
    sender: mpsc::Sender<Event>,
    logins: mpsc::Sender<auth::Login>,
    config: SessionConfig,
    spectrum_supported: bool,
    state_dir: PathBuf,
) -> io::Result<()> {
    let _ = fs::remove_file(voice::CONTROL);
    let listener = UnixListener::bind(voice::CONTROL)?;
    let limit = Arc::new(Semaphore::new(8));
    let login_lock = Arc::new(Mutex::new(()));
    loop {
        let (socket, _) = listener.accept().await?;
        let Ok(permit) = limit.clone().try_acquire_owned() else {
            continue;
        };
        let sender = sender.clone();
        let logins = logins.clone();
        let config = config.clone();
        let login_lock = login_lock.clone();
        let state_dir = state_dir.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut reader = BufReader::new(socket).take(1024);
            let mut line = String::new();
            if !matches!(
                tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line)).await,
                Ok(Ok(1..))
            ) || !line.ends_with('\n')
            {
                return;
            }
            let mut socket = reader.into_inner().into_inner();
            if line.trim() == "LOGIN" {
                let (r, w) = socket.into_split();
                auth::serve(r, w, login_lock, config, logins).await;
                return;
            }
            let reply = if let Some(json) = line.trim().strip_prefix("NAVIDROME SETUP ") {
                match navidrome_setup(json, &state_dir).await {
                    Ok(()) => {
                        let _ = sender.try_send(Event {
                            intent: Intent::NavidromeReload,
                            received: Instant::now(),
                            from_voice: false,
                        });
                        "OK\n".to_owned()
                    }
                    Err(e) => format!("ERROR {}\n", e.replace(['\n', '\r'], " ")),
                }
            } else if line.trim() == "STATUS" {
                fs::read_to_string(STATUS).unwrap_or_else(|_| "ERROR Status unavailable\n".into())
            } else if let Some(intent) = parse(&line) {
                if matches!(intent, Intent::Spectrum(_)) && !spectrum_supported {
                    "ERROR Spectrum display is unavailable on this device\n".into()
                } else if sender
                    .try_send(Event {
                        intent,
                        received: Instant::now(),
                        from_voice: false,
                    })
                    .is_ok()
                {
                    "OK\n".into()
                } else {
                    "BUSY\n".into()
                }
            } else {
                "INVALID\n".into()
            };
            let _ = socket.write_all(reply.as_bytes()).await;
        });
    }
}
pub fn request(command: &str) -> io::Result<String> {
    request_within(command, Duration::from_secs(2))
}
pub fn request_within(command: &str, timeout: Duration) -> io::Result<String> {
    let mut socket = std::os::unix::net::UnixStream::connect(voice::CONTROL)?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    writeln!(socket, "{command}")?;
    let mut reply = String::new();
    io::BufReader::new(socket)
        .take(16384)
        .read_line(&mut reply)?;
    if reply.trim().is_empty()
        || reply.starts_with("ERROR")
        || matches!(reply.trim(), "BUSY" | "INVALID")
    {
        return Err(io::Error::other(reply.trim().to_owned()));
    }
    Ok(reply)
}
// The caller owns the reader, so closing it cancels a pending device login.
pub fn login() -> io::Result<io::BufReader<std::os::unix::net::UnixStream>> {
    let mut socket = std::os::unix::net::UnixStream::connect(voice::CONTROL)?;
    socket.set_read_timeout(Some(Duration::from_secs(660)))?;
    writeln!(socket, "LOGIN")?;
    Ok(io::BufReader::new(socket))
}
pub fn login_line(line: &str) -> String {
    if let Some(json) = line.strip_prefix("CODE ")
        && let Ok(code) = serde_json::from_str::<serde_json::Value>(json)
    {
        return format!(
            "Open {}\nCode: {}\nWaiting for approval... (Ctrl+C cancels)",
            code["url"].as_str().unwrap_or("https://spotify.com/pair"),
            code["code"].as_str().unwrap_or("?")
        );
    }
    line.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn accept(_: &str) -> (&'static str, Vec<u8>) {
        (
            "application/json",
            br#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#.to_vec(),
        )
    }
    fn reject(_: &str) -> (&'static str, Vec<u8>) {
        (
            "application/json",
            br#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"Wrong username or password"}}}"#.to_vec(),
        )
    }
    #[tokio::test]
    async fn setup_stores_credentials_only_after_the_server_accepts_them() {
        let dir = std::env::temp_dir().join(format!("spotiya-setup-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let config = |url: String| {
            let mut config =
                navidrome::Config::new("https://music.example", "alice", "pw").unwrap();
            config.url = url;
            serde_json::to_string(&config).unwrap()
        };
        let denied = navidrome::mock::serve(reject).await;
        assert_eq!(
            navidrome_setup(&config(denied), &dir).await.unwrap_err(),
            "Wrong username or password"
        );
        assert!(!dir.join(navidrome::CONFIG).exists());
        assert!(navidrome_setup("{\"url\":1}", &dir).await.is_err());
        assert!(
            navidrome_setup(&config("ftp://x".into()), &dir)
                .await
                .is_err()
        );
        let allowed = navidrome::mock::serve(accept).await;
        navidrome_setup(&config(allowed.clone()), &dir)
            .await
            .unwrap();
        let saved = navidrome::Config::load(&dir).unwrap();
        assert_eq!((saved.url, saved.user.as_str()), (allowed, "alice"));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn validates_local_commands_and_volume_bounds() {
        assert_eq!(parse("VOLUME 100\n"), Some(Intent::Volume(100)));
        assert_eq!(parse("VOLUME 101"), None);
        assert_eq!(parse("VOLUME -1"), None);
        assert_eq!(
            parse("PLAY no surprises\n"),
            Some(Intent::Play("no surprises".into(), None))
        );
        assert_eq!(
            parse("NAVIDROME PLAY radiohead"),
            Some(Intent::Play("radiohead".into(), Some(Source::Navidrome)))
        );
        assert_eq!(
            parse("SPOTIFY PLAY radiohead"),
            Some(Intent::Play("radiohead".into(), Some(Source::Spotify)))
        );
        assert_eq!(parse("NAVIDROME PLAY "), None);
        assert_eq!(
            parse("SOURCE NAVIDROME"),
            Some(Intent::Source(Source::Navidrome))
        );
        assert_eq!(
            parse("SOURCE spotify\n"),
            Some(Intent::Source(Source::Spotify))
        );
        assert_eq!(parse("SOURCE TIDAL"), None);
        assert_eq!(parse("PLAY a\nPAUSE"), None);
        assert_eq!(parse("PLAY "), None);
        assert_eq!(parse("VOICE OFF"), Some(Intent::Voice(false)));
        assert_eq!(parse("SPECTRUM ON"), Some(Intent::Spectrum(true)));
        assert_eq!(parse("SPECTRUM OFF"), Some(Intent::Spectrum(false)));
        assert_eq!(parse("SPECTRUM 1"), None);
    }
}
