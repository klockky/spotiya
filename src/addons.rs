use crate::{
    control::Status,
    installation::{self, Temporary, error},
    update,
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    time::Duration,
};

const MAX_ARCHIVE: u64 = 16 * 1024 * 1024;
const MAX_EXPANDED: u64 = 32 * 1024 * 1024;
pub const AIRPLAY_BINS: &[&str] = &["shairport-sync", "nqptp", "avahi-daemon", "dbus-daemon"];

pub struct Addon {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub asset: &'static str,
    sums: &'static str,
    root: &'static str,
    directory: &'static str,
    bins: &'static [&'static str],
    /// Receiver setting that keeps the add-on running; it must be off before removal.
    setting: Option<&'static str>,
    /// Older single-file location that still counts as installed.
    legacy: Option<&'static str>,
    pub command: Option<&'static str>,
    pub state: fn(&Status) -> State<'_>,
}
pub struct State<'a> {
    pub installed: bool,
    pub enabled: bool,
    pub running: bool,
    pub error: &'a str,
}
impl State<'_> {
    pub fn label(&self, on_demand: bool) -> &'static str {
        if !self.installed {
            "Not installed"
        } else if on_demand {
            "Ready (on demand)"
        } else if !self.enabled {
            "Off"
        } else if !self.error.is_empty() {
            "Error"
        } else if self.running {
            "Running"
        } else {
            "Starting"
        }
    }
}
pub static CATALOG: &[Addon] = &[
    Addon {
        id: "airplay",
        name: "AirPlay 2",
        description: "Receive audio from iPhone, iPad and Mac. Disabled services use no RAM.",
        asset: "spotiya-airplay2-armv7.tar.gz",
        sums: "AIRPLAY_SHA256SUMS",
        root: "spotiya-airplay2",
        directory: "airplay",
        bins: AIRPLAY_BINS,
        setting: Some("airplay-enabled"),
        legacy: None,
        command: Some("AIRPLAY"),
        state: |s| State {
            installed: s.airplay_available,
            enabled: s.airplay_enabled,
            running: s.airplay_running,
            error: &s.airplay_error,
        },
    },
    Addon {
        id: "playplay",
        name: "PlayPlay",
        description: "Optional Spotify decoder. Used automatically only when ordinary audio keys are refused.",
        asset: "spotiya-playplay-armv7.tar.gz",
        sums: "PLAYPLAY_SHA256SUMS",
        root: "spotiya-playplay",
        directory: "playplay",
        bins: &["playplay-decoder"],
        setting: None,
        legacy: Some("playplay-decoder"),
        command: None,
        state: |s| State {
            installed: s.playplay_available,
            enabled: true,
            running: false,
            error: "",
        },
    },
    Addon {
        id: "youtube",
        name: "YouTube",
        description: "Cast from the YouTube and YouTube Music apps, or paste a link. Disabled when off.",
        asset: "spotiya-youtube-armv7.tar.gz",
        sums: "YOUTUBE_SHA256SUMS",
        root: "spotiya-youtube",
        directory: "youtube",
        bins: &["spotiya-youtube"],
        setting: Some("youtube-enabled"),
        legacy: None,
        command: Some("YOUTUBE"),
        state: |s| State {
            installed: s.youtube_available,
            enabled: s.youtube_enabled,
            running: s.youtube_running,
            error: &s.youtube_error,
        },
    },
    Addon {
        id: "soloist",
        name: "Spotify Soloist",
        description: "Official client from Spotify; compatibility runtime from GitHub. Personal API key required.",
        asset: crate::soloist_install::ASSET,
        sums: crate::soloist_install::SUMS,
        root: "spotiya-soloist-compat",
        directory: "soloist",
        bins: &[],
        setting: None,
        legacy: None,
        command: None,
        state: |s| State {
            installed: s.soloist_available,
            enabled: s.spotify_backend == "soloist",
            running: s.spotify_backend == "soloist" && s.connected,
            error: if s.spotify_backend == "soloist" {
                &s.backend_error
            } else {
                ""
            },
        },
    },
];
pub fn find(id: &str) -> io::Result<&'static Addon> {
    CATALOG
        .iter()
        .find(|a| a.id == id)
        .ok_or_else(|| error("Unknown add-on"))
}
pub fn airplay_available(base: &Path) -> bool {
    AIRPLAY_BINS.iter().all(|name| {
        fs::metadata(base.join("airplay/bin").join(name))
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}
pub fn youtube_available(base: &Path) -> bool {
    crate::youtube::available(base)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u32,
    target: String,
    spotiya_version: String,
    files: BTreeMap<String, String>,
}
fn allowed(addon: &Addon, path: &Path) -> bool {
    let Some(p) = path.to_str() else {
        return false;
    };
    // Packages up to 0.9.3 carried a manual installer; it stays in the stage and is never installed.
    if p == "install.sh" {
        return true;
    }
    let Some(p) = p.strip_prefix(&format!("{}/", addon.directory)) else {
        return false;
    };
    matches!(
        p,
        "manifest.json"
            | "SHA256SUMS"
            | "sources.json"
            | "revisions.json"
            | "avahi-no-reverse.patch"
    ) || addon.bins.iter().any(|b| p == format!("bin/{b}"))
        || (p.starts_with("licenses/") && path.components().count() == 4)
}
fn extract(addon: &Addon, archive: &Path, stage: &Path, space: u64) -> io::Result<PathBuf> {
    let input = fs::File::open(archive)?;
    let mut tar =
        tar::Archive::new(flate2::read::GzDecoder::new(input).take(MAX_EXPANDED + 1024 * 1024));
    let mut total = 0u64;
    for (i, entry) in tar.entries()?.enumerate() {
        if i >= 256 {
            return Err(error("Too many add-on archive entries"));
        }
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
            || !path.starts_with(addon.root)
        {
            return Err(error("Unsafe add-on archive path"));
        }
        let relative = path
            .strip_prefix(addon.root)
            .map_err(|_| error("Invalid add-on root"))?;
        if entry.header().entry_type().is_dir() {
            continue;
        }
        if !entry.header().entry_type().is_file() || !allowed(addon, relative) {
            return Err(error("Unexpected file or link in add-on archive"));
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| error("Add-on size overflow"))?;
        if total > MAX_EXPANDED {
            return Err(error("Expanded add-on exceeds 32 MiB"));
        }
        if total > space {
            return Err(error("Not enough disk space for add-on"));
        }
        let target = stage.join(relative);
        fs::create_dir_all(target.parent().unwrap())?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(target)?;
        io::copy(&mut entry, &mut output)?;
        output.sync_all()?;
    }
    Ok(stage.join(addon.directory))
}
fn validate_manifest(addon: &Addon, payload: &Path) -> io::Result<()> {
    let text = update::limited_file(&payload.join("manifest.json"), 16384)?;
    let manifest: Manifest =
        serde_json::from_str(&text).map_err(|_| error("Invalid add-on manifest"))?;
    if manifest.format != 1
        || manifest.target != "armv7-unknown-linux-gnueabihf"
        || update::parse_version(&manifest.spotiya_version)?
            != update::parse_version(env!("CARGO_PKG_VERSION"))?
        || manifest.files.len() != addon.bins.len()
    {
        return Err(error(
            "Add-on does not match this Spotiya version or architecture",
        ));
    }
    for name in addon.bins {
        let path = format!("bin/{name}");
        let expected = manifest
            .files
            .get(&path)
            .ok_or_else(|| error("Incomplete add-on manifest"))?;
        let sum = update::checksum(&format!("{expected}  {path}"), &path)?;
        let file = payload.join(&path);
        if update::sha256(&file)? != sum {
            return Err(error("Add-on binary SHA256 mismatch"));
        }
        fs::set_permissions(file, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}
fn validate_binaries(addon: &Addon, payload: &Path) -> io::Result<()> {
    for name in addon.bins {
        let file = payload.join("bin").join(name);
        let mut header = [0; 20];
        fs::File::open(&file)?.read_exact(&mut header)?;
        if &header[..6] != b"\x7fELF\x01\x01" || header[18..20] != [40, 0] {
            return Err(error("Add-on requires ARMv7 Linux executables"));
        }
        let argument = if *name == "playplay-decoder" {
            "--info"
        } else if matches!(*name, "shairport-sync" | "nqptp") {
            "-V"
        } else {
            "--version"
        };
        let result = installation::command(&file, &[argument.as_ref()], Duration::from_secs(5))?;
        if !result.status.success() {
            return Err(error(format!("{name} cannot run on this station")));
        }
        if *name == "playplay-decoder" {
            #[derive(Deserialize)]
            struct DecoderInfo {
                protocol_version: u32,
                network: bool,
            }
            let info: DecoderInfo = serde_json::from_slice(&result.stdout)
                .map_err(|_| error("Invalid PlayPlay decoder information"))?;
            if info.protocol_version != 1 || info.network {
                return Err(error("Unsupported PlayPlay decoder protocol"));
            }
            librespot::audio::playplay::PlayPlayDecoder::configuration(&file)
                .map_err(|_| error("PlayPlay decoder configuration check failed"))?;
        }
        if *name == "shairport-sync"
            && !String::from_utf8_lossy(&result.stdout).contains("AirPlay2")
        {
            return Err(error("The AirPlay package lacks AirPlay 2 support"));
        }
    }
    Ok(())
}
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Installed,
    Updated,
    Current,
}
impl Outcome {
    pub fn message(&self, addon: &Addon) -> String {
        if addon.id == "soloist" {
            return match self {
                Outcome::Current => "Soloist is already up to date".into(),
                Outcome::Installed => "Soloist installed.\nKey: /data/opt/solo/set-key.sh\nUse: spotiya backend soloist; spotiya restart".into(),
                Outcome::Updated => "Soloist updated; run spotiya retry (restart after switching backend)".into(),
            };
        }
        match self {
            Outcome::Installed if addon.command.is_none() => {
                format!("{} installed; ready on demand", addon.name)
            }
            Outcome::Installed => format!("{} installed", addon.name),
            // The PlayPlay resolver notices a replaced decoder; running services do not.
            Outcome::Updated if addon.command.is_none() => format!("{} updated", addon.name),
            Outcome::Updated => format!(
                "{} updated; restart the receiver to load the new version",
                addon.name
            ),
            Outcome::Current => format!("{} is already up to date", addon.name),
        }
    }
}
#[cfg(not(test))]
use fs::rename;
#[cfg(test)]
thread_local! {
    static FAIL_RENAMES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[cfg(test)]
fn rename(from: &Path, to: &Path) -> io::Result<()> {
    if FAIL_RENAMES.get() {
        return Err(error("injected rename failure"));
    }
    fs::rename(from, to)
}
fn install_verified(
    addon: &Addon,
    base: &Path,
    archive: &Path,
    sums: &str,
    validate: impl FnOnce(&Addon, &Path) -> io::Result<()>,
) -> io::Result<Outcome> {
    if fs::metadata(archive)?.len() > MAX_ARCHIVE {
        return Err(error("Add-on archive exceeds 16 MiB"));
    }
    if update::sha256(archive)? != update::checksum(sums, addon.asset)? {
        return Err(error("Add-on archive SHA256 mismatch"));
    }
    let _lock = crate::file_lock(&base.join("receiver-state"), "install.lock")
        .map_err(|_| error("Another installation is running"))?;
    let target = base.join(addon.directory);
    let installed = target.symlink_metadata().is_ok();
    let space = installation::free_bytes(base)?.saturating_sub(2 * 1024 * 1024);
    if space == 0 {
        return Err(error("Not enough disk space for add-on"));
    }
    let stage = Temporary::new(base, "addon-install")?;
    let payload = extract(addon, archive, &stage.0, space)?;
    validate_manifest(addon, &payload)?;
    if installed
        && fs::read(target.join("manifest.json")).ok()
            == Some(fs::read(payload.join("manifest.json"))?)
        && validate_manifest(addon, &target).is_ok()
    {
        return Ok(Outcome::Current);
    }
    validate(addon, &payload)?;
    if installed {
        // Running helpers keep their open files, so the old tree can move aside and be deleted.
        // It stays outside the stage: if neither rename works, it is the only copy left.
        let previous = base.join(format!(
            ".{}-previous-{}",
            addon.directory,
            uuid::Uuid::new_v4()
        ));
        fs::rename(&target, &previous)?;
        if let Err(e) = rename(&payload, &target) {
            if let Err(restore) = rename(&previous, &target) {
                return Err(error(format!(
                    "{e}; previous add-on kept at {} ({restore})",
                    previous.display()
                )));
            }
            return Err(e);
        }
        let _ = fs::remove_dir_all(previous);
    } else {
        fs::rename(payload, &target)?;
    }
    fs::File::open(base)?.sync_all()?;
    Ok(if installed {
        Outcome::Updated
    } else {
        Outcome::Installed
    })
}
pub fn remove(id: &str, base: &Path) -> io::Result<()> {
    if id == "soloist" {
        return crate::soloist_install::remove(base);
    }
    let addon = find(id)?;
    let _lock = crate::file_lock(&base.join("receiver-state"), "install.lock")
        .map_err(|_| error("Another installation is running"))?;
    if let Some(setting) = addon.setting
        && fs::read_to_string(base.join("receiver-state").join(setting))
            .is_ok_and(|s| s.trim() == "1")
    {
        return Err(error(format!("Disable {} before removing it", addon.name)));
    }
    let target = base.join(addon.directory);
    let legacy = addon.legacy.map(|name| base.join(name));
    let mut removed = false;
    if target.symlink_metadata().is_ok() {
        let stage = Temporary::new(base, "addon-remove")?;
        fs::rename(&target, stage.0.join(addon.directory))?;
        removed = true;
    }
    if let Some(legacy) = legacy
        && legacy.symlink_metadata().is_ok()
    {
        fs::remove_file(legacy)?;
        removed = true;
    }
    if !removed {
        return Err(error(format!("{} is not installed", addon.name)));
    }
    fs::File::open(base)?.sync_all()
}
pub fn local(id: &str, base: &Path, archive: &Path, sums: &Path) -> io::Result<Outcome> {
    let addon = find(id)?;
    let stage = Temporary::new(Path::new("/tmp"), "addon-download")?;
    let copy = stage.0.join(addon.asset);
    update::copy_limited(archive, &copy, MAX_ARCHIVE)?;
    if id == "soloist" {
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(crate::soloist_install::install(
                base,
                &copy,
                &update::limited_file(sums, 65536)?,
                |s| println!("{s}"),
            ));
    }
    install_verified(
        addon,
        base,
        &copy,
        &update::limited_file(sums, 65536)?,
        validate_binaries,
    )
}

pub async fn remote(
    id: &str,
    base: &Path,
    mut progress: impl FnMut(String),
) -> io::Result<Outcome> {
    if !cfg!(target_arch = "arm") {
        return Err(error("Install add-ons on the ARM station"));
    }
    let addon = find(id)?;
    if !base.join("spotiya").is_file() {
        return Err(error("Install Spotiya first"));
    }
    let version = env!("CARGO_PKG_VERSION");
    update::parse_version(version)?;
    let tag = format!("v{version}");
    progress(format!("Checking GitHub release {tag}..."));
    let client = update::client()?;
    let stage = Temporary::new(Path::new("/tmp"), "addon-download")?;
    if installation::free_bytes(&stage.0)? < MAX_ARCHIVE + 2 * 1024 * 1024 {
        return Err(error("Not enough temporary space"));
    }
    let release = update::release(&client, &format!("tags/{tag}"))
        .await?
        .ok_or_else(|| error(format!("No published add-ons for {tag} yet")))?;
    if release.draft || release.prerelease || release.tag_name != tag {
        return Err(error("Release does not match this Spotiya version"));
    }
    let archive = release
        .asset(addon.asset, MAX_ARCHIVE)
        .ok_or_else(|| error("This release has no compatible add-on package"))?;
    if release.asset(addon.sums, 65536).is_none() {
        return Err(error("Release is missing add-on checksums"));
    }
    let download = format!(
        "https://github.com/{}/releases/download/{tag}",
        update::REPO
    );
    let sums = stage.0.join(addon.sums);
    update::download_progress(
        &client,
        &format!("{download}/{}", addon.sums),
        &sums,
        65536,
        |_, _| {},
    )
    .await?;
    let path = stage.0.join(addon.asset);
    let mut last = 101;
    update::download_progress(
        &client,
        &format!("{download}/{}", addon.asset),
        &path,
        MAX_ARCHIVE,
        |n, _| {
            let percent = (n.saturating_mul(100) / archive.size).min(100);
            if percent != last {
                progress(format!(
                    "Downloading {}: {percent}% ({n}/{})",
                    addon.name, archive.size
                ));
                last = percent;
            }
        },
    )
    .await?;
    if fs::metadata(&path)?.len() != archive.size {
        return Err(error("Downloaded add-on size does not match release"));
    }
    if id == "soloist" {
        return crate::soloist_install::install(
            base,
            &path,
            &update::limited_file(&sums, 65536)?,
            progress,
        )
        .await;
    }
    progress("Verifying and installing add-on...".into());
    install_verified(
        addon,
        base,
        &path,
        &update::limited_file(&sums, 65536)?,
        validate_binaries,
    )
}

pub enum Message {
    Progress(String),
    Finished(io::Result<Outcome>),
}
pub struct InstallJob {
    pub id: &'static str,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl InstallJob {
    pub fn start(
        addon: &'static Addon,
        messages: std::sync::mpsc::Sender<Message>,
    ) -> io::Result<Self> {
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        let worker = std::thread::Builder::new().name("addon-install".into()).stack_size(512*1024).spawn(move || {
            let result = (|| {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                let result = runtime.block_on(async {
                    tokio::select! {
                        result = remote(addon.id, Path::new(installation::BASE), |text| { let _ = messages.send(Message::Progress(text)); }) => result,
                        _ = cancelled => Err(error("Add-on installation cancelled")),
                    }
                });
                runtime.shutdown_timeout(Duration::from_secs(1));
                result
            })();
            let _ = messages.send(Message::Finished(result));
        })?;
        Ok(Self {
            id: addon.id,
            cancel: Some(cancel),
            worker: Some(worker),
        })
    }
    pub fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}
impl Drop for InstallJob {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            if !worker.is_finished() {
                // eprintln! would panic on a closed stderr and skip the join below.
                let _ = io::Write::write_all(
                    &mut io::stderr(),
                    b"Waiting for the add-on installation to stop...\n",
                );
            }
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    fn fixture() -> Temporary {
        let dir = Temporary::new(&std::env::temp_dir(), "addon-test").unwrap();
        fs::create_dir(dir.0.join("receiver-state")).unwrap();
        fs::write(
            dir.0.join("receiver-state/credentials.json"),
            b"private credentials",
        )
        .unwrap();
        fs::write(dir.0.join("spotiya"), b"running receiver").unwrap();
        dir
    }
    fn archive(
        root: &Path,
        bad_manifest: bool,
        extra: Option<(&str, tar::EntryType)>,
    ) -> (PathBuf, String) {
        archive_for(&CATALOG[0], root, bad_manifest, extra)
    }
    fn archive_for(
        addon: &Addon,
        root: &Path,
        bad_manifest: bool,
        extra: Option<(&str, tar::EntryType)>,
    ) -> (PathBuf, String) {
        let path = root.join(addon.asset);
        let mut files = BTreeMap::new();
        let mut hashes = BTreeMap::new();
        for name in addon.bins {
            let data = format!("binary {name}").into_bytes();
            hashes.insert(
                format!("bin/{name}"),
                format!("{:x}", Sha256::digest(&data)),
            );
            files.insert(
                format!("{}/{}/bin/{name}", addon.root, addon.directory),
                data,
            );
        }
        let manifest = serde_json::json!({"format":1,"target":"armv7-unknown-linux-gnueabihf",
            "spotiya_version":if bad_manifest {"0.0.0"} else {env!("CARGO_PKG_VERSION")},"files":hashes});
        files.insert(
            format!("{}/{}/manifest.json", addon.root, addon.directory),
            serde_json::to_vec(&manifest).unwrap(),
        );
        let zip = flate2::write::GzEncoder::new(
            fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(zip);
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(data.len() as u64);
            header.set_cksum();
            tar.append_data(&mut header, name, &data[..]).unwrap();
        }
        if let Some((name, kind)) = extra {
            let mut header = tar::Header::new_gnu();
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            header.set_entry_type(kind);
            header.set_size(0);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, &b""[..]).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
        let sum = format!(
            "{:x}  {}\n",
            Sha256::digest(fs::read(&path).unwrap()),
            addon.asset
        );
        (path, sum)
    }
    fn unchanged(base: &Path) {
        assert_eq!(fs::read(base.join("spotiya")).unwrap(), b"running receiver");
        assert_eq!(
            fs::read(base.join("receiver-state/credentials.json")).unwrap(),
            b"private credentials"
        );
        assert!(!fs::read_dir(base).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".addon-install-")
        }));
    }
    #[test]
    fn installs_only_addon_payload_and_does_not_restart_or_replace_spotify() {
        let dir = fixture();
        let addon = &CATALOG[0];
        let (path, sums) = archive(&dir.0, false, None);
        install_verified(addon, &dir.0, &path, &sums, |_, _| Ok(())).unwrap();
        assert!(airplay_available(&dir.0));
        assert!(!dir.0.join("receiver-state/airplay-enabled").exists());
        unchanged(&dir.0);
        assert_eq!(
            install_verified(addon, &dir.0, &path, &sums, |_, _| Ok(())).unwrap(),
            Outcome::Current
        );
        assert!(airplay_available(&dir.0));
    }
    #[test]
    fn reinstalling_a_changed_package_replaces_the_previous_tree() {
        let dir = fixture();
        let addon = &CATALOG[0];
        let (path, sums) = archive(&dir.0, false, None);
        install_verified(addon, &dir.0, &path, &sums, |_, _| Ok(())).unwrap();
        fs::write(dir.0.join("airplay/bin/nqptp"), b"older binary").unwrap();
        fs::write(dir.0.join("airplay/stale"), b"previous release").unwrap();
        assert!(
            install_verified(addon, &dir.0, &path, &sums, |_, _| Err(error("cannot run"))).is_err()
        );
        assert_eq!(
            fs::read(dir.0.join("airplay/bin/nqptp")).unwrap(),
            b"older binary"
        );
        assert_eq!(
            install_verified(addon, &dir.0, &path, &sums, |_, _| Ok(())).unwrap(),
            Outcome::Updated
        );
        assert_eq!(
            fs::read(dir.0.join("airplay/bin/nqptp")).unwrap(),
            b"binary nqptp"
        );
        assert!(!dir.0.join("airplay/stale").exists());
        unchanged(&dir.0);
        fs::write(dir.0.join("airplay/bin/nqptp"), b"older binary").unwrap();
        FAIL_RENAMES.set(true);
        let result = install_verified(addon, &dir.0, &path, &sums, |_, _| Ok(()));
        FAIL_RENAMES.set(false);
        let message = result.unwrap_err().to_string();
        let kept = fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.to_string_lossy().contains(".airplay-previous-"))
            .expect(&message);
        assert!(message.contains(&*kept.to_string_lossy()), "{message}");
        assert_eq!(fs::read(kept.join("bin/nqptp")).unwrap(), b"older binary");
        assert!(!dir.0.join("airplay").exists());
    }
    #[test]
    fn removal_requires_a_disabled_service_and_clears_legacy_files() {
        let dir = fixture();
        let airplay = &CATALOG[0];
        let (path, sums) = archive(&dir.0, false, None);
        install_verified(airplay, &dir.0, &path, &sums, |_, _| Ok(())).unwrap();
        fs::write(dir.0.join("receiver-state/airplay-enabled"), "1\n").unwrap();
        assert_eq!(
            remove("airplay", &dir.0).unwrap_err().to_string(),
            "Disable AirPlay 2 before removing it"
        );
        assert!(airplay_available(&dir.0));
        fs::write(dir.0.join("receiver-state/airplay-enabled"), "0\n").unwrap();
        remove("airplay", &dir.0).unwrap();
        assert!(!dir.0.join("airplay").exists());
        assert!(remove("airplay", &dir.0).is_err());
        fs::write(dir.0.join("playplay-decoder"), b"legacy").unwrap();
        fs::set_permissions(
            dir.0.join("playplay-decoder"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(crate::playplay::available(&dir.0));
        remove("playplay", &dir.0).unwrap();
        assert!(!crate::playplay::available(&dir.0));
        unchanged(&dir.0);
        assert!(!fs::read_dir(&dir.0).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".addon-remove-")
        }));
    }
    #[test]
    fn playplay_installation_is_independent_of_airplay_and_receiver_state() {
        let dir = fixture();
        let addon = find("playplay").unwrap();
        let (path, sums) = archive_for(addon, &dir.0, false, None);
        install_verified(addon, &dir.0, &path, &sums, |_, _| Ok(())).unwrap();
        assert!(crate::playplay::available(&dir.0));
        assert!(!airplay_available(&dir.0));
        assert!(!dir.0.join("receiver-state/airplay-enabled").exists());
        unchanged(&dir.0);
    }
    #[test]
    fn extraction_uses_actual_payload_space_and_rejects_a_smaller_budget() {
        let dir = fixture();
        let addon = &CATALOG[0];
        let (path, _) = archive(&dir.0, false, None);
        let mut archive =
            tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(&path).unwrap()));
        let expanded = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().size())
            .sum::<u64>();
        assert!(expanded < 1024 * 1024);
        let enough = Temporary::new(&dir.0, "addon-space").unwrap();
        let payload = extract(addon, &path, &enough.0, expanded).unwrap();
        validate_manifest(addon, &payload).unwrap();
        let short = Temporary::new(&dir.0, "addon-space").unwrap();
        assert_eq!(
            extract(addon, &path, &short.0, expanded - 1)
                .unwrap_err()
                .to_string(),
            "Not enough disk space for add-on"
        );
        assert!(!dir.0.join("airplay").exists());
        unchanged(&dir.0);
    }

    #[test]
    fn corrupt_wrong_version_or_unrunnable_packages_leave_no_installed_payload() {
        let dir = fixture();
        let addon = &CATALOG[0];
        let (path, sums) = archive(&dir.0, true, None);
        assert!(install_verified(addon, &dir.0, &path, &sums, |_, _| Ok(())).is_err());
        let (path, sums) = archive(&dir.0, false, None);
        let unpacked = Temporary::new(&dir.0, "addon-inspect").unwrap();
        let payload = extract(addon, &path, &unpacked.0, MAX_EXPANDED).unwrap();
        fs::write(payload.join("bin/shairport-sync"), b"changed binary").unwrap();
        assert!(validate_manifest(addon, &payload).is_err());
        assert!(
            install_verified(addon, &dir.0, &path, &sums, |_, _| Err(error(
                "incompatible libc"
            )))
            .is_err()
        );
        fs::write(&path, b"corrupt").unwrap();
        assert!(install_verified(addon, &dir.0, &path, &sums, |_, _| Ok(())).is_err());
        assert!(!dir.0.join("airplay").exists());
        unchanged(&dir.0);
    }
    #[test]
    fn archives_reject_traversal_links_unexpected_files_and_duplicate_entries() {
        let addon = &CATALOG[0];
        for (name, kind) in [
            ("spotiya-airplay2/../../outside", tar::EntryType::Regular),
            (
                "spotiya-airplay2/airplay/bin/shairport-sync",
                tar::EntryType::Symlink,
            ),
            (
                "spotiya-airplay2/airplay/bin/shairport-sync",
                tar::EntryType::Regular,
            ),
            ("spotiya-airplay2/airplay/secret", tar::EntryType::Regular),
            ("outside/file", tar::EntryType::Regular),
        ] {
            let dir = fixture();
            let (path, sums) = archive(&dir.0, false, Some((name, kind)));
            assert!(
                install_verified(addon, &dir.0, &path, &sums, |_, _| Ok(())).is_err(),
                "{name}"
            );
            assert!(!dir.0.join("airplay").exists());
            unchanged(&dir.0);
        }
    }
    #[test]
    fn installation_respects_existing_lock_and_rejects_foreign_elf() {
        let dir = fixture();
        let addon = &CATALOG[0];
        let (path, sums) = archive(&dir.0, false, None);
        let lock = crate::file_lock(&dir.0.join("receiver-state"), "install.lock").unwrap();
        assert!(install_verified(addon, &dir.0, &path, &sums, |_, _| Ok(())).is_err());
        drop(lock);
        assert!(install_verified(addon, &dir.0, &path, &sums, validate_binaries).is_err());
        assert!(!dir.0.join("airplay").exists());
        unchanged(&dir.0);
    }
}
