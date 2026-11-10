use alsa::mixer::SelemId;
use futures_util::StreamExt;
use librespot::{
    connect::{ConnectConfig, Spirc},
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
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, MissedTickBehavior},
};
mod addons;
mod airplay;
mod animations;
mod audio_guard;
mod auth;
mod control;
mod device;
mod doctor;
mod ducking;
mod installation;
mod logging;
mod playback;
mod profile_selection;
mod profiles;
mod recovery;
mod screen;
mod soloist;
mod soloist_install;
mod spectrum;
mod spotify_backend;
mod supervisor;
use spotify_backend::{Backend, Handle};
mod tui;
mod update;
mod voice;
mod webapi;
mod volume_guard;
mod youtube;

struct Command {
    action: i32,
    deadline: std::time::Instant,
    reply: std::sync::mpsc::SyncSender<bool>,
}
const VOICE_EXPIRY: Duration = Duration::from_secs(5);
struct Prepare {
    intent: voice::Intent,
    from_voice: bool,
    received: Instant,
    epoch: u64,
}
type SpircTask = Pin<Box<dyn Future<Output = ()> + Send>>;
type Connecting = Pin<Box<dyn Future<Output = Option<Result<(Spirc, SpircTask), Error>>>>>;
struct Shared {
    running: AtomicBool,
    active: AtomicBool,
    paused: AtomicBool,
    volume: AtomicU16,
    commands: mpsc::UnboundedSender<Command>,
    spectrum: Arc<spectrum::Spectrum>,
    airplay: Arc<airplay::Gate>,
    ducking: Arc<ducking::Ducking>,
}
static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

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
    let timeout = Duration::from_millis(400);
    let deadline = std::time::Instant::now() + timeout;
    let (reply, response) = std::sync::mpsc::sync_channel(1);
    if s.commands
        .send(Command {
            action,
            deadline,
            reply,
        })
        .is_err()
    {
        return 0;
    }
    // A command that misses this deadline is dropped by the receiver, not run late.
    response.recv_timeout(timeout).unwrap_or(false) as i32
}
fn percent(volume: u16) -> u16 {
    ((u32::from(volume) * 100 + 32767) / 65535) as u16
}
fn from_percent(value: u16) -> u16 {
    ((u32::from(value.min(100)) * 65535 + 50) / 100) as u16
}

