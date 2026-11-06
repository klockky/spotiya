//! Official Soloist stays a separate, user-downloaded process controlled through
//! its loopback WebSocket API. Our apulse sends optional spectrum levels locally;
//! no librespot session or key fallback is involved.
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::{
    process::{Child, Command},
    sync::{mpsc, watch},
    time::Instant,
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

const LEVELS: &str = "/run/spotiya-soloist-levels";

pub fn receive_levels(
    gate: Arc<crate::airplay::Gate>,
    spectrum: Arc<crate::spectrum::Spectrum>,
) -> io::Result<()> {
    match fs::remove_file(LEVELS) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let socket = tokio::net::UnixDatagram::bind(LEVELS)?;
    fs::set_permissions(LEVELS, fs::Permissions::from_mode(0o600))?;
    tokio::spawn(async move {
        let mut packet = [0; 5]; // A fifth byte distinguishes oversized datagrams.
        loop {
            match socket.recv(&mut packet).await {
                Ok(n) => publish_levels(&packet[..n], &gate, &spectrum),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    eprintln!("soloist: spectrum socket failed: {e}");
                    break;
                }
            }
        }
    });
    Ok(())
}

fn publish_levels(
    packet: &[u8],
    gate: &crate::airplay::Gate,
    spectrum: &crate::spectrum::Spectrum,
) {
    if let Ok(levels) = <[u8; 4]>::try_from(packet)
        && levels.iter().all(|&level| level <= 100)
        && gate.spotify_open.load(Ordering::Acquire)
        && !gate.active.load(Ordering::Acquire)
        && spectrum.enabled()
    {
        spectrum.publish(levels);
    }
}

