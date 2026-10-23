use alsa::mixer::{SelemChannelId, SelemId};
use futures_util::StreamExt;
use librespot::{
    connect::{ConnectConfig, LoadRequest, LoadRequestOptions, Spirc},
    core::{
        Error,
        cache::Cache,
        config::{DeviceType, SessionConfig},
        session::Session,
    },
    playback::{
        audio_backend,
        config::{AudioFormat, NormalisationMethod, PlayerConfig},
        mixer::{Mixer, MixerConfig},
        player::{Player, PlayerEvent},
    },
};
use source::Source;
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, MissedTickBehavior},
};
mod audio_guard;
mod auth;
mod control;
mod device;
mod nav_player;
mod navidrome;
mod source;
mod spectrum;
mod tui;
mod voice;

struct Command {
    action: i32,
    reply: std::sync::mpsc::SyncSender<bool>,
}
struct Shared {
    running: AtomicBool,
    active: AtomicBool,
    paused: AtomicBool,
    volume: AtomicU16,
    commands: mpsc::UnboundedSender<Command>,
    spectrum: Arc<spectrum::Spectrum>,
}
static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

unsafe extern "C" {
    fn mini2_buttons_run() -> i32;
}

#[unsafe(no_mangle)]
pub extern "C" fn mini2_receiver_running() -> i32 {
    SHARED
        .get()
        .is_some_and(|s| s.running.load(Ordering::Acquire)) as i32
}
/// # Safety
/// Non-null pointers must reference writable, aligned `i32` objects for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mini2_receiver_status(paused: *mut i32, volume: *mut i32) -> i32 {
    let Some(s) = SHARED.get() else {
        return 0;
    };
    if paused.is_null() || volume.is_null() {
        return 0;
    }
    unsafe {
        *paused = s.paused.load(Ordering::Relaxed) as i32;
        *volume = percent(s.volume.load(Ordering::Relaxed)) as i32;
    }
    s.active.load(Ordering::Acquire) as i32
}
/// # Safety
/// A non-null `levels` must reference four writable bytes for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mini2_receiver_spectrum(levels: *mut u8) -> i32 {
    if levels.is_null() {
        return 0;
    }
    let Some(shared) = SHARED.get() else {
        return 0;
    };
    if !shared.active.load(Ordering::Acquire) || shared.paused.load(Ordering::Relaxed) {
        return 0;
    }
    let Some(frame) = shared.spectrum.frame() else {
        return 0;
    };
    unsafe {
        std::ptr::copy_nonoverlapping(frame.as_ptr(), levels, frame.len());
    }
    1
}
#[unsafe(no_mangle)]
pub extern "C" fn mini2_receiver_command(action: i32) -> i32 {
    let Some(s) = SHARED.get() else {
        return 0;
    };
    if !(0..=4).contains(&action) || !s.active.load(Ordering::Acquire) {
        return 0;
    }
    let (reply, response) = std::sync::mpsc::sync_channel(1);
    if s.commands.send(Command { action, reply }).is_err() {
        return 0;
    }
    response
        .recv_timeout(Duration::from_millis(400))
        .unwrap_or(false) as i32
}
fn percent(volume: u16) -> u16 {
    ((u32::from(volume) * 100 + 32767) / 65535) as u16
}
fn from_percent(value: u16) -> u16 {
    ((u32::from(value.min(100)) * 65535 + 50) / 100) as u16
}