struct DeviceMixer {
    shared: Arc<Shared>,
    handle: Mutex<Option<alsa::mixer::Mixer>>,
    audio: profiles::Audio,
}
impl DeviceMixer {
    fn with_control<T>(
        &self,
        action: impl FnOnce(&alsa::mixer::Selem<'_>) -> Result<T, alsa::Error>,
    ) -> Result<T, alsa::Error> {
        let mut handle = self
            .handle
            .lock()
            .map_err(|_| alsa::Error::new("ALSA mixer lock", libc::EIO))?;
        if handle.is_none() {
            *handle = Some(alsa::mixer::Mixer::new(&self.audio.mixer_device, true)?);
        }
        let result = self.access(handle.as_ref().unwrap(), action);
        if result.is_err() {
            *handle = None;
        }
        result
    }
    fn access<T>(
        &self,
        mixer: &alsa::mixer::Mixer,
        action: impl FnOnce(&alsa::mixer::Selem<'_>) -> Result<T, alsa::Error>,
    ) -> Result<T, alsa::Error> {
        mixer.handle_events()?;
        let control = mixer
            .find_selem(&SelemId::new(
                &self.audio.mixer_control,
                self.audio.mixer_index,
            ))
            .ok_or_else(|| alsa::Error::new("ALSA mixer control", libc::ENOENT))?;
        action(&control)
    }
    fn read(&self) -> Result<u16, alsa::Error> {
        self.with_control(|control| {
            let (min, max) = control.get_playback_volume_range();
            let value = control.get_playback_volume(self.audio.mixer_channel.alsa())?;
            if max <= min {
                return Err(alsa::Error::new("ALSA mixer range", libc::EINVAL));
            }
            Ok(((value - min).clamp(0, max - min) * 65535 / (max - min)) as u16)
        })
    }
    fn write(&self, volume: u16) -> Result<u16, alsa::Error> {
        self.with_control(|control| {
            let (min, max) = control.get_playback_volume_range();
            if max <= min {
                return Err(alsa::Error::new("ALSA mixer range", libc::EINVAL));
            }
            let value = min + (i64::from(volume) * (max - min) + 32767) / 65535;
            control.set_playback_volume_all(value)?;
            Ok(((value - min) * 65535 / (max - min)) as u16)
        })
    }
    fn set_master_volume(&self, value: u16) {
        if value != self.shared.volume.load(Ordering::Acquire) {
            self.shared.ducking.volume.spotify_command();
        }
        if self.shared.ducking.volume.holding() {
            self.shared.volume.store(value, Ordering::Release);
            return;
        }
        if let Ok(actual) = self.write(value) {
            self.shared.volume.store(actual, Ordering::Release);
        } else {
            eprintln!("receiver: mixer volume write failed");
        }
    }
}
impl Mixer for DeviceMixer {
    fn open(config: MixerConfig) -> Result<Self, Error> {
        Ok(Self {
            shared: SHARED.get().expect("receiver initialized").clone(),
            handle: Mutex::new(None),
            audio: profiles::Audio {
                mixer_device: config.device,
                mixer_control: config.control,
                mixer_index: config.index,
                ..Default::default()
            },
        })
    }
    fn volume(&self) -> u16 {
        if self.shared.ducking.volume.holding() {
            return self.shared.volume.load(Ordering::Acquire);
        }
        self.read()
            .unwrap_or_else(|_| self.shared.volume.load(Ordering::Relaxed))
    }
    fn set_volume(&self, value: u16) {
        if !self.shared.airplay.active.load(Ordering::Acquire) {
            self.set_master_volume(value);
        }
    }
}

struct Options {
    state_dir: PathBuf,
    name: String,
    buttons: bool,
    voice: bool,
    supervise: bool,
    profile: Option<String>,
    backend: Option<Backend>,
}
fn options() -> Result<Option<Options>, String> {
    let mut result = Options {
        state_dir: "/data/opt/spotify-connect/receiver-state".into(),
        name: device::system_name(),
        buttons: true,
        voice: true,
        supervise: false,
        profile: None,
        backend: None,
    };
    let mut args = std::env::args().skip(1);
    let mut profile_info = false;
    let mut profile_list = false;
    let mut profile_select = None;
    let mut doctor = false;
    let mut doctor_report: Option<PathBuf> = None;
    let mut update = false;
    let mut update_check = false;
    let mut update_tag = None;
    let mut update_file = None;
    let mut install_root = None;
    let mut install_binary = None;
    let mut uninstall = false;
    let mut assume_yes = false;
    let mut addon_install = None;
    let mut addon_files = None;
    let mut addon_remove = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--backend" => {
                result.backend = Some(
                    Backend::parse(&args.next().ok_or("missing backend")?)
                        .map_err(|e| e.to_string())?,
                )
            }
            "--backend-select" => {
                let backend = Backend::parse(&args.next().ok_or("missing backend")?)
                    .map_err(|e| e.to_string())?;
                backend.save(&result.state_dir).map_err(|e| e.to_string())?;
                println!(
                    "Spotify backend: {}; restart Spotiya to apply",
                    backend.name()
                );
                return Ok(None);
            }
            "--soloist-key" => {
                soloist::save_key(&result.state_dir).map_err(|e| e.to_string())?;
                println!("Soloist key saved; restart Spotiya to apply");
                return Ok(None);
            }
            "--addon-install" => addon_install = Some(args.next().ok_or("missing add-on id")?),
            "--addon-remove" => addon_remove = Some(args.next().ok_or("missing add-on id")?),
            "--addon-install-file" => {
                addon_install = Some(args.next().ok_or("missing add-on id")?);
                addon_files = Some((
                    PathBuf::from(args.next().ok_or("missing archive")?),
                    PathBuf::from(args.next().ok_or("missing SHA256SUMS")?),
                ));
            }
            "--addons" => {
                tui::run(&result.state_dir, true).map_err(|e| e.to_string())?;
                return Ok(None);
            }
            "--state-dir" => {
                result.state_dir = args.next().ok_or("missing state directory")?.into()
            }
            "--name" => result.name = args.next().ok_or("missing device name")?,
            "--profile" => {
                result.profile = Some(args.next().ok_or("missing profile id or JSON path")?)
            }
            "--profile-info" => profile_info = true,
            "--profiles" => profile_list = true,
            "--profile-select" => {
                profile_select = Some(args.next().ok_or("missing profile id or JSON path")?)
            }
            "--doctor" => doctor = true,
            "--doctor-report" => {
                doctor = true;
                let path = args
                    .next()
                    .ok_or("missing report path; use - for an automatic path")?;
                doctor_report = Some(if path == "-" {
                    std::env::temp_dir()
                        .join(format!("spotiya-doctor-{}.json", uuid::Uuid::new_v4()))
                } else {
                    path.into()
                });
            }
            "--update" => update = true,
            "--uninstall" => uninstall = true,
            "--yes" => assume_yes = true,
            "--update-check" => {
                update = true;
                update_check = true;
            }
            "--update-tag" => {
                update = true;
                update_tag = Some(args.next().ok_or("missing release version")?);
            }
            "--update-file" => {
                update = true;
                update_file = Some((
                    PathBuf::from(args.next().ok_or("missing archive path")?),
                    PathBuf::from(args.next().ok_or("missing SHA256SUMS path")?),
                ));
            }
            "--airplay-event" => {
                let kind = args.next().ok_or("missing AirPlay event")?;
                let volume = args.next();
                if args.next().is_some() {
                    return Err("Unexpected AirPlay hook arguments".into());
                }
                airplay::hook(&kind, volume.as_deref()).map_err(|e| e.to_string())?;
                return Ok(None);
            }
            "--install" => {
                install_root = Some(PathBuf::from(args.next().ok_or("missing package root")?))
            }
            "--install-binary" => {
                install_binary = Some(PathBuf::from(args.next().ok_or("missing binary path")?))
            }
            "--supervise" => result.supervise = true,
            "--no-buttons" => result.buttons = false,
            "--no-voice" => result.voice = false,
            "--tui" => {
                tui::run(&result.state_dir, false).map_err(|e| e.to_string())?;
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
            "--youtube-url" => {
                let url = args.next().ok_or("missing YouTube link")?;
                if youtube::video_id(&url).is_none() {
                    return Err("Not a YouTube link".into());
                }
                print!(
                    "{}",
                    control::request(&format!("YOUTUBE URL {url}")).map_err(|e| e.to_string())?
                );
                return Ok(None);
            }
            "--play" | "--resume" | "--pause" | "--next" | "--previous" | "--toggle"
            | "--volume" | "--voice" | "--spectrum" | "--airplay" | "--youtube" | "--release"
            | "--retry" => {
                let command = match arg.as_str() {
                    "--play" => format!("PLAY {}", args.next().ok_or("missing music query")?),
                    "--resume" => "RESUME".to_owned(),
                    "--pause" => "PAUSE".to_owned(),
                    "--next" => "NEXT".to_owned(),
                    "--previous" => "PREVIOUS".to_owned(),
                    "--toggle" => "TOGGLE".to_owned(),
                    "--release" => "RELEASE".to_owned(),
                    "--retry" => "RETRY".to_owned(),
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
                println!(
                    "spotiya {} (Soloist / librespot 0.8.0)",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(None);
            }
            "--status" => {
                print!(
                    "{}",
                    fs::read_to_string(control::STATUS).map_err(|e| e.to_string())?
                );
                return Ok(None);
            }
            "--help" | "-h" => {
                println!(
                    "spotiya [--state-dir PATH] [--name NAME] [--profile auto|mini1|mini2|generic|FILE] [--no-buttons] [--no-voice]\nspotiya --backend soloist|librespot | --backend-select soloist|librespot\nspotiya --soloist-key < PRIVATE_KEY_FILE\nspotiya --profiles | --profile-info | --profile-select ID_OR_JSON\nspotiya --tui | --addons | --login | --status | --version\nspotiya --doctor | --doctor-report PATH\nspotiya --update | --update-check | --update-tag VERSION\nspotiya --uninstall [--yes]\nspotiya --update-file ARCHIVE SHA256SUMS\nspotiya --addon-install ID | --addon-install-file ID ARCHIVE SHA256SUMS | --addon-remove ID\nspotiya --play QUERY | --resume | --pause | --toggle | --next | --previous\nspotiya --volume 0..100 | --voice on/off | --spectrum on/off | --airplay on/off | --youtube on/off | --release | --retry\nspotiya --youtube-url LINK"
                );
                return Ok(None);
            }
            _ => return Err(format!("unknown option: {arg}")),
        }
    }
    if assume_yes && !uninstall {
        return Err("--yes requires --uninstall".into());
    }
    if [
        doctor,
        update,
        uninstall,
        addon_install.is_some(),
        addon_remove.is_some(),
        install_root.is_some(),
        profile_select.is_some(),
        profile_list,
        profile_info,
    ]
    .into_iter()
    .filter(|v| *v)
    .count()
        > 1
    {
        return Err("Choose one maintenance operation per command".into());
    }
    if uninstall {
        installation::uninstall(assume_yes).map_err(|e| e.to_string())?;
        return Ok(None);
    }
    if doctor {
        doctor::run(&result.state_dir, doctor_report.as_deref()).map_err(|e| e.to_string())?;
        return Ok(None);
    }
    if let Some(id) = addon_remove {
        addons::remove(&id, Path::new(installation::BASE)).map_err(|e| e.to_string())?;
        println!("Add-on removed.");
        return Ok(None);
    }
    if let Some(id) = addon_install {
        let base = Path::new(installation::BASE);
        let outcome = if let Some((archive, sums)) = addon_files {
            addons::local(&id, base, &archive, &sums).map_err(|e| e.to_string())?
        } else {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?
                .block_on(addons::remote(&id, base, |s| println!("{s}")))
                .map_err(|e| e.to_string())?
        };
        let addon = addons::find(&id).map_err(|e| e.to_string())?;
        if outcome == addons::Outcome::Installed && addon.command.is_some() {
            println!("{}. Enable it in spotiya addons.", outcome.message(addon));
        } else {
            println!("{}.", outcome.message(addon));
        }
        if id == "soloist" {
            soloist::prompt_key_if_missing(&result.state_dir).map_err(|e| e.to_string())?;
        }
        return Ok(None);
    }
    if update {
        if let Some((archive, sums)) = update_file {
            if update_check || update_tag.is_some() {
                return Err("Local update cannot be combined with release options".into());
            }
            update::local(&archive, &sums).map_err(|e| e.to_string())?;
        } else {
            update::remote(update_check, update_tag.as_deref()).map_err(|e| e.to_string())?;
        }
        return Ok(None);
    }
    if let Some(root) = install_root {
        let root = fs::canonicalize(root).map_err(|e| e.to_string())?;
        let binary = fs::canonicalize(install_binary.unwrap_or_else(|| root.join("spotiya")))
            .map_err(|e| e.to_string())?;
        installation::install(&root, &binary, false).map_err(|e| e.to_string())?;
        return Ok(None);
    }
    if install_binary.is_some() {
        return Err("--install-binary requires --install".into());
    }
    if let Some(choice) = profile_select {
        println!(
            "{}",
            profile_selection::select(&result.state_dir, &choice).map_err(|e| e.to_string())?
        );
        return Ok(None);
    }
    if profile_list {
        profiles::list(&result.state_dir).map_err(|e| e.to_string())?;
        return Ok(None);
    }
    if profile_info {
        let resolved = resolved_profile(&result).map_err(|e| e.to_string())?;
        println!(
            "{}",
            serde_json::to_string_pretty(&resolved).map_err(|e| e.to_string())?
        );
        return Ok(None);
    }
    Ok(Some(result))
}
fn resolved_profile(options: &Options) -> io::Result<profiles::Resolved> {
    let mut resolved = profiles::load(options.profile.as_deref(), &options.state_dir)?;
    if !options.buttons {
        resolved.capabilities.buttons = false;
        resolved.capabilities.ring = false;
        resolved.capabilities.display = false;
    }
    Ok(resolved)
}
/// Unlocks explicitly on drop: a child forked by another thread may still share the descriptor.
struct FileLock(File);
impl Drop for FileLock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}
fn file_lock(dir: &Path, name: &str) -> io::Result<FileLock> {
    fs::create_dir_all(dir)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(name))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileLock(file))
}
/// Why Spotify refused a login outright; retrying the same credentials cannot help.
#[derive(Clone, Copy)]
enum Rejected {
    Credentials,
    Premium,
}
impl Rejected {
    // librespot keeps its login error type private, so the reason is read from the message,
    // which is fixed in the pinned revision. An unknown reason keeps the normal retries.
    fn of(e: &Error) -> Option<Self> {
        if e.kind != librespot::core::error::ErrorKind::PermissionDenied {
            return None;
        }
        let text = e.error.to_string();
        if text.ends_with("reason: Bad credentials") {
            Some(Self::Credentials)
        } else if text.ends_with("reason: Premium account required") {
            Some(Self::Premium)
        } else {
            None
        }
    }
    fn status(self) -> &'static str {
        match self {
            Self::Credentials => "Saved Spotify login was rejected; pair again with spotiya login",
            Self::Premium => "Spotify Premium is required for this account",
        }
    }
    fn key(self) -> &'static str {
        match self {
            Self::Credentials => "credentials",
            Self::Premium => "premium",
        }
    }
    fn reply(self) -> &'static str {
        match self {
            Self::Credentials => "Spotify rejected the new login; previous account kept",
            Self::Premium => {
                "Spotify Premium is required for the new account; previous account kept"
            }
        }
    }
}
fn publish_status(
    s: &Shared,
    playback: &playback::Playback,
    failures: &recovery::Failures,
    login: Option<Rejected>,
    info: &mut control::Status,
    last: &mut Vec<u8>,
    text: &mut Vec<u8>,
) {
    let external = s.airplay.active.load(Ordering::Acquire);
    // AirPlay and YouTube share the external-output gate; only YouTube reports pauses.
    let youtube = external && s.airplay.holds_youtube();
    s.paused.store(
        if external {
            s.airplay.youtube_paused()
        } else {
            playback.paused()
        },
        Ordering::Relaxed,
    );
    s.active
        .store(external || playback.active(), Ordering::Release);
    info.airplay_active = external && !youtube;
    info.youtube_active = youtube;
    info.loading = !external && playback.loading();
    info.retry_required = failures.blocked();
    info.retry_after_secs = failures.retry_after();
    info.failed_tracks = failures.count();
    let refused = login.map_or("", Rejected::key);
    if info.login_refused != refused {
        info.login_refused = refused.into();
    }
    info.error = if !info.backend_error.is_empty() {
        info.backend_error.clone()
    } else if failures.blocked() {
        failures.message()
    } else if let Some(rejected) = login {
        rejected.status().to_owned()
    } else {
        playback.error().to_owned()
    };
    info.position_ms = if youtube {
        // The add-on reports where its track is; the gate counts on between reports.
        let (position, duration) = s.airplay.youtube_position().unwrap_or((0, 0));
        info.duration_ms = duration;
        position
    } else if external {
        0
    } else {
        playback.position_ms().min(info.duration_ms)
    };
    info.active = s.active.load(Ordering::Acquire);
    s.ducking.volume.set_enabled(info.active);
    s.ducking.volume.activity(s.ducking.active());
    info.paused = s.paused.load(Ordering::Relaxed);
    info.volume = percent(s.volume.load(Ordering::Relaxed));
    info.spectrum = s.spectrum.enabled();
    info.ducking = playback.playing() && s.ducking.active();
    text.clear();
    serde_json::to_writer(&mut *text, info).unwrap();
    text.push(b'\n');
    if last == text {
        return;
    }
    let temporary = Path::new(control::STATUS).with_extension("status.new");
    if fs::write(&temporary, &*text)
        .and_then(|_| fs::rename(&temporary, control::STATUS))
        .is_ok()
    {
        std::mem::swap(last, text);
    }
}
async fn stop_connection(
    spirc: &mut Option<Handle>,
    task: &mut Option<JoinHandle<()>>,
    session: &Session,
    shared: &Shared,
    playback: &mut playback::Playback,
) {
    playback.reset();
    shared.paused.store(false, Ordering::Relaxed);
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

fn main() -> std::process::ExitCode {
    unsafe extern "C" {
        fn spotiya_native_stderr();
    }
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tui::restore_terminal_on_panic();
        installation::restore_root_on_panic();
        report(info);
    }));
    unsafe {
        spotiya_native_stderr();
        libc::umask(0o077);
        libc::mallopt(libc::M_ARENA_MAX, 1);
        // Player creates its own multi-thread runtime after this single-threaded entry point.
        if std::env::var_os("TOKIO_WORKER_THREADS").is_none() {
            std::env::set_var("TOKIO_WORKER_THREADS", "1");
        }
    }
    // Returning the error from main would print its Debug form, such as `Custom { kind: .. }`.
    match start() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
