use crate::{
    auth,
    voice::{self, Event, Intent},
};
use librespot::core::config::SessionConfig;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, BufRead, Read, Write},
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
    pub spotify_backend: String,
    pub backend_error: String,
    pub soloist_available: bool,
    pub airplay_available: bool,
    pub airplay_enabled: bool,
    pub airplay_running: bool,
    pub airplay_active: bool,
    pub airplay_error: String,
    pub youtube_available: bool,
    pub youtube_enabled: bool,
    pub youtube_running: bool,
    pub youtube_active: bool,
    pub youtube_error: String,
    pub paused: bool,
    pub loading: bool,
    pub error: String,
    pub retry_required: bool,
    /// "credentials" or "premium" when Spotify refused the login and retries stopped.
    pub login_refused: String,
    pub retry_after_secs: u32,
    pub failed_tracks: u8,
    pub volume: u16,
    pub connected: bool,
    pub voice: bool,
    pub voice_supported: bool,
    pub profile: String,
    pub profile_selection: String,
    pub receiver_pid: u32,
    pub native_guard: bool,
    pub ducking: bool,
    pub spectrum: bool,
    pub spectrum_supported: bool,
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
        "AIRPLAY ON" => Some(Intent::Airplay(true)),
        "AIRPLAY OFF" => Some(Intent::Airplay(false)),
        "YOUTUBE ON" => Some(Intent::Youtube(true)),
        "YOUTUBE OFF" => Some(Intent::Youtube(false)),
        "RETRY" => Some(Intent::Retry),
        "RELEASE" => Some(Intent::Release),
        s => {
            if let Some(n) = s.strip_prefix("VOLUME ") {
                return n
                    .parse::<u16>()
                    .ok()
                    .filter(|n| *n <= 100)
                    .map(Intent::Volume);
            }
            s.strip_prefix("PLAY ")
                .map(str::trim)
                .filter(|q| !q.is_empty() && q.len() <= 400 && !q.contains(['\n', '\r', '\0']))
                .map(|q| Intent::Play(q.to_owned()))
        }
    }
}
pub async fn serve(
    sender: mpsc::Sender<Event>,
    logins: mpsc::Sender<auth::Login>,
    config: Option<SessionConfig>,
    capabilities: crate::profiles::Capabilities,
    airplay: mpsc::Sender<crate::airplay::Request>,
    youtube: mpsc::Sender<crate::youtube::Request>,
    spectrum: Arc<crate::spectrum::Spectrum>,
) -> io::Result<()> {
    let _ = fs::remove_file(voice::CONTROL);
    let listener = UnixListener::bind(voice::CONTROL)?;
    let limit = Arc::new(Semaphore::new(8));
    let login_lock = Arc::new(Mutex::new(()));
    let mut failing = false;
    loop {
        let socket = match listener.accept().await {
            Ok((socket, _)) => socket,
            Err(e) => {
                // Descriptor exhaustion and similar errors are transient; keep the socket alive.
                if !std::mem::replace(&mut failing, true) {
                    eprintln!("voice: control socket accept failed: {e}");
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
        };
        failing = false;
        let Ok(permit) = limit.clone().try_acquire_owned() else {
            continue;
        };
        let sender = sender.clone();
        let logins = logins.clone();
        let config = config.clone();
        let airplay = airplay.clone();
        let youtube = youtube.clone();
        let login_lock = login_lock.clone();
        let spectrum = spectrum.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut reader = BufReader::new(socket).take(512);
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
                let Some(config) = config else {
                    let _=socket.write_all(b"ERROR Soloist uses its own API key and Spotify Connect pairing; use spotiya soloist key, then select the device in Spotify\n").await;
                    return;
                };
                let (r, w) = socket.into_split();
                auth::serve(r, w, login_lock, config, logins).await;
                return;
            }
            let reply = if let Some((action, token)) = crate::airplay::parse(&line) {
                let (reply, result) = tokio::sync::oneshot::channel();
                if airplay
                    .try_send(crate::airplay::Request {
                        action,
                        token,
                        reply,
                    })
                    .is_ok()
                    && matches!(
                        tokio::time::timeout(Duration::from_secs(4), result).await,
                        Ok(Ok(true))
                    )
                {
                    "OK\n".into()
                } else {
                    "ERROR AirPlay handover failed\n".into()
                }
            } else if let Some((action, token)) = crate::youtube::parse(&line) {
                youtube_request(&youtube, action, token).await
            } else if let Some(rest) = line.trim().strip_prefix("YOUTUBE URL ") {
                match crate::youtube::video_id(rest) {
                    Some(id) => youtube_play(&youtube, id).await,
                    None => "ERROR Not a YouTube link\n".into(),
                }
            } else if let Some(id) = line
                .trim()
                .strip_prefix("PLAY ")
                .and_then(crate::youtube::link)
            {
                // A YouTube link typed into the search goes to YouTube, not Spotify search.
                youtube_play(&youtube, id).await
            } else if line.trim() == "YOUTUBE STOP" {
                youtube_request(
                    &youtube,
                    crate::youtube::Action::Control("STOP"),
                    String::new(),
                )
                .await
            } else if line.trim() == "LEVELS" {
                levels(spectrum.frame())
            } else if line.trim() == "STATUS" {
                fs::read_to_string(STATUS).unwrap_or_else(|_| "ERROR Status unavailable\n".into())
            } else if let Some(intent) = parse(&line) {
                let status = fs::read_to_string(STATUS)
                    .ok()
                    .and_then(|s| serde_json::from_str::<Status>(&s).ok())
                    .unwrap_or_default();
                if matches!(intent, Intent::Airplay(true))
                    && !status.airplay_available
                    && !crate::addons::airplay_available(std::path::Path::new(
                        crate::installation::BASE,
                    ))
                {
                    "ERROR Install the optional AirPlay 2 package first\n".into()
                } else if matches!(intent, Intent::Youtube(true))
                    && !status.youtube_available
                    && !crate::addons::youtube_available(std::path::Path::new(
                        crate::installation::BASE,
                    ))
                {
                    "ERROR Install the optional YouTube package first\n".into()
                } else if matches!(intent, Intent::Retry)
                    && status.spotify_backend != "soloist"
                    && (!status.retry_required || status.retry_after_secs > 0)
                {
                    format!(
                        "ERROR Retry is unavailable; wait {} seconds or check status\n",
                        status.retry_after_secs
                    )
                } else if status.retry_required
                    && matches!(
                        intent,
                        Intent::Play(_)
                            | Intent::Resume
                            | Intent::Toggle
                            | Intent::Next
                            | Intent::Previous
                    )
                {
                    "ERROR Playback stopped after repeated failures; use retry\n".into()
                } else if matches!(intent, Intent::Voice(_)) && !capabilities.voice {
                    "ERROR Voice control is unavailable in this device profile\n".into()
                } else if matches!(intent, Intent::Spectrum(_)) && !capabilities.display {
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
/// Plays a video, or explains why it cannot when the add-on is not running.
async fn youtube_play(youtube: &mpsc::Sender<crate::youtube::Request>, id: String) -> String {
    let status = fs::read_to_string(STATUS)
        .ok()
        .and_then(|s| serde_json::from_str::<Status>(&s).ok())
        .unwrap_or_default();
    if !status.youtube_available {
        return "ERROR Install the optional YouTube package to play YouTube links\n".into();
    }
    if !status.youtube_enabled {
        return "ERROR Turn on the YouTube add-on to play YouTube links\n".into();
    }
    youtube_request(youtube, crate::youtube::Action::Play(id), String::new()).await
}
/// Sends a YouTube handover or playback request to the receiver loop and waits
/// for its verdict. Returns the control-socket reply line.
async fn youtube_request(
    youtube: &mpsc::Sender<crate::youtube::Request>,
    action: crate::youtube::Action,
    token: String,
) -> String {
    let (reply, result) = tokio::sync::oneshot::channel();
    if youtube
        .try_send(crate::youtube::Request {
            action,
            token,
            reply,
        })
        .is_ok()
        && matches!(
            tokio::time::timeout(Duration::from_secs(5), result).await,
            Ok(Ok(true))
        )
    {
        "OK\n".into()
    } else {
        "ERROR YouTube command failed\n".into()
    }
}
/// The live spectrum for the TUI graph; `-` while it is off or no audio is playing.
fn levels(frame: Option<[u8; 4]>) -> String {
    frame.map_or_else(
        || "LEVELS -\n".into(),
        |[a, b, c, d]| format!("LEVELS {a} {b} {c} {d}\n"),
    )
}
/// Parses a `LEVELS` reply; `None` when no spectrum is available.
pub fn parse_levels(reply: &str) -> Option<[u8; 4]> {
    let mut values = reply.trim().strip_prefix("LEVELS ")?.split(' ');
    let mut levels = [0; 4];
    for level in &mut levels {
        *level = values.next()?.parse::<u8>().ok()?.min(100);
    }
    values.next().is_none().then_some(levels)
}
pub fn request(command: &str) -> io::Result<String> {
    let mut socket = std::os::unix::net::UnixStream::connect(voice::CONTROL)?;
    socket.set_read_timeout(Some(Duration::from_secs(2)))?;
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
    #[test]
    fn validates_local_commands_and_volume_bounds() {
        assert_eq!(parse("VOLUME 100\n"), Some(Intent::Volume(100)));
        assert_eq!(parse("VOLUME 101"), None);
        assert_eq!(parse("VOLUME -1"), None);
        assert_eq!(
            parse("PLAY no surprises\n"),
            Some(Intent::Play("no surprises".into()))
        );
        assert_eq!(parse("PLAY a\nPAUSE"), None);
        assert_eq!(parse("PLAY "), None);
        assert_eq!(parse("VOICE OFF"), Some(Intent::Voice(false)));
        assert_eq!(parse("SPECTRUM ON"), Some(Intent::Spectrum(true)));
        assert_eq!(parse("SPECTRUM OFF"), Some(Intent::Spectrum(false)));
        assert_eq!(parse("SPECTRUM 1"), None);
        assert_eq!(parse("RETRY"), Some(Intent::Retry));
        assert_eq!(parse("RETRY extra"), None);
    }
    #[test]
    fn spectrum_levels_round_trip_and_reject_malformed_replies() {
        assert_eq!(
            parse_levels(&levels(Some([0, 40, 100, 7]))),
            Some([0, 40, 100, 7])
        );
        assert_eq!(parse_levels(&levels(None)), None);
        assert_eq!(parse_levels("LEVELS 1 2 3"), None);
        assert_eq!(parse_levels("LEVELS 1 2 3 4 5"), None);
        assert_eq!(parse_levels("LEVELS 1 2 3 x"), None);
        assert_eq!(parse_levels("LEVELS 1 2 3 255"), Some([1, 2, 3, 100]));
    }
}