struct MasterMixer {
    shared: Arc<Shared>,
}
impl MasterMixer {
    fn read(&self) -> Result<u16, alsa::Error> {
        let mixer = alsa::mixer::Mixer::new("default", false)?;
        let control = mixer
            .find_selem(&SelemId::new("Master", 0))
            .ok_or_else(|| alsa::Error::new("Master mixer", libc::ENOENT))?;
        let (min, max) = control.get_playback_volume_range();
        let value = control.get_playback_volume(SelemChannelId::FrontLeft)?;
        if max <= min {
            return Err(alsa::Error::new("Master range", libc::EINVAL));
        }
        Ok(((value - min).clamp(0, max - min) * 65535 / (max - min)) as u16)
    }
    fn write(&self, volume: u16) -> Result<u16, alsa::Error> {
        let mixer = alsa::mixer::Mixer::new("default", false)?;
        let control = mixer
            .find_selem(&SelemId::new("Master", 0))
            .ok_or_else(|| alsa::Error::new("Master mixer", libc::ENOENT))?;
        let (min, max) = control.get_playback_volume_range();
        if max <= min {
            return Err(alsa::Error::new("Master range", libc::EINVAL));
        }
        let value = min + (i64::from(volume) * (max - min) + 32767) / 65535;
        control.set_playback_volume_all(value)?;
        Ok(((value - min) * 65535 / (max - min)) as u16)
    }
}
impl Mixer for MasterMixer {
    fn open(_: MixerConfig) -> Result<Self, Error> {
        Ok(Self {
            shared: SHARED.get().expect("receiver initialized").clone(),
        })
    }
    fn volume(&self) -> u16 {
        self.read()
            .unwrap_or_else(|_| self.shared.volume.load(Ordering::Relaxed))
    }
    fn set_volume(&self, value: u16) {
        if let Ok(actual) = self.write(value) {
            self.shared.volume.store(actual, Ordering::Release);
        } else {
            eprintln!("receiver: Master volume write failed");
        }
    }
}

struct Options {
    state_dir: PathBuf,
    name: String,
    buttons: bool,
    voice: bool,
}
fn options() -> Result<Option<Options>, String> {
    let mut result = Options {
        state_dir: "/data/opt/spotify-connect/receiver-state".into(),
        name: device::system_name(),
        buttons: device::mini2_controls_available(),
        voice: true,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--state-dir" => {
                result.state_dir = args.next().ok_or("missing state directory")?.into()
            }
            "--name" => result.name = args.next().ok_or("missing device name")?,
            "--no-buttons" => result.buttons = false,
            "--no-voice" => result.voice = false,
            "--tui" => {
                tui::run().map_err(|e| e.to_string())?;
                return Ok(None);
            }
            "--login" => {
                use std::io::BufRead;
                let reader = control::login().map_err(|e| e.to_string())?;
                for line in reader.lines() {
                    let line = line.map_err(|e| e.to_string())?;
                    println!("{}", control::login_line(&line));
                    if line.starts_with("DONE") {
                        return Ok(None);
                    }
                    if line.starts_with("ERROR") {
                        return Err("Login was not completed".into());
                    }
                }
                return Err("Pairing connection closed before login completed".into());
            }
            "--navidrome-setup" => {
                let url = args.next().ok_or("missing Navidrome URL")?;
                let user = args.next().ok_or("missing Navidrome user")?;
                navidrome::setup_cli(&url, &user)?;
                println!("OK");
                return Ok(None);
            }
            "--play" | "--navidrome" | "--spotify" | "--source" | "--resume" | "--pause"
            | "--next" | "--previous" | "--toggle" | "--volume" | "--voice" | "--spectrum"
            | "--release" => {
                let command = match arg.as_str() {
                    "--play" => format!("PLAY {}", args.next().ok_or("missing music query")?),
                    "--navidrome" => {
                        format!(
                            "NAVIDROME PLAY {}",
                            args.next().ok_or("missing music query")?
                        )
                    }
                    "--spotify" => {
                        format!("SPOTIFY PLAY {}", args.next().ok_or("missing music query")?)
                    }
                    "--source" => format!(
                        "SOURCE {}",
                        args.next().ok_or("missing source")?.to_uppercase()
                    ),
                    "--resume" => "RESUME".to_owned(),
                    "--pause" => "PAUSE".to_owned(),
                    "--next" => "NEXT".to_owned(),
                    "--previous" => "PREVIOUS".to_owned(),
                    "--toggle" => "TOGGLE".to_owned(),
                    "--release" => "RELEASE".to_owned(),
                    "--volume" => format!("VOLUME {}", args.next().ok_or("missing volume 0..100")?),
                    _ => format!(
                        "{} {}",
                        arg.trim_start_matches("--").to_uppercase(),
                        args.next().ok_or("missing on/off")?.to_uppercase()
                    ),
                };
                if control::parse(&command).is_none() {
                    return Err("Invalid command or value".into());
                }
                print!("{}", control::request(&command).map_err(|e| e.to_string())?);
                return Ok(None);
            }
            "--version" => {
                println!("spotiya {} (librespot 0.8.0)", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--status" => {
                print!(
                    "{}",
                    fs::read_to_string("/run/spotiya.status").map_err(|e| e.to_string())?
                );
                return Ok(None);
            }
            "--help" | "-h" => {
                println!(
                    "spotiya [--state-dir PATH] [--name NAME] [--no-buttons] [--no-voice]\nspotiya --tui | --login | --status | --version\nspotiya --navidrome-setup URL USER   (password from stdin)\nspotiya --source spotify|navidrome | --navidrome QUERY | --spotify QUERY\nspotiya --play QUERY | --resume | --pause | --toggle | --next | --previous\nspotiya --volume 0..100 | --voice on/off | --spectrum on/off | --release"
                );
                return Ok(None);
            }
            _ => return Err(format!("unknown option: {arg}")),
        }
    }
    Ok(Some(result))
}
fn step_volume(mixer: &MasterMixer, up: bool) {
    let old = percent(mixer.volume()) as i32;
    let target = (old + if up { 5 } else { -5 }).clamp(0, 100) as u16;
    mixer.set_volume(from_percent(target));
}
fn read_source(path: &Path) -> Source {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| Source::parse(&s))
        .unwrap_or(Source::Spotify)
}
fn instance_lock(dir: &Path) -> io::Result<File> {
    fs::create_dir_all(dir)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("receiver.lock"))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}