#[derive(Clone, Debug, Default)]
pub struct State {
    pub connected: bool,
    pub active: bool,
    pub status: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub uri: String,
    pub position_ms: u32,
    pub duration_ms: u32,
    pub volume: Option<u16>,
    pub volume_revision: u64,
    pub position_at: Option<Instant>,
    pub error: String,
}
impl State {
    fn apply(&mut self, event: &Value) -> bool {
        match event["type"].as_str().unwrap_or("") {
            "auth_state" => {
                self.connected = event["logged_in"].as_bool().unwrap_or(false);
                self.active = self.connected && event["is_active"].as_bool().unwrap_or(false);
                if !self.connected {
                    self.status = "idle".into();
                    self.clear_track();
                }
                self.error.clear();
            }
            "device_changed" => self.active = event["is_active"].as_bool().unwrap_or(false),
            "playback_state" => {
                self.active = event["is_active"].as_bool().unwrap_or(false);
                self.status = event["status"].as_str().unwrap_or("idle").into();
                self.track(&event["item"]);
                self.position_ms = number(&event["position"]["position_ms"]);
                self.position_at = Some(Instant::now());
                if let Some(v) = volume(&event["volume"]) {
                    self.volume = Some(v);
                    self.volume_revision = self.volume_revision.wrapping_add(1);
                }
                self.error.clear();
            }
            "playback_changed" => {
                self.position_ms = self.position();
                self.position_at = Some(Instant::now());
                self.status = event["status"].as_str().unwrap_or("idle").into();
            }
            "track_changed" => {
                self.track(&event["item"]);
                self.position_ms = 0;
                self.position_at = Some(Instant::now());
            }
            "position_sync" => {
                self.position_ms = number(&event["position"]["position_ms"]);
                self.position_at = Some(Instant::now());
            }
            "volume_changed" => {
                if let Some(v) = volume(&event["volume"]) {
                    self.volume = Some(v);
                    self.volume_revision = self.volume_revision.wrapping_add(1);
                }
            }
            "error" => self.error = format!("Soloist: {}", clean(&event["message"])),
            _ => return false,
        }
        true
    }
    fn clear_track(&mut self) {
        self.title.clear();
        self.artist.clear();
        self.album.clear();
        self.uri.clear();
        self.position_ms = 0;
        self.duration_ms = 0;
    }
    fn track(&mut self, item: &Value) {
        self.uri = clean(&item["uri"]);
        let d = &item["decorations"];
        self.title = clean(&d["identity"]["name"]);
        self.album = clean(&d["parent"]["entity"]["decorations"]["identity"]["name"]);
        self.artist = d["creators"]
            .as_array()
            .map(|creators| {
                creators
                    .iter()
                    .take(16)
                    .map(|c| clean(&c["entity"]["decorations"]["identity"]["name"]))
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        self.duration_ms = number(&d["playback"]["duration_ms"]);
    }
    pub fn position(&self) -> u32 {
        let elapsed = if self.playing() {
            self.position_at.map_or(0, |at| {
                at.elapsed().as_millis().min(u128::from(u32::MAX)) as u32
            })
        } else {
            0
        };
        self.position_ms.saturating_add(elapsed)
    }
    pub fn playing(&self) -> bool {
        self.connected && self.active && self.status == "playing"
    }
}
fn clean(v: &Value) -> String {
    v.as_str()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(400)
        .collect()
}
fn number(v: &Value) -> u32 {
    v.as_u64().unwrap_or(0).min(u64::from(u32::MAX)) as u32
}
fn volume(v: &Value) -> Option<u16> {
    v.as_u64().filter(|v| *v <= 100).map(|v| v as u16)
}
struct Request {
    commands: Vec<Value>,
    deadline: Instant,
}
pub struct Handle {
    tx: mpsc::Sender<Request>,
    stop: watch::Sender<bool>,
    state: watch::Receiver<State>,
}
impl Handle {
    fn send(&self, commands: Vec<Value>) -> io::Result<()> {
        if !self.state.borrow().connected {
            return Err(io::Error::other("Soloist is not logged in"));
        }
        self.tx
            .try_send(Request {
                commands,
                deadline: Instant::now() + Duration::from_millis(350),
            })
            .map_err(|_| io::Error::other("Soloist control unavailable or busy"))
    }
    pub fn command(&self, command: &str) -> io::Result<()> {
        self.send(vec![wire(command)])
    }
    pub fn toggle(&self) -> io::Result<()> {
        self.command(if self.state.borrow().playing() {
            "pause"
        } else {
            "play"
        })
    }
    pub fn release(&self) -> io::Result<()> {
        self.send(vec![wire("pause"), wire("deactivate")])
    }
    pub fn shutdown(&self) -> io::Result<()> {
        self.stop.send(true).map_err(io::Error::other)
    }
    pub fn volume(&self, volume: u16) -> io::Result<()> {
        self.send(vec![
            json!({"type":"command","command":"set_volume","volume":volume.min(100)}),
        ])
    }
    pub fn play_uri(&self, uri: String) -> io::Result<()> {
        // Search URIs are not part of Soloist's public API. Never silently fall
        // back to librespot or turn a voice search into a different track.
        if !playable(&uri) {
            return Err(io::Error::other(
                "Soloist accepts Spotify track/album/playlist/episode URIs; text search is not supported by its public API",
            ));
        }
        self.send(vec![json!({"type":"command","command":"play","uri":uri})])
    }
}
pub fn playable(uri: &str) -> bool {
    let parts: Vec<_> = uri.split(':').collect();
    parts.len() == 3
        && parts[0] == "spotify"
        && matches!(parts[1], "track" | "album" | "playlist" | "episode")
        && parts[2].len() == 22
        && parts[2].bytes().all(|c| c.is_ascii_alphanumeric())
}
fn wire(command: &str) -> Value {
    json!({"type":"command", "command":command})
}

pub fn save_key(state: &Path) -> io::Result<()> {
    use std::io::IsTerminal;
    if io::stdin().is_terminal() {
        return Err(io::Error::other(
            "Read the key privately into stdin: spotiya soloist key < /path/to/private-key-file",
        ));
    }
    let mut key = String::new();
    io::stdin().take(4097).read_to_string(&mut key)?;
    store_key(state, &key)
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 4096 && key.bytes().all(|c| c.is_ascii_graphic())
}

pub fn key_required(state: &Path) -> bool {
    let mut key = String::new();
    fs::File::open(state.join("soloist/api-key"))
        .and_then(|file| file.take(4097).read_to_string(&mut key))
        .is_err()
        || !valid_key(key.trim())
}

pub fn prompt_key_if_missing(state: &Path) -> io::Result<()> {
    if !key_required(state) {
        return Ok(());
    }
    if fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .is_err()
    {
        println!(
            "Soloist needs an API key. Open a terminal and run /data/opt/solo/set-key.sh, then spotiya retry."
        );
        return Ok(());
    }
    println!("Create a personal key at https://developer.spotify.com/dashboard/soloist");
    let result = std::process::Command::new("sh")
        .arg("/data/opt/solo/set-key.sh")
        .env("SPOTIYA_STATE_DIR", state)
        .status()?;
    if !result.success() {
        println!(
            "Soloist is installed; key setup was not completed. Run /data/opt/solo/set-key.sh to retry."
        );
    }
    Ok(())
}

pub fn store_key(state: &Path, key: &str) -> io::Result<()> {
    let key = key.trim();
    if !valid_key(key) {
        return Err(io::Error::other("Invalid Soloist API key"));
    }
    let dir = state.join("soloist");
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let temp = dir.join(format!(".key-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        writeln!(f, "{key}")?;
        f.sync_all()?;
        fs::rename(&temp, dir.join("api-key"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
pub fn spawn(
    base: PathBuf,
    state: PathBuf,
    name: String,
    gate: Arc<crate::airplay::Gate>,
) -> (Handle, tokio::task::JoinHandle<()>, watch::Receiver<State>) {
    let (tx, rx) = mpsc::channel(16);
    let (stop_tx, stop_rx) = watch::channel(false);
    let (events, state_rx) = watch::channel(State::default());
    let handle = Handle {
        tx,
        stop: stop_tx,
        state: state_rx.clone(),
    };
    let task = tokio::spawn(async move {
        let result = worker(&base, &state, &name, rx, stop_rx, &events, &gate).await;
        gate.close_spotify();
        let error = result
            .err()
            .map(|e| format!("Soloist: {e}"))
            .unwrap_or_default();
        if !error.is_empty() {
            eprintln!("receiver: {error}");
        }
        events.send_replace(State {
            error,
            ..Default::default()
        });
    });
    (handle, task, state_rx)
}
async fn stop_child(child: &mut Child) {
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    if tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}
async fn worker(
    _base: &Path,
    state_dir: &Path,
    name: &str,
    mut commands: mpsc::Receiver<Request>,
    mut stop: watch::Receiver<bool>,
    events: &watch::Sender<State>,
    gate: &crate::airplay::Gate,
) -> io::Result<()> {
    let dir = state_dir.join("soloist");
    let key = fs::read_to_string(dir.join("api-key")).map_err(|_| {
        io::Error::other(
            "API key missing; use spotiya soloist key with a key from an active Premium account",
        )
    })?;
    let key = key.trim();
    if key.is_empty() || key.len() > 4096 || key.chars().any(char::is_whitespace) {
        return Err(io::Error::other("Invalid API key file"));
    }
    let launcher = fs::read_to_string(dir.join("launcher"))
        .map(|s| PathBuf::from(s.trim()))
        .unwrap_or_else(|_| PathBuf::from(crate::soloist_install::ROOT).join("run.sh"));
    if !launcher.is_absolute() || !launcher.is_file() {
        return Err(io::Error::other(
            "Soloist launcher missing; install the official runtime first",
        ));
    }
    let data = dir.join("data");
    let cache = dir.join("cache");
    fs::create_dir_all(&data)?;
    fs::create_dir_all(&cache)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    for file in ["ws.addr", "ws.port"] {
        match fs::remove_file(data.join(file)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    let log = Path::new("/tmp/spotiya-soloist.log");
    let output = crate::supervisor::open_log(log)?;
    fs::set_permissions(log, fs::Permissions::from_mode(0o600))?;
    let parent = std::process::id();
    let mut cmd = Command::new(launcher);
    cmd.args([
        "--device-name",
        name,
        "--api-key",
        key,
        "--cache-size",
        "100",
        "--ws",
        "127.0.0.1:0",
    ])
    .arg("--data-dir")
    .arg(&data)
    .arg("--cache-dir")
    .arg(&cache)
    .env("MALLOC_ARENA_MAX", "1")
    .env("SPOTIYA_SOLOIST_LEVELS", LEVELS)
    .env(
        "SPOTIYA_SPECTRUM_SETTING",
        state_dir.join("spectrum-enabled"),
    )
    .stdin(Stdio::null())
    .stdout(output.try_clone()?)
    .stderr(output)
    .kill_on_drop(true);
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() as u32 != parent {
                return Err(io::Error::other("Receiver exited"));
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let result=async {
        let deadline=Instant::now()+Duration::from_secs(15);
        let port=loop {
            if *stop.borrow() {return Ok(());}
            if let Some(status)=child.try_wait()? {return Err(exit_error(status.code()));}
            if let Ok(text)=fs::read_to_string(data.join("ws.port")) && let Ok(port)=text.trim().parse::<u16>() && port!=0 {break port;}
            if Instant::now()>=deadline {return Err(io::Error::other("Local API did not start in 15 seconds"));}
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let ws_config=WebSocketConfig::default().max_message_size(Some(1024*1024)).max_frame_size(Some(1024*1024));
        let (mut socket,_)=tokio::time::timeout(Duration::from_secs(5),connect_async_with_config(format!("ws://127.0.0.1:{port}"),Some(ws_config),false)).await.map_err(io::Error::other)?.map_err(io::Error::other)?;
        let mut state=State::default(); let mut tick=tokio::time::interval(Duration::from_secs(10)); let mut seen=Instant::now();let mut trimmed=Instant::now();
        loop {
            tokio::select! {
                _=stop.changed()=>return Ok(()),
                status=child.wait()=>return Err(exit_error(status?.code())),
                request=commands.recv()=>{
                    let Some(request)=request else {return Ok(());};
                    if Instant::now()>=request.deadline {continue;}
                    for value in request.commands {
                        tokio::time::timeout_at(request.deadline,socket.send(Message::Text(value.to_string().into()))).await.map_err(io::Error::other)?.map_err(io::Error::other)?;
                    }
                },
                message=socket.next()=>{
                    let message=message.ok_or_else(||io::Error::other("Local API disconnected"))?.map_err(io::Error::other)?; seen=Instant::now();
                    if let Message::Text(text)=message {
                        let event:Value=serde_json::from_str(&text).map_err(io::Error::other)?;
                        if state.apply(&event) {
                            if state.playing() {gate.spotify_open.store(true,Ordering::Release);} else {gate.close_spotify();}
                            events.send_replace(state.clone());
                        }
                    } else if matches!(message,Message::Close(_)) {return Err(io::Error::other("Local API closed"));}
                },
                _=tick.tick()=>{
                    if seen.elapsed()>Duration::from_secs(30) {return Err(io::Error::other("Local API timed out"));}
                    tokio::time::timeout(Duration::from_secs(2),socket.send(Message::Ping(Vec::new().into()))).await.map_err(io::Error::other)?.map_err(io::Error::other)?;
                    if trimmed.elapsed()>=Duration::from_secs(30) {let _=crate::supervisor::trim_log(log);trimmed=Instant::now();}
                }
            }
        }
    }.await;
    stop_child(&mut child).await;
    result
}
fn exit_error(code: Option<i32>) -> io::Error {
    io::Error::other(if code == Some(10) {
        "Build expired; download a new official Soloist build".into()
    } else {
        format!("Process exited ({code:?}); see /tmp/spotiya-soloist.log")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn spectrum_accepts_only_valid_levels_from_active_spotify() {
        let gate = crate::airplay::Gate::new();
        let spectrum = crate::spectrum::Spectrum::new(true);
        publish_levels(&[10, 20, 30, 40], &gate, &spectrum);
        assert!(spectrum.frame().is_none());
        gate.spotify_open.store(true, Ordering::Release);
        for invalid in [&[1, 2, 3][..], &[1, 2, 3, 4, 5], &[101, 2, 3, 4]] {
            publish_levels(invalid, &gate, &spectrum);
            assert!(spectrum.frame().is_none());
        }
        publish_levels(&[10, 20, 30, 40], &gate, &spectrum);
        assert_eq!(spectrum.frame(), Some([10, 20, 30, 40]));
        spectrum.set_enabled(false);
        publish_levels(&[50; 4], &gate, &spectrum);
        assert!(spectrum.frame().is_none());
        spectrum.set_enabled(true);
        gate.active.store(true, Ordering::Release);
        publish_levels(&[50; 4], &gate, &spectrum);
        assert!(spectrum.frame().is_none());
    }
    #[test]
    fn login_loading_pause_and_logout_do_not_claim_early_playback() {
        let mut s = State::default();
        s.apply(&json!({"type":"auth_state","logged_in":true,"is_active":true}));
        assert!(!s.playing());
        s.apply(&json!({"type":"playback_state","status":"buffering","is_active":true}));
        assert!(!s.playing());
        s.apply(&json!({"type":"playback_changed","status":"playing"}));
        assert!(s.playing());
        s.apply(&json!({"type":"playback_changed","status":"paused"}));
        assert!(!s.playing());
        s.apply(&json!({"type":"auth_state","logged_in":false}));
        assert!(!s.active);
        assert!(!s.connected);
    }
    #[test]
    fn volume_updates_are_bounded_and_not_replayed_by_position_events() {
        let mut s = State::default();
        s.apply(&json!({"type":"volume_changed","volume":42}));
        assert_eq!(s.volume, Some(42));
        s.apply(&json!({"type":"position_sync","position":{"position_ms":1234}}));
        assert_eq!(s.volume, Some(42));
        assert_eq!(s.volume_revision, 1);
        assert_eq!(s.position_ms, 1234);
        s.apply(&json!({"type":"volume_changed","volume":101}));
        assert_eq!(s.volume, Some(42));
        assert_eq!(s.volume_revision, 1);
    }
    #[test]
    fn only_documented_playable_uri_types_are_sent() {
        assert!(playable("spotify:track:2JRo0gjbX4GrCqBYdRohoo"));
        assert!(!playable("spotify:search:radiohead"));
        assert!(!playable("spotify:track:bad"));
    }
    #[tokio::test]
    async fn missing_key_never_starts_a_process_or_falls_back() {
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let (_, task, state) = spawn(
            root.clone(),
            root,
            "test".into(),
            Arc::new(crate::airplay::Gate::new()),
        );
        task.await.unwrap();
        assert!(!state.borrow().connected);
        assert!(state.borrow().error.contains("API key missing"));
    }
}

#[cfg(test)]
mod control_tests {
    use super::*;
    #[test]
    fn key_storage_is_private_atomic_and_preserved_on_invalid_input() {
        let dir =
            crate::installation::Temporary::new(&std::env::temp_dir(), "soloist-key").unwrap();
        assert!(key_required(&dir.0));
        store_key(&dir.0, " synthetic-key\n").unwrap();
        assert!(!key_required(&dir.0));
        let key = dir.0.join("soloist/api-key");
        assert_eq!(
            fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(dir.0.join("soloist"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for invalid in [
            "",
            "with space",
            "with\tseparator",
            "with\0null",
            &"x".repeat(4097),
        ] {
            assert!(store_key(&dir.0, invalid).is_err());
            assert_eq!(fs::read_to_string(&key).unwrap(), "synthetic-key\n");
        }
        // Updating an installed runtime must never prompt or overwrite a valid key.
        prompt_key_if_missing(&dir.0).unwrap();
        assert_eq!(fs::read_to_string(&key).unwrap(), "synthetic-key\n");
        assert_eq!(fs::read_dir(dir.0.join("soloist")).unwrap().count(), 1);
        fs::write(&key, "").unwrap();
        assert!(key_required(&dir.0));
    }
    #[tokio::test(start_paused = true)]
    async fn coalesced_events_preserve_volume_and_position_anchor() {
        let mut s = State {
            connected: true,
            active: true,
            status: "playing".into(),
            position_ms: 1000,
            position_at: Some(Instant::now()),
            ..Default::default()
        };
        tokio::time::advance(Duration::from_secs(2)).await;
        s.apply(&json!({"type":"volume_changed","volume":30}));
        assert_eq!(s.position(), 3000);
        s.apply(&json!({"type":"playback_changed","status":"paused"}));
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(s.position(), 3000);
        assert_eq!(s.volume_revision, 1);
        s.apply(&json!({"type":"playback_changed","status":"playing"}));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(s.position(), 4000);
    }
    #[tokio::test]
    async fn transport_is_bounded_and_uses_documented_commands() {
        let (tx, mut rx) = mpsc::channel(1);
        let (stop, _) = watch::channel(false);
        let (events, state) = watch::channel(State::default());
        let h = Handle { tx, stop, state };
        assert!(h.command("play").is_err());
        assert!(rx.try_recv().is_err());
        events.send_replace(State {
            connected: true,
            active: true,
            status: "playing".into(),
            ..Default::default()
        });
        h.toggle().unwrap();
        assert!(h.command("skip_next").is_err());
        let request = rx.recv().await.unwrap();
        assert_eq!(request.commands, vec![wire("pause")]);
        assert!(request.deadline > Instant::now());
        h.release().unwrap();
        assert_eq!(
            rx.recv().await.unwrap().commands,
            vec![wire("pause"), wire("deactivate")]
        );
        assert!(h.play_uri("spotify:search:radiohead".into()).is_err());
        assert!(rx.try_recv().is_err());
    }
}