fn start() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = options().map_err(io::Error::other)? else {
        return Ok(());
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .thread_stack_size(512 * 1024)
        .thread_keep_alive(Duration::from_secs(2))
        .build()?
        .block_on(run(options))
}
async fn run(options: Options) -> Result<(), Box<dyn std::error::Error>> {
    if options.supervise {
        let _ = resolved_profile(&options)?;
        supervisor::run(&options).await?;
        return Ok(());
    }
    let backend_kind = options
        .backend
        .map(Ok)
        .unwrap_or_else(|| Backend::read(&options.state_dir))?;
    let is_soloist = backend_kind == Backend::Soloist;
    let _lock = file_lock(&options.state_dir, "receiver.lock")?;
    logging::init()?;
    let resolved = resolved_profile(&options)?;
    let profile = resolved.profile;
    let caps = resolved.capabilities;
    let native = profile.voice.as_ref().map(|v| v.address);
    let voice_setting = options.state_dir.join("voice-enabled");
    let mut voice_enabled = options.voice
        && caps.voice
        && fs::read_to_string(&voice_setting)
            .map(|s| s.trim() != "0")
            .unwrap_or(true);
    let spectrum_setting = options.state_dir.join("spectrum-enabled");
    let spectrum_supported = caps.display;
    let spectrum_enabled =
        spectrum_supported && fs::read_to_string(&spectrum_setting).is_ok_and(|s| s.trim() == "1");
    let last_track_file = options.state_dir.join("last-track");
    let mut last_track = fs::read_to_string(&last_track_file)
        .ok()
        .and_then(|s| voice::playable_track(&s));
    let cache = Cache::new(Some(&options.state_dir), None, None, None)?;
    let mut config = SessionConfig::default();
    let id_file = options.state_dir.join("device-id");
    match fs::read_to_string(&id_file) {
        Ok(id) if !id.trim().is_empty() => config.device_id = id.trim().to_owned(),
        _ => {
            // An empty file left by a power cut would otherwise give a new device on every start.
            let temp = id_file.with_extension("new");
            fs::write(&temp, &config.device_id).and_then(|_| fs::rename(&temp, &id_file))?;
        }
    }
    config.tmp_dir = "/tmp/spotiya".into();
    fs::create_dir_all(&config.tmp_dir)?;
    let mut discovery = if is_soloist {
        None
    } else {
        Some(
            librespot::discovery::Discovery::builder(
                config.device_id.clone(),
                config.client_id.clone(),
            )
            .name(options.name.clone())
            .device_type(DeviceType::Speaker)
            .port(10337)
            .launch()?,
        )
    };
    let (commands, mut command_rx) = mpsc::unbounded_channel();
    let shared = Arc::new(Shared {
        running: AtomicBool::new(true),
        active: AtomicBool::new(false),
        paused: AtomicBool::new(false),
        volume: AtomicU16::new(0),
        commands,
        spectrum: Arc::new(spectrum::Spectrum::new(spectrum_enabled)),
        airplay: Arc::new(airplay::Gate::new()),
        ducking: Arc::new(ducking::Ducking::new()),
    });
    let _ = SHARED.set(shared.clone());
    let mixer = Arc::new(DeviceMixer {
        shared: shared.clone(),
        handle: Mutex::new(None),
        audio: profile.audio.clone(),
    });
    let initial = mixer.read().map_err(|e| {
        io::Error::other(format!(
            "Cannot open ALSA mixer '{}' on '{}': {e}; check the device profile",
            profile.audio.mixer_control, profile.audio.mixer_device
        ))
    })?;
    shared.volume.store(initial, Ordering::Relaxed);
    let mut session = Session::new(config.clone(), Some(cache.clone()));
    let player_config = PlayerConfig {
        normalisation: true,
        normalisation_method: NormalisationMethod::Basic,
        ..Default::default()
    };
    let backend = audio_backend::find(Some("alsa".into())).expect("ALSA compiled");
    let visual = shared.spectrum.clone();
    let airplay_gate = shared.airplay.clone();
    let ducking = shared.ducking.clone();
    let audio_device = profile.audio.device.clone();
    let audio_format = profile
        .audio
        .format
        .parse::<AudioFormat>()
        .map_err(|_| io::Error::other("Invalid profile audio format"))?;
    let player = Player::new(
        player_config,
        session.clone(),
        mixer.get_soft_volume(),
        move || {
            Box::new(spectrum::VisualSink::new(
                backend(Some(audio_device), audio_format),
                visual.clone(),
                ducking.clone(),
                airplay_gate.clone(),
            ))
        },
    );
    let mut events = player.get_player_event_channel();
    let (voice_tx, mut voice_rx) = mpsc::channel(8);
    let search_tx = voice_tx.clone();
    let voice_watch = native.map(|address| {
        tokio::spawn(voice::watch(
            voice_tx.clone(),
            shared.ducking.clone(),
            address,
        ))
    });
    let (login_tx, mut login_rx) = mpsc::channel::<auth::Login>(1);
    let control_config = if is_soloist {
        None
    } else {
        Some(config.clone())
    };
    let (airplay_tx, mut airplay_rx) = mpsc::channel::<airplay::Request>(8);
    let (youtube_tx, mut youtube_rx) = mpsc::channel::<youtube::Request>(8);
    let control_spectrum = shared.spectrum.clone();
    let voice_control = tokio::spawn(async move {
        if control::serve(
            voice_tx,
            login_tx,
            control_config,
            caps,
            airplay_tx,
            youtube_tx,
            control_spectrum,
        )
        .await
        .is_err()
        {
            eprintln!("voice: local control socket unavailable");
        }
    });
    // Native IPC can take up to a second, so it runs beside the event loop in request order.
    let (native_tx, mut native_rx) = mpsc::channel::<Prepare>(8);
    let (prepared_tx, mut prepared_rx) = mpsc::channel::<(Prepare, bool)>(8);
    let native_worker = tokio::spawn(async move {
        while let Some(request) = native_rx.recv().await {
            let prepared = voice::prepare_playback(native, request.from_voice)
                .await
                .is_ok();
            if prepared_tx.send((request, prepared)).await.is_err() {
                break;
            }
        }
    });
    // Bumped whenever the Spotify connection or ownership changes, so queued intents expire.
    let mut voice_epoch = 0u64;
    let audio_guard = Arc::new(audio_guard::AudioGuard::new());
    let audio_worker = native.map(|address| {
        let guard = audio_guard.clone();
        tokio::spawn(async move {
            guard.run(address).await;
        })
    });
    let controls = if caps.controls() {
        let config = profile
            .controls
            .clone()
            .expect("controls capability requires a driver");
        Some(
            std::thread::Builder::new()
                .name("spotiya-controls".into())
                .stack_size(128 * 1024)
                .spawn(move || config.run(caps))?,
        )
    } else {
        None
    };
    let mut credentials = if is_soloist {
        None
    } else {
        cache.credentials()
    };
    // Credentials that worked before a pending login, restored if the new account cannot connect.
    let mut fallback = None;
    // Set when Spotify refused the saved login; retries wait for new credentials.
    let mut rejected = None::<Rejected>;
    let mut spirc: Option<Handle> = None;
    let mut task: Option<JoinHandle<()>> = None;
    let mut connecting: Option<Connecting> = None;
    let mut retry_at = Instant::now();
    let mut backoff = 5;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut pending_login = None::<tokio::sync::oneshot::Sender<Result<(), &'static str>>>;
    let mut last_status = Vec::new();
    let mut status_buffer = Vec::new();
    let mut info = control::Status {
        name: options.name.clone(),
        spotify_backend: backend_kind.name().into(),
        voice: voice_enabled,
        voice_supported: caps.voice,
        profile: profile.id.clone(),
        profile_selection: options
            .profile
            .clone()
            .or_else(|| {
                profile_selection::read(&options.state_dir)
                    .ok()
                    .flatten()
                    .map(|s| s.selection)
            })
            .unwrap_or_else(|| "auto".into()),
        receiver_pid: std::process::id(),
        spectrum_supported,
        ..Default::default()
    };
    let base = std::env::current_exe()?
        .parent()
        .ok_or_else(|| io::Error::other("No install directory"))?
        .to_path_buf();
    let mut airplay =
        airplay::Runtime::new(&base, &options.state_dir, &options.name, &profile.audio);
    airplay.poll(&shared.airplay).await;
    airplay.status(&mut info, &shared.airplay);
    if let Err(e) = youtube::receive_levels(shared.airplay.clone(), shared.spectrum.clone()) {
        eprintln!("youtube: spectrum levels unavailable: {e}");
    }
    let mut youtube =
        youtube::Runtime::new(&base, &options.state_dir, &options.name, &profile.audio);
    youtube.poll(&shared.airplay).await;
    youtube.status(&mut info, &shared.airplay);
    if is_soloist {
        soloist::receive_levels(shared.airplay.clone(), shared.spectrum.clone())?;
    }
    let (mut soloist_task, mut soloist_state) = if is_soloist {
        let (handle, task, state) = soloist::spawn(
            base.clone(),
            options.state_dir.clone(),
            options.name.clone(),
            shared.airplay.clone(),
        );
        spirc = Some(Handle::Soloist(handle));
        (Some(task), Some(state))
    } else {
        (None, None)
    };
    let mut soloist_volume_revision = 0;
    let mut soloist_error = String::new();
    let mut soloist_connected = false;
    let mut playback = playback::Playback::default();
    let mut failures = recovery::Failures::default();
    let mut retry_track = None::<String>;
    let mut reclaim_at = Instant::now() + Duration::from_secs(30);
    eprintln!(
        "receiver: ready, profile={}, buttons={}, ring={}, display={}, voice={}",
        profile.id, caps.buttons, caps.ring, caps.display, caps.voice
    );
    publish_status(
        &shared,
        &playback,
        &failures,
        rejected,
        &mut info,
        &mut last_status,
        &mut status_buffer,
    );
    loop {
        tokio::select! {
            _=terminate.recv()=>break,
            _=tokio::signal::ctrl_c()=>break,
            changed=async {match soloist_state.as_mut() {Some(state)=>state.changed().await,None=>std::future::pending().await}}, if soloist_state.is_some()=>{
                if let Some(state)=soloist_state.as_ref() {
                    let state=state.borrow().clone();
                    soloist_error=state.error.clone();soloist_connected=state.connected;
                    playback.soloist(&state);
                    if !shared.airplay.active.load(Ordering::Acquire) {
                        info.title=state.title.clone();info.artist=state.artist.clone();info.album=state.album.clone();info.duration_ms=state.duration_ms;
                    }
                    if state.volume_revision!=soloist_volume_revision {
                        soloist_volume_revision=state.volume_revision;
                        if let Some(volume)=state.volume {mixer.set_master_volume(from_percent(volume));}
                    }
                    if state.playing() {
                        if shared.airplay.holds_youtube() {let _=youtube.send("STOP");}
                        else if shared.airplay.active.load(Ordering::Acquire) {airplay.stop(&shared.airplay).await;}
                        if let Some(uri)=voice::playable_track(&state.uri) && last_track.as_ref()!=Some(&uri) {
                            let temp=last_track_file.with_extension("new");
                            if fs::write(&temp,&uri).and_then(|_|fs::rename(&temp,&last_track_file)).is_ok(){last_track=Some(uri);}
                        }
                    }
                }
                if changed.is_err() {soloist_state=None;}
            },
            fresh=async {match discovery.as_mut() {Some(d)=>d.next().await,None=>std::future::pending().await}}, if discovery.is_some()=>{
                let Some(fresh)=fresh else { eprintln!("receiver: discovery stopped"); break; };
                audio_guard.set_playing(false);connecting=None;voice_epoch+=1;
                if let Some(reply)=pending_login.take(){let _=reply.send(Err("Login was replaced by another Spotify device"));}
                fallback=None;rejected=None;
                stop_connection(&mut spirc,&mut task,&session,&shared,&mut playback).await;
                failures.reset();retry_track=None;
                credentials=Some(fresh); retry_at=Instant::now(); backoff=5;
            },
            Some(login)=login_rx.recv()=>{
                audio_guard.set_playing(false);connecting=None;voice_epoch+=1;
                if let Some(reply)=pending_login.take(){let _=reply.send(Err("Login was replaced by a newer login"));}
                else {fallback=credentials.take();}
                stop_connection(&mut spirc,&mut task,&session,&shared,&mut playback).await;
                failures.reset();retry_track=None;
                credentials=Some(login.credentials);pending_login=Some(login.completed);rejected=None;
                retry_at=Instant::now();backoff=5;
            },
            _=tokio::time::sleep_until(retry_at), if spirc.is_none() && connecting.is_none() && credentials.is_some() && !failures.blocked()=>{
                session=Session::new(config.clone(),Some(cache.clone()));
                player.set_session(session.clone());
                let connect=ConnectConfig { name:options.name.clone(), initial_volume:mixer.volume(),
                    volume_steps:100, device_type:DeviceType::Speaker, ..Default::default() };
                let attempt=Spirc::new(connect,session.clone(),credentials.clone().unwrap(),player.clone(),mixer.clone());
                connecting=Some(Box::pin(async move {
                    tokio::time::timeout(Duration::from_secs(30),attempt).await.ok()
                        .map(|result| result.map(|(handle,future)| (handle,Box::pin(future) as SpircTask)))
                }));
            },
            result=async {match connecting.as_mut() {Some(attempt)=>attempt.await,None=>std::future::pending().await}}, if connecting.is_some()=>{
                connecting=None;
                match result {
                    Some(Ok((handle,future)))=>{
                        spirc=Some(Handle::Librespot(handle));task=Some(tokio::spawn(future));backoff=5;fallback=None;eprintln!("receiver: Spotify connected");
                        if let Some(reply)=pending_login.take(){let _=reply.send(Ok(()));}
                        if let Some(uri)=retry_track.take() && let Some(handle)=spirc.as_ref() {
                            let _=handle.activate();
                            let _=handle.load_uri(uri);
                        }
                    },
                    Some(Err(e))=>{
                        let refused=Rejected::of(&e);
                        if let Some(reply)=pending_login.take(){let _=reply.send(Err(refused.map_or("Receiver could not connect the new account; previous account kept",Rejected::reply)));}
                        eprintln!("receiver: connect failed ({e}); discovery remains available");
                        if e.kind==librespot::core::error::ErrorKind::Unauthenticated {credentials=None;}
                        session.shutdown(); retry_at=Instant::now()+Duration::from_secs(backoff);backoff=(backoff*2).min(60);
                        if let Some(previous)=fallback.take() {credentials=Some(previous);retry_at=Instant::now();backoff=5;eprintln!("receiver: previous account restored");}
                        else if refused.is_some() {credentials=None;rejected=refused;eprintln!("receiver: retries stopped until a new login");}
                    },
                    None=>{
                        if let Some(reply)=pending_login.take(){let _=reply.send(Err("Receiver connect timed out; previous account kept"));}
                        session.shutdown();retry_at=Instant::now()+Duration::from_secs(backoff);backoff=(backoff*2).min(60);eprintln!("receiver: connect timeout");
                        if let Some(previous)=fallback.take() {credentials=Some(previous);retry_at=Instant::now();backoff=5;eprintln!("receiver: previous account restored");}
                    },
                }
            },
            _=async {if let Some(task)=task.as_mut(){let _=task.await;}}, if task.is_some()=>{
                spirc=None;task=None;session.shutdown();playback.reset();voice_epoch+=1;
                retry_at=Instant::now()+Duration::from_secs(5);eprintln!("receiver: reconnect scheduled");
            },
            Some(event)=events.recv()=>{
                if failures.blocked() || !playback.update(&event) {continue;}
                if failures.observe(&event) {
                    log::warn!("receiver: {}",failures.message());
                    audio_guard.set_playing(false);player.stop();voice_epoch+=1;
                    stop_connection(&mut spirc,&mut task,&session,&shared,&mut playback).await;
                    while events.try_recv().is_ok() {}
                }
                match event {
                    PlayerEvent::PlayRequestIdChanged{..}=>{
                        info.title.clear();info.artist.clear();info.album.clear();info.duration_ms=0;
                    },
                    PlayerEvent::Playing{track_id,..}=>{
                        if let Some(uri)=voice::playable_track(&track_id.to_uri())
                            && last_track.as_ref()!=Some(&uri) {
                                let temp=last_track_file.with_extension("new");
                                if fs::write(&temp,&uri).and_then(|_| fs::rename(&temp,&last_track_file)).is_ok() {
                                    last_track=Some(uri);
                                } else {eprintln!("receiver: could not save last track");}
                            }
                    },
                    PlayerEvent::TrackChanged{audio_item}=>{
                        info.title=audio_item.name;info.duration_ms=audio_item.duration_ms;
                        match audio_item.unique_fields {
                            librespot::metadata::audio::UniqueFields::Track{artists,album,..}=>{info.artist=artists.iter().map(|a|a.name.as_str()).collect::<Vec<_>>().join(", ");info.album=album;},
                            librespot::metadata::audio::UniqueFields::Episode{show_name,..}=>{info.artist=show_name;info.album.clear();},
                            librespot::metadata::audio::UniqueFields::Local{artists,album,..}=>{info.artist=artists.unwrap_or_default();info.album=album.unwrap_or_default();},
                        }
                    },
                    PlayerEvent::VolumeChanged{..}=>{shared.volume.store(mixer.volume(),Ordering::Release);},
                    _=>{},
                }
            },
            Some(request)=airplay_rx.recv()=>{
                let accepted = if request.reply.is_closed() || !airplay.valid(&request.token) { false } else {
                    match request.action {
                        airplay::Action::Begin => {
                            if shared.airplay.holds_youtube(){youtube.stop(&shared.airplay).await;}
                            shared.airplay.active.store(true,Ordering::Release);
                            if let Some(handle)=spirc.as_ref(){let _=handle.disconnect(true);}
                            player.stop();playback.reset();
                            let released=shared.airplay.spotify_released(Instant::now()+Duration::from_millis(1500)).await;
                            let accepted=shared.airplay.active.load(Ordering::Acquire) && released;
                            if accepted {
                                if let Some(address)=native {tokio::spawn(voice::pause_native(address));}
                                info.title.clear();info.artist.clear();info.album.clear();info.duration_ms=0;
                            } else {airplay.stop(&shared.airplay).await;}
                            accepted
                        },
                        airplay::Action::End=>{shared.airplay.active.store(false,Ordering::Release);true},
                        airplay::Action::Volume(value)=>{
                            if shared.airplay.active.load(Ordering::Acquire){mixer.set_master_volume(from_percent(value));}
                            true
                        },
                    }
                };
                let _=request.reply.send(accepted);
            },
            Some(request)=youtube_rx.recv()=>{
                let accepted = if request.reply.is_closed() { false } else {
                    match request.action {
                        // The add-on took the speaker for a track (token-authenticated).
                        youtube::Action::Begin => {
                            if !youtube.valid(&request.token) { false } else {
                                if shared.airplay.active.load(Ordering::Acquire) && !shared.airplay.holds_youtube() {
                                    airplay.stop(&shared.airplay).await;
                                }
                                shared.airplay.hold_youtube(youtube.control_path());
                                if let Some(handle)=spirc.as_ref(){let _=handle.disconnect(true);}
                                player.stop();playback.reset();
                                let released=shared.airplay.spotify_released(Instant::now()+Duration::from_millis(1500)).await;
                                let accepted=shared.airplay.holds_youtube() && released;
                                if accepted {
                                    if let Some(address)=native {tokio::spawn(voice::pause_native(address));}
                                    info.title.clear();info.artist.clear();info.album.clear();info.duration_ms=0;
                                } else {shared.airplay.release_youtube();}
                                accepted
                            }
                        },
                        youtube::Action::End=>{
                            if youtube.valid(&request.token) && shared.airplay.holds_youtube(){
                                shared.airplay.release_youtube();
                                info.title.clear();info.artist.clear();info.album.clear();info.duration_ms=0;
                            }
                            true
                        },
                        youtube::Action::Volume(value)=>{
                            if youtube.valid(&request.token) && shared.airplay.holds_youtube(){mixer.set_master_volume(from_percent(value));}
                            true
                        },
                        youtube::Action::Title(title)=>{
                            if youtube.valid(&request.token) && shared.airplay.holds_youtube(){
                                info.title=title;info.artist.clear();info.album.clear();info.duration_ms=0;
                            }
                            true
                        },
                        youtube::Action::Paused(paused)=>{
                            if youtube.valid(&request.token){shared.airplay.set_youtube_paused(paused);}
                            true
                        },
                        youtube::Action::Position(position,duration)=>{
                            if youtube.valid(&request.token) && shared.airplay.holds_youtube(){shared.airplay.set_youtube_progress(position,duration);}
                            true
                        },
                        // A pasted link or a transport command (trusted local socket, no token).
                        youtube::Action::Play(id)=>{
                            match youtube.send(&format!("PLAY {id}")){Ok(())=>true,Err(e)=>{eprintln!("youtube: {e}");false}}
                        },
                        youtube::Action::Control(command)=>youtube.send(command).is_ok(),
                    }
                };
                let _=request.reply.send(accepted);
            },
            Some(command)=command_rx.recv()=>{
                if std::time::Instant::now()>=command.deadline {continue;}
                let ok=if shared.airplay.active.load(Ordering::Acquire) {
                    // YouTube takes transport commands; AirPlay can only be stopped.
                    // Volume keys drive the shared mixer for both.
                    if shared.airplay.holds_youtube() && matches!(command.action,0..=2) {
                        // The add-on toggles by its own state, which is never stale.
                        let verb=match command.action {
                            0=>"TOGGLE",
                            1=>"NEXT",
                            _=>"PREVIOUS",
                        };
                        youtube.send(verb).is_ok()
                    }
                    else if command.action==0 {airplay.stop(&shared.airplay).await;true}
                    else if matches!(command.action,3|4){
                        let target=(percent(mixer.volume()) as i32+if command.action==3{5}else{-5}).clamp(0,100) as u16;
                        mixer.set_master_volume(from_percent(target));true
                    } else {false}
                } else if let Some(handle)=spirc.as_ref().filter(|_| shared.active.load(Ordering::Acquire)) {
                    match command.action {
                        0=>handle.play_pause().is_ok(),
                        1=>handle.next().is_ok(),
                        2=>handle.prev().is_ok(),
                        3|4=>{
                            let old=percent(mixer.volume()) as i32;
                            let target=(old+if command.action==3 {5}else{-5}).clamp(0,100) as u16;
                            mixer.set_master_volume(from_percent(target));
                            handle.set_volume(mixer.volume()).is_ok()
                        },
                        _=>false,
                    }
                }else{false};
                let _=command.reply.send(ok);
            },
            Some(event)=voice_rx.recv()=>{
                if event.received.elapsed()>VOICE_EXPIRY {continue;}
                if event.from_voice && !voice_enabled {continue;}
                if is_soloist && let voice::Intent::Play(query)=&event.intent && !soloist::playable(query)
                    && webapi::credentials_path(&options.state_dir).exists() {
                    // Soloist has no text search: resolve the query through the Web API
                    // and feed the resulting URI back as a fresh voice event.
                    let (query,state,tx,from_voice)=(query.clone(),options.state_dir.clone(),search_tx.clone(),event.from_voice);
                    tokio::spawn(async move {
                        match webapi::search(&state,&query).await {
                            Ok(uri)=>{let _=tx.send(voice::Event{intent:voice::Intent::Play(uri),received:Instant::now(),from_voice}).await;},
                            Err(e)=>eprintln!("voice: {e}"),
                        }
                    });
                    continue;
                }
                if is_soloist && let voice::Intent::Play(query)=&event.intent && !soloist::playable(query) {
                    soloist_error="Soloist: text search is not supported; select music in Spotify or pass a spotify:track/album/playlist/episode URI".into();
                    info.backend_error=soloist_error.clone();
                    publish_status(&shared,&playback,&failures,rejected,&mut info,&mut last_status,&mut status_buffer);
                    continue;
                }
                let intent=match event.intent {
                    voice::Intent::Airplay(enabled)=>{
                        if let Err(e)=airplay.set_enabled(enabled){eprintln!("airplay: {e}");}
                        airplay.poll(&shared.airplay).await;
                        airplay.status(&mut info,&shared.airplay);
                        publish_status(&shared,&playback,&failures,rejected,&mut info,&mut last_status,&mut status_buffer);
                        continue;
                    },
                    voice::Intent::Youtube(enabled)=>{
                        if let Err(e)=youtube.set_enabled(enabled){eprintln!("youtube: {e}");}
                        youtube.poll(&shared.airplay).await;
                        youtube.status(&mut info,&shared.airplay);
                        publish_status(&shared,&playback,&failures,rejected,&mut info,&mut last_status,&mut status_buffer);
                        continue;
                    },
                    voice::Intent::Retry if is_soloist=>{
                        if let Some(handle)=spirc.take(){let _=handle.shutdown();}
                        if let Some(task)=soloist_task.take(){let _=task.await;}
                        let (handle,task,state)=soloist::spawn(base.clone(),options.state_dir.clone(),options.name.clone(),shared.airplay.clone());
                        spirc=Some(Handle::Soloist(handle));soloist_task=Some(task);soloist_state=Some(state);
                        soloist_error.clear();soloist_volume_revision=0;soloist_connected=false;playback.reset();continue;
                    },
                    voice::Intent::Retry=>{
                        if let Some(uri)=failures.retry() {
                            retry_track=Some(uri);retry_at=Instant::now();backoff=5;
                            eprintln!("receiver: explicit retry scheduled");
                        } else {eprintln!("receiver: retry unavailable or cooling down");}
                        publish_status(&shared,&playback,&failures,rejected,&mut info,&mut last_status,&mut status_buffer);
                        continue;
                    },
                    voice::Intent::Voice(enabled)=>{
                        if !caps.voice {continue;}
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
                        mixer.set_master_volume(from_percent(volume));
                        if let Some(handle)=spirc.as_ref(){let _=handle.set_volume(mixer.volume());}
                        continue;
                    },
                    voice::Intent::Release=>{
                        if shared.airplay.holds_youtube() {let _=youtube.send("STOP");}
                        else if shared.airplay.active.load(Ordering::Acquire) {airplay.stop(&shared.airplay).await;}
                        if let Some(handle)=spirc.as_ref(){let _=handle.disconnect(true);}
                        voice_epoch+=1;
                        playback.reset();shared.paused.store(false,Ordering::Relaxed);
                        shared.active.store(false,Ordering::Release);audio_guard.set_playing(false);
                        continue;
                    },
                    // YouTube toggles by its own state (below); others by ours.
                    voice::Intent::Toggle if shared.airplay.holds_youtube()=>voice::Intent::Toggle,
                    voice::Intent::Toggle=>if shared.active.load(Ordering::Acquire)&&!shared.paused.load(Ordering::Relaxed){voice::Intent::Pause}else{voice::Intent::Resume},
                    intent=>intent,
                };
                if shared.airplay.holds_youtube() {
                    let verb=match intent {
                        voice::Intent::Toggle=>Some("TOGGLE"),
                        voice::Intent::Pause=>Some("PAUSE"),
                        voice::Intent::Resume=>Some("RESUME"),
                        voice::Intent::Next=>Some("NEXT"),
                        voice::Intent::Previous=>Some("PREVIOUS"),
                        _=>None,
                    };
                    if let Some(verb)=verb {let _=youtube.send(verb);continue;}
                    // A Spotify search takes the speaker; YouTube stops and drains first.
                    // Without a Spotify connection the search cannot play, so keep YouTube.
                    if matches!(intent,voice::Intent::Play(_)) && spirc.is_some() {let _=youtube.send("STOP");}
                    else {
                        if spirc.is_none() {eprintln!("voice: Spotify is not connected yet");}
                        continue;
                    }
                } else if shared.airplay.active.load(Ordering::Acquire) {
                    if matches!(intent,voice::Intent::Pause|voice::Intent::Toggle) {
                        airplay.stop(&shared.airplay).await;
                        continue;
                    }
                    if matches!(intent,voice::Intent::Play(_)|voice::Intent::Resume) {airplay.stop(&shared.airplay).await;}
                    else {continue;}
                }
                if failures.blocked() {continue;}
                if spirc.is_none() {eprintln!("voice: Spotify is not connected yet");continue;}
                let active=shared.active.load(Ordering::Acquire);
                let wanted=match intent {
                    voice::Intent::Play(_)=>true,
                    voice::Intent::Resume if !active && last_track.is_some()=>true,
                    _=>active,
                };
                let request=Prepare{intent,from_voice:event.from_voice,received:event.received,epoch:voice_epoch};
                if wanted && native_tx.try_send(request).is_err() {eprintln!("voice: native command queue is full");}
            },
            Some((request,prepared))=prepared_rx.recv()=>{
                if request.epoch!=voice_epoch || request.received.elapsed()>VOICE_EXPIRY {eprintln!("voice: stale command dropped");continue;}
                if shared.airplay.active.load(Ordering::Acquire) || failures.blocked() {continue;}
                let intent=request.intent;
                let Some(handle)=spirc.as_ref() else {eprintln!("voice: Spotify is not connected yet");continue;};
                match intent {
                    voice::Intent::Play(query)=>{
                        if !prepared {eprintln!("voice: could not cancel native conversation");continue;}
                        let _=handle.activate();
                        let uri=if query.starts_with("spotify:") {query} else {voice::context(&query)};
                        match handle.load_uri(uri) {Ok(())=>eprintln!("voice: Spotify music query queued"),Err(e)=>{eprintln!("voice: {e}");if is_soloist {soloist_error=e.to_string();}}}
                    },
                    voice::Intent::Resume if !shared.active.load(Ordering::Acquire) && last_track.is_some()=>{
                        if !prepared {continue;}
                        let _=handle.activate();
                        if handle.load_uri(last_track.clone().unwrap()).is_ok() {eprintln!("voice: last Spotify track queued");}
                    },
                    intent if shared.active.load(Ordering::Acquire)=>{
                        match intent {
                            voice::Intent::Pause=>{let _=handle.pause();},
                            voice::Intent::Resume=>{let _=handle.play();},
                            voice::Intent::Next=>{let _=handle.next();},
                            voice::Intent::Previous=>{let _=handle.prev();},
                            _=>{},
                        }
                        eprintln!("voice: Spotify transport command queued");
                    },
                    _=>{},
                }
            },
            _=tick.tick()=>{
                airplay.poll(&shared.airplay).await;
                airplay.status(&mut info,&shared.airplay);
                youtube.poll(&shared.airplay).await;
                youtube.status(&mut info,&shared.airplay);
                if !playback.playing() && Instant::now() >= reclaim_at {
                    unsafe { libc::malloc_trim(0); }
                    reclaim_at=Instant::now()+Duration::from_secs(30);
                }
                let guard=&shared.ducking.volume;
                guard.set_enabled(playback.active() || shared.airplay.active.load(Ordering::Acquire));guard.activity(shared.ducking.active());
                if let Ok(value)=mixer.read() {
                    if guard.holding() {
                        let desired=shared.volume.load(Ordering::Acquire);
                        if let Some(target)=guard.restore_target(value,desired)
                            && let Ok(actual)=if target==value {Ok(value)} else {mixer.write(target)} {
                                shared.volume.store(actual,Ordering::Release);guard.restored();
                                if let Some(handle)=spirc.as_ref(){let _=handle.set_volume(actual);}
                                eprintln!("volume: restored after Alice conversation");
                        }
                    } else {
                        let previous=shared.volume.swap(value,Ordering::AcqRel);
                        if value!=previous && shared.active.load(Ordering::Acquire)
                            && let Some(handle)=spirc.as_ref() {let _=handle.set_volume(value);}
                    }
                }
                if player.is_invalid() {eprintln!("receiver: audio worker stopped");break;}
            },
        }
        let playing = playback.playing() || shared.airplay.active.load(Ordering::Acquire);
        audio_guard.set_playing(playing);
        info.soloist_available = soloist_install::available();
        info.connected = if is_soloist {
            soloist_connected
        } else {
            spirc.is_some()
        };
        info.backend_error = soloist_error.clone();
        info.native_guard = playing && audio_guard.healthy();
        publish_status(
            &shared,
            &playback,
            &failures,
            rejected,
            &mut info,
            &mut last_status,
            &mut status_buffer,
        );
    }
    airplay.stop(&shared.airplay).await;
    youtube.stop(&shared.airplay).await;
    shared.active.store(false, Ordering::Release);
    shared.running.store(false, Ordering::Release);
    if let Some(worker) = voice_watch {
        worker.abort();
    }
    native_worker.abort();
    audio_guard.set_playing(false);
    if let Some(worker) = audio_worker {
        worker.abort();
    }
    voice_control.abort();
    let _ = fs::remove_file(voice::CONTROL);
    stop_connection(&mut spirc, &mut task, &session, &shared, &mut playback).await;
    if let Some(controls) = controls {
        let _ = controls.join();
    }
    player.stop();
    if let Some(discovery) = discovery.take() {
        let _ = tokio::time::timeout(Duration::from_secs(2), discovery.shutdown()).await;
    }
    if let Some(task) = soloist_task.take() {
        let _ = task.await;
    }
    info.connected = false;
    info.native_guard = false;
    publish_status(
        &shared,
        &playback,
        &failures,
        rejected,
        &mut info,
        &mut last_status,
        &mut status_buffer,
    );
    eprintln!("receiver: stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_final_login_refusals_stop_retries() {
        let refusal =
            |reason: &str| Error::permission_denied(format!("Login failed with reason: {reason}"));
        assert!(matches!(
            Rejected::of(&refusal("Bad credentials")),
            Some(Rejected::Credentials)
        ));
        assert!(matches!(
            Rejected::of(&refusal("Premium account required")),
            Some(Rejected::Premium)
        ));
        assert!(Rejected::of(&refusal("Try another access point")).is_none());
        assert!(
            Rejected::of(&Error::unavailable(
                "Login failed with reason: Bad credentials"
            ))
            .is_none()
        );
    }
    #[test]
    fn youtube_on_the_external_gate_is_not_reported_as_airplay() {
        let (commands, _receiver) = mpsc::unbounded_channel();
        let shared = Shared {
            running: AtomicBool::new(true),
            active: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            volume: AtomicU16::new(0),
            commands,
            spectrum: Arc::new(spectrum::Spectrum::new(false)),
            airplay: Arc::new(airplay::Gate::new()),
            ducking: Arc::new(ducking::Ducking::new()),
        };
        let (playback, failures) = (playback::Playback::default(), recovery::Failures::default());
        let mut info = control::Status::default();
        let publish = |info: &mut control::Status| {
            publish_status(
                &shared,
                &playback,
                &failures,
                None,
                info,
                &mut vec![],
                &mut vec![],
            )
        };
        shared
            .airplay
            .hold_youtube("/nonexistent/youtube.control".into());
        publish(&mut info);
        assert!(info.youtube_active && !info.airplay_active && info.active && !info.paused);
        shared.airplay.set_youtube_progress(60_000, 195_000);
        publish(&mut info);
        assert_eq!(info.duration_ms, 195_000);
        assert!((60_000..61_000).contains(&info.position_ms));
        shared.airplay.set_youtube_paused(true);
        let frozen = shared.airplay.youtube_position().unwrap().0;
        std::thread::sleep(std::time::Duration::from_millis(30));
        publish(&mut info);
        assert!(info.youtube_active && info.paused);
        assert_eq!(info.position_ms, frozen, "a paused bar stands still");
        shared.airplay.release_youtube();
        shared.airplay.active.store(true, Ordering::Release);
        publish(&mut info);
        assert!(info.airplay_active && !info.youtube_active && !info.paused);
    }
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
            airplay: Arc::new(airplay::Gate::new()),
            ducking: Arc::new(ducking::Ducking::new()),
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