fn publish_status(s: &Shared, info: &mut control::Status, last: &mut String) {
    info.active = s.active.load(Ordering::Acquire);
    info.paused = s.paused.load(Ordering::Relaxed);
    info.volume = percent(s.volume.load(Ordering::Relaxed));
    info.spectrum = s.spectrum.enabled();
    let mut text = serde_json::to_string(info).unwrap();
    text.push('\n');
    if *last == text {
        return;
    }
    if fs::write("/run/spotiya.status.new", &text).is_ok() {
        let _ = fs::rename("/run/spotiya.status.new", "/run/spotiya.status");
    }
    *last = text;
}
async fn stop_connection(
    spirc: &mut Option<Spirc>,
    task: &mut Option<JoinHandle<()>>,
    session: &Session,
    shared: &Shared,
) {
    shared.active.store(false, Ordering::Release);
    if let Some(handle) = spirc.take() {
        let _ = handle.shutdown();
    }
    if let Some(mut task) = task.take()
        && tokio::time::timeout(Duration::from_secs(2), &mut task)
            .await
            .is_err()
    {
        task.abort();
    }
    session.shutdown();
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        libc::umask(0o077);
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
    let Some(options) = options().map_err(io::Error::other)? else {
        return Ok(());
    };
    let _lock = instance_lock(&options.state_dir)?;
    let voice_setting = options.state_dir.join("voice-enabled");
    let mut voice_enabled = options.voice
        && fs::read_to_string(&voice_setting)
            .map(|s| s.trim() != "0")
            .unwrap_or(true);
    let spectrum_setting = options.state_dir.join("spectrum-enabled");
    let spectrum_supported = options.buttons && device::mini2_display_available();
    let spectrum_enabled =
        spectrum_supported && fs::read_to_string(&spectrum_setting).is_ok_and(|s| s.trim() == "1");
    let source_file = options.state_dir.join("default-source");
    let mut default_source = read_source(&source_file);
    let mut current = default_source;
    let mut navidrome_client = navidrome::Config::load(&options.state_dir)
        .and_then(|c| navidrome::Client::new(c).ok())
        .map(Arc::new);
    let last_track_file = options.state_dir.join("last-track");
    let mut last_track = fs::read_to_string(&last_track_file)
        .ok()
        .and_then(|s| voice::playable_track(&s));
    let cache = Cache::new(Some(&options.state_dir), None, None, None)?;
    let mut config = SessionConfig::default();
    let id_file = options.state_dir.join("device-id");
    if let Ok(id) = fs::read_to_string(&id_file) {
        if !id.trim().is_empty() {
            config.device_id = id.trim().to_owned();
        }
    } else {
        fs::write(&id_file, &config.device_id)?;
    }
    config.tmp_dir = "/tmp/spotiya".into();
    fs::create_dir_all(&config.tmp_dir)?;
    let mut discovery = librespot::discovery::Discovery::builder(
        config.device_id.clone(),
        config.client_id.clone(),
    )
    .name(options.name.clone())
    .device_type(DeviceType::Speaker)
    .port(10337)
    .launch()?;
    let (commands, mut command_rx) = mpsc::unbounded_channel();
    let shared = Arc::new(Shared {
        running: AtomicBool::new(true),
        active: AtomicBool::new(false),
        paused: AtomicBool::new(false),
        volume: AtomicU16::new(0),
        commands,
        spectrum: Arc::new(spectrum::Spectrum::new(spectrum_enabled)),
    });
    let _ = SHARED.set(shared.clone());
    let mixer = Arc::new(MasterMixer {
        shared: shared.clone(),
    });
    let initial = mixer.read()?;
    shared.volume.store(initial, Ordering::Relaxed);
    let mut session = Session::new(config.clone(), Some(cache.clone()));
    let player_config = PlayerConfig {
        normalisation: true,
        normalisation_method: NormalisationMethod::Basic,
        ..Default::default()
    };
    let backend = audio_backend::find(Some("alsa".into())).expect("ALSA compiled");
    let visual = shared.spectrum.clone();
    let nav_visual = shared.spectrum.clone();
    let player = Player::new(
        player_config,
        session.clone(),
        mixer.get_soft_volume(),
        move || {
            Box::new(spectrum::VisualSink::new(
                backend(Some("default".into()), AudioFormat::S16),
                visual.clone(),
            ))
        },
    );
    let mut events = player.get_player_event_channel();
    let (nav_tx, mut nav_events) = mpsc::unbounded_channel();
    let nav = nav_player::spawn(
        move || {
            Box::new(spectrum::VisualSink::new(
                backend(Some("default".into()), AudioFormat::S16),
                nav_visual,
            ))
        },
        tokio::runtime::Handle::current(),
        nav_tx,
        config.tmp_dir.join("navidrome"),
    )?;
    nav.configure(navidrome_client.clone());
    let (resolved_tx, mut resolved_rx) =
        mpsc::unbounded_channel::<(u64, Result<Vec<navidrome::Track>, String>)>();
    let mut play_seq = 0u64;
    let (voice_tx, mut voice_rx) = mpsc::channel(8);
    let voice_watch = tokio::spawn(voice::watch(voice_tx.clone()));
    let (login_tx, mut login_rx) = mpsc::channel::<auth::Login>(1);
    let control_config = config.clone();
    let control_dir = options.state_dir.clone();
    let voice_control = tokio::spawn(async move {
        if control::serve(
            voice_tx,
            login_tx,
            control_config,
            spectrum_supported,
            control_dir,
        )
        .await
        .is_err()
        {
            eprintln!("voice: local control socket unavailable");
        }
    });
    let audio_guard = Arc::new(audio_guard::AudioGuard::new());
    let audio_worker = {
        let guard = audio_guard.clone();
        tokio::spawn(async move {
            guard.run().await;
        })
    };
    let controls = if options.buttons {
        Some(
            std::thread::Builder::new()
                .name("spotiya-controls".into())
                .stack_size(128 * 1024)
                .spawn(|| unsafe { mini2_buttons_run() })?,
        )
    } else {
        None
    };
    let mut credentials = cache.credentials();
    let mut spirc: Option<Spirc> = None;
    let mut task: Option<JoinHandle<()>> = None;
    let mut retry_at = Instant::now();
    let mut backoff = 5;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut pending_login = None::<tokio::sync::oneshot::Sender<Result<(), &'static str>>>;
    let mut last_status = String::new();
    let mut info = control::Status {
        name: options.name.clone(),
        voice: voice_enabled,
        spectrum_supported,
        ..Default::default()
    };
    let mut spotify_meta: Option<(String, String, String, u32)> = None;
    let mut position_at = Instant::now();
    let mut position_ms = 0u32;
    eprintln!("receiver: ready, integrated buttons/ring/display; one Master volume");
    loop {
        tokio::select! {
            _=terminate.recv()=>break,
            _=tokio::signal::ctrl_c()=>break,
            fresh=discovery.next()=>{
                let Some(fresh)=fresh else { eprintln!("receiver: discovery stopped"); break; };
                stop_connection(&mut spirc,&mut task,&session,&shared).await;
                credentials=Some(fresh); retry_at=Instant::now(); backoff=5;
            },
            Some(login)=login_rx.recv()=>{
                audio_guard.set_playing(false);
                stop_connection(&mut spirc,&mut task,&session,&shared).await;
                credentials=Some(login.credentials);pending_login=Some(login.completed);
                retry_at=Instant::now();backoff=5;
            },
            _=tokio::time::sleep_until(retry_at), if spirc.is_none() && credentials.is_some()=>{
                session=Session::new(config.clone(),Some(cache.clone()));
                player.set_session(session.clone());
                let connect=ConnectConfig { name:options.name.clone(), initial_volume:mixer.volume(),
                    volume_steps:100, device_type:DeviceType::Speaker, ..Default::default() };
                let attempt=Spirc::new(connect,session.clone(),credentials.clone().unwrap(),player.clone(),mixer.clone());
                let result=tokio::select! {
                    _=terminate.recv()=>break,
                    _=tokio::signal::ctrl_c()=>break,
                    result=tokio::time::timeout(Duration::from_secs(30),attempt)=>result,
                };
                match result {
                    Ok(Ok((handle,future)))=>{
                        spirc=Some(handle);task=Some(tokio::spawn(future));backoff=5;eprintln!("receiver: Spotify connected");
                        if let Some(reply)=pending_login.take(){let _=reply.send(Ok(()));}
                    },
                    Ok(Err(e))=>{
                        if let Some(reply)=pending_login.take(){let _=reply.send(Err("Receiver could not connect the new account"));}
                        eprintln!("receiver: connect failed ({:?}); discovery remains available",e.kind);
                        if e.kind==librespot::core::error::ErrorKind::Unauthenticated {credentials=None;}
                        session.shutdown(); retry_at=Instant::now()+Duration::from_secs(backoff);backoff=(backoff*2).min(60);
                    },
                    Err(_)=>{
                        if let Some(reply)=pending_login.take(){let _=reply.send(Err("Receiver connect timed out"));}
                        session.shutdown();retry_at=Instant::now()+Duration::from_secs(backoff);backoff=(backoff*2).min(60);eprintln!("receiver: connect timeout");
                    },
                }
            },
            _=async {if let Some(task)=task.as_mut(){let _=task.await;}}, if task.is_some()=>{
                spirc=None;task=None;session.shutdown();shared.active.store(false,Ordering::Release);
                retry_at=Instant::now()+Duration::from_secs(5);eprintln!("receiver: reconnect scheduled");
            },
            Some(event)=events.recv()=>{
                if matches!(event,PlayerEvent::Playing{..}|PlayerEvent::Loading{..}) && current!=Source::Spotify {
                    nav.stop();current=Source::Spotify;
                    // Resuming the same Spotify track sends no TrackChanged, so restore its metadata.
                    if let Some((title,artist,album,duration))=spotify_meta.clone() {
                        info.title=title;info.artist=artist;info.album=album;info.duration_ms=duration;
                    }
                }
                match event {
                    PlayerEvent::VolumeChanged{..}=>{shared.volume.store(mixer.volume(),Ordering::Release);},
                    _ if current!=Source::Spotify=>{},
                    PlayerEvent::Playing{track_id,position_ms:position,..}=>{
                        position_ms=position;position_at=Instant::now();
                        shared.active.store(true,Ordering::Release);shared.paused.store(false,Ordering::Relaxed);
                        if let Some(uri)=track_id.to_uri().ok().and_then(|s| voice::playable_track(&s))
                            && last_track.as_ref()!=Some(&uri) {
                                let temp=last_track_file.with_extension("new");
                                if fs::write(&temp,&uri).and_then(|_| fs::rename(&temp,&last_track_file)).is_ok() {
                                    last_track=Some(uri);
                                } else {eprintln!("receiver: could not save last track");}
                            }
                    },
                    PlayerEvent::Loading{position_ms:position,..}=>{position_ms=position;position_at=Instant::now();shared.active.store(true,Ordering::Release);shared.paused.store(false,Ordering::Relaxed);},
                    PlayerEvent::Paused{position_ms:position,..}=>{position_ms=position;position_at=Instant::now();shared.active.store(true,Ordering::Release);shared.paused.store(true,Ordering::Relaxed);},
                    PlayerEvent::Stopped{..}|PlayerEvent::SessionDisconnected{..}=>{shared.active.store(false,Ordering::Release);},
                    PlayerEvent::PositionChanged{position_ms:position,..}|PlayerEvent::Seeked{position_ms:position,..}=>{position_ms=position;position_at=Instant::now();},
                    PlayerEvent::TrackChanged{audio_item}=>{
                        info.title=audio_item.name;info.duration_ms=audio_item.duration_ms;
                        match audio_item.unique_fields {
                            librespot::metadata::audio::UniqueFields::Track{artists,album,..}=>{info.artist=artists.iter().map(|a|a.name.as_str()).collect::<Vec<_>>().join(", ");info.album=album;},
                            librespot::metadata::audio::UniqueFields::Episode{show_name,..}=>{info.artist=show_name;info.album.clear();},
                            librespot::metadata::audio::UniqueFields::Local{artists,album,..}=>{info.artist=artists.unwrap_or_default();info.album=album.unwrap_or_default();},
                        }
                        spotify_meta=Some((info.title.clone(),info.artist.clone(),info.album.clone(),info.duration_ms));
                    },
                    _=>{},
                }
            },
            Some(event)=nav_events.recv()=>{
                match event {
                    nav_player::Event::Started(track) if current==Source::Navidrome=>{
                        info.title=track.title;info.artist=track.artist;info.album=track.album;info.duration_ms=track.duration_ms;
                        position_ms=0;position_at=Instant::now();
                        shared.active.store(true,Ordering::Release);shared.paused.store(false,Ordering::Relaxed);
                    },
                    nav_player::Event::Resumed if current==Source::Navidrome=>{
                        position_ms=nav.position_ms();position_at=Instant::now();
                        shared.active.store(true,Ordering::Release);shared.paused.store(false,Ordering::Relaxed);
                    },
                    nav_player::Event::Paused if current==Source::Navidrome=>{
                        position_ms=nav.position_ms();position_at=Instant::now();
                        shared.paused.store(true,Ordering::Relaxed);
                    },
                    nav_player::Event::Stopped if current==Source::Navidrome=>{
                        shared.active.store(false,Ordering::Release);current=default_source;
                    },
                    nav_player::Event::Failed(message)=>eprintln!("navidrome: {message}"),
                    _=>{},
                }
            },
            Some((seq,result))=resolved_rx.recv()=>{
                if seq!=play_seq {continue;}
                match result {
                    Ok(queue)=>{
                        if current==Source::Spotify && shared.active.load(Ordering::Acquire)
                            && let Some(handle)=spirc.as_ref() {let _=handle.pause();}
                        current=Source::Navidrome;
                        eprintln!("voice: Navidrome queue started ({} tracks)",queue.len());
                        nav.play(queue);
                    },
                    Err(e)=>eprintln!("voice: Navidrome search failed: {e}"),
                }
            },
            Some(command)=command_rx.recv()=>{
                let ok=if !shared.active.load(Ordering::Acquire) {false}
                else if current==Source::Navidrome {
                    match command.action {
                        0=>{if shared.paused.fetch_xor(true,Ordering::AcqRel) {nav.resume()}else{nav.pause()}true},
                        1=>{nav.next();true},
                        2=>{nav.previous();true},
                        3|4=>{step_volume(&mixer,command.action==3);true},
                        _=>false,
                    }
                }else if let Some(handle)=spirc.as_ref() {
                    match command.action {
                        0=>{let ok=handle.play_pause().is_ok();if ok {shared.paused.fetch_xor(true,Ordering::AcqRel);}ok},
                        1=>handle.next().is_ok(),
                        2=>handle.prev().is_ok(),
                        3|4=>{
                            step_volume(&mixer,command.action==3);
                            handle.set_volume(mixer.volume()).is_ok()
                        },
                        _=>false,
                    }
                }else{false};
                let _=command.reply.send(ok);
            },
            Some(event)=voice_rx.recv()=>{
                if event.received.elapsed()>Duration::from_secs(5) {continue;}
                if event.from_voice && !voice_enabled {continue;}
                let intent=match event.intent {
                    voice::Intent::Voice(enabled)=>{
                        if fs::write(&voice_setting,if enabled{"1\n"}else{"0\n"}).is_ok(){voice_enabled=enabled;info.voice=enabled;}
                        else{eprintln!("voice: could not save voice setting");}
                        continue;
                    },
                    voice::Intent::Spectrum(enabled)=>{
                        if spectrum_supported {
                            let temp=spectrum_setting.with_extension("new");
                            if fs::write(&temp,if enabled{"1\n"}else{"0\n"})
                                .and_then(|_| fs::rename(&temp,&spectrum_setting)).is_ok() {
                                shared.spectrum.set_enabled(enabled);
                            } else { eprintln!("display: could not save spectrum setting"); }
                        }
                        continue;
                    },
                    voice::Intent::Volume(volume)=>{
                        mixer.set_volume(from_percent(volume));
                        if let Some(handle)=spirc.as_ref(){let _=handle.set_volume(mixer.volume());}
                        continue;
                    },
                    voice::Intent::Release=>{
                        if let Some(handle)=spirc.as_ref(){let _=handle.disconnect(true);}
                        nav.stop();current=default_source;
                        shared.active.store(false,Ordering::Release);audio_guard.set_playing(false);
                        continue;
                    },
                    voice::Intent::Source(chosen)=>{
                        let temp=source_file.with_extension("new");
                        if fs::write(&temp,format!("{}\n",chosen.name())).and_then(|_| fs::rename(&temp,&source_file)).is_ok() {
                            default_source=chosen;
                            if !shared.active.load(Ordering::Acquire) {current=chosen;}
                        } else {eprintln!("receiver: could not save default source");}
                        continue;
                    },
                    voice::Intent::NavidromeReload=>{
                        navidrome_client=navidrome::Config::load(&options.state_dir)
                            .and_then(|c| navidrome::Client::new(c).ok()).map(Arc::new);
                        nav.configure(navidrome_client.clone());
                        eprintln!("receiver: Navidrome settings {}",if navidrome_client.is_some(){"loaded"}else{"invalid"});
                        continue;
                    },
                    voice::Intent::Toggle=>if shared.active.load(Ordering::Acquire)&&!shared.paused.load(Ordering::Relaxed){voice::Intent::Pause}else{voice::Intent::Resume},
                    intent=>intent,
                };
                if let voice::Intent::Play(query,chosen)=&intent
                    && chosen.unwrap_or(default_source)==Source::Navidrome {
                    let Some(client)=navidrome_client.clone() else {
                        eprintln!("voice: Navidrome is not set up; run spotiya navidrome-setup");continue;
                    };
                    if event.from_voice && voice::cancel_alice().await.is_err() {eprintln!("voice: could not cancel native conversation");continue;}
                    let _=voice::pause_native().await;
                    play_seq+=1;
                    let (seq,query,done)=(play_seq,query.clone(),resolved_tx.clone());
                    tokio::spawn(async move {let _=done.send((seq,client.resolve(&query).await));});
                    voice::guard_native_pause();
                    eprintln!("voice: Navidrome music query queued");
                    continue;
                }
                if current==Source::Navidrome && matches!(intent,voice::Intent::Pause|voice::Intent::Resume|voice::Intent::Next|voice::Intent::Previous) {
                    if event.from_voice {let _=voice::cancel_alice().await;}
                    let _=voice::pause_native().await;
                    match intent {
                        voice::Intent::Pause=>nav.pause(),
                        voice::Intent::Resume=>nav.resume(),
                        voice::Intent::Next=>nav.next(),
                        _=>nav.previous(),
                    }
                    if event.from_voice {voice::guard_native_pause();}
                    eprintln!("voice: Navidrome transport command queued");
                    continue;
                }
                let Some(handle)=spirc.as_ref() else {eprintln!("voice: Spotify is not connected yet");continue;};
                match intent {
                    voice::Intent::Play(query,_)=>{
                        if current==Source::Navidrome {nav.stop();current=Source::Spotify;}
                        if event.from_voice && voice::cancel_alice().await.is_err() {eprintln!("voice: could not cancel native conversation");continue;}
                        let _=voice::pause_native().await;
                        let _=handle.activate();
                        let request=LoadRequest::from_context_uri(voice::context(&query),LoadRequestOptions{start_playing:true,..Default::default()});
                        if handle.load(request).is_ok() {eprintln!("voice: Spotify music query queued");}
                        voice::guard_native_pause();
                    },
                    voice::Intent::Resume if !shared.active.load(Ordering::Acquire) && last_track.is_some()=>{
                        if event.from_voice && voice::cancel_alice().await.is_err() {continue;}
                        let _=voice::pause_native().await;
                        let _=handle.activate();
                        let request=LoadRequest::from_tracks(vec![last_track.clone().unwrap()],LoadRequestOptions{start_playing:true,..Default::default()});
                        if handle.load(request).is_ok() {eprintln!("voice: last Spotify track queued");}
                        voice::guard_native_pause();
                    },
                    intent if shared.active.load(Ordering::Acquire)=>{
                        if event.from_voice {let _=voice::cancel_alice().await;}
                        let _=voice::pause_native().await;
                        match intent {
                            voice::Intent::Pause=>{let _=handle.pause();},
                            voice::Intent::Resume=>{let _=handle.play();},
                            voice::Intent::Next=>{let _=handle.next();},
                            voice::Intent::Previous=>{let _=handle.prev();},
                            _=>{},
                        }
                        if event.from_voice {voice::guard_native_pause();}
                        eprintln!("voice: Spotify transport command queued");
                    },
                    _=>{},
                }
            },
            _=tick.tick()=>{
                if let Ok(value)=mixer.read() {
                    let previous=shared.volume.swap(value,Ordering::AcqRel);
                    if value!=previous && shared.active.load(Ordering::Acquire) && current==Source::Spotify
                        && let Some(handle)=spirc.as_ref() {let _=handle.set_volume(value);}
                }
                if player.is_invalid() {eprintln!("receiver: audio worker stopped");break;}
            },
        }
        let playing =
            shared.active.load(Ordering::Acquire) && !shared.paused.load(Ordering::Relaxed);
        audio_guard.set_playing(playing);
        info.connected = spirc.is_some();
        info.source = current.name().to_owned();
        info.default_source = default_source.name().to_owned();
        info.navidrome = navidrome_client.is_some();
        info.native_guard = playing && audio_guard.healthy();
        info.position_ms = position_ms
            .saturating_add(if playing {
                (position_at
                    .elapsed()
                    .as_secs()
                    .min(u64::from(u32::MAX) / 1000)
                    * 1000) as u32
            } else {
                0
            })
            .min(info.duration_ms);
        if current == Source::Navidrome && shared.active.load(Ordering::Acquire) {
            info.position_ms = nav.position_ms().min(info.duration_ms);
        }
        publish_status(&shared, &mut info, &mut last_status);
    }
    shared.active.store(false, Ordering::Release);
    shared.running.store(false, Ordering::Release);
    voice_watch.abort();
    audio_guard.set_playing(false);
    audio_worker.abort();
    nav.stop();
    voice_control.abort();
    let _ = fs::remove_file(voice::CONTROL);
    stop_connection(&mut spirc, &mut task, &session, &shared).await;
    if let Some(controls) = controls {
        let _ = controls.join();
    }
    player.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), discovery.shutdown()).await;
    info.connected = false;
    info.native_guard = false;
    publish_status(&shared, &mut info, &mut last_status);
    eprintln!("receiver: stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_volume_percentages_round_trip_through_master() {
        for p in 0..=100 {
            let normalized = from_percent(p);
            let raw = (u32::from(normalized) * 255 + 32767) / 65535;
            assert_eq!(percent((raw * 65535 / 255) as u16), p);
        }
    }
    #[tokio::test]
    async fn controls_bridge_checks_ownership_and_acknowledges_in_process_commands() {
        assert_eq!(mini2_receiver_command(0), 0);
        assert_eq!(mini2_receiver_command(99), 0);
        assert_eq!(mini2_receiver_running(), 0);
        let (commands, mut receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            running: AtomicBool::new(true),
            active: AtomicBool::new(false),
            paused: AtomicBool::new(true),
            volume: AtomicU16::new(from_percent(37)),
            commands,
            spectrum: Arc::new(spectrum::Spectrum::new(false)),
        });
        assert!(SHARED.set(shared.clone()).is_ok());
        assert_eq!(mini2_receiver_command(0), 0);
        shared.active.store(true, Ordering::Release);
        assert_eq!(mini2_receiver_command(99), 0);
        let worker = std::thread::spawn(|| mini2_receiver_command(3));
        let command = receiver.recv().await.unwrap();
        assert_eq!(command.action, 3);
        command.reply.send(true).unwrap();
        assert_eq!(worker.join().unwrap(), 1);
        let (mut paused, mut volume) = (0, 0);
        assert_eq!(
            unsafe { mini2_receiver_status(&mut paused, &mut volume) },
            1
        );
        assert_eq!((paused, volume), (1, 37));
        let mut levels = [0; 4];
        assert_eq!(unsafe { mini2_receiver_spectrum(std::ptr::null_mut()) }, 0);
        assert_eq!(unsafe { mini2_receiver_spectrum(levels.as_mut_ptr()) }, 0);
        shared.active.store(false, Ordering::Release);
        shared.running.store(false, Ordering::Release);
        assert_eq!(mini2_receiver_command(3), 0);
        assert_eq!(mini2_receiver_running(), 0);
    }
}
