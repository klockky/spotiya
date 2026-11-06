//! Only the compatibility runtime is distributed by Spotiya. The official client
//! is fetched over HTTPS from Spotify and staged privately on the user's station.
use crate::{
    addons::Outcome,
    installation::{self, Temporary, error},
    update,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path},
    time::Duration,
};

pub const ROOT: &str = "/data/opt/solo";
pub const URL: &str = "https://soloist-builds.spotifycdn.com/soloist_release_arm32.tar.gz";
pub const ASSET: &str = "spotiya-soloist-compat-armv7.tar.gz";
pub const SUMS: &str = "SOLOIST_SHA256SUMS";
const ARCHIVE_LIMIT: u64 = 32 * 1024 * 1024;
const EXPANDED_LIMIT: u64 = 48 * 1024 * 1024;
const REQUIRED: &[&str] = &[
    "ld.so",
    "lib/libc.so.6",
    "lib/libm.so.6",
    "lib/libdl.so.2",
    "lib/libpthread.so.0",
    "lib/librt.so.1",
    "lib/libresolv.so.2",
    "lib/libnss_dns.so.2",
    "lib/libnss_files.so.2",
    "lib/libatomic.so.1",
    "lib/libpulse.so.0",
    "run.sh",
    "set-key.sh",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u32,
    target: String,
    spotiya_version: String,
    files: BTreeMap<String, String>,
}
#[derive(Serialize, Deserialize)]
struct Installed {
    format: u32,
    owner: String,
    source: String,
    archive_sha256: String,
    original_sha256: String,
    installed_sha256: String,
}
fn hex(hash: [u8; 32]) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn available() -> bool {
    Path::new(ROOT).join("soloist").is_file() && Path::new(ROOT).join("run.sh").is_file()
}
fn regular(path: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|m| m.is_file())
}
fn owned(root: &Path, base: &Path) -> bool {
    update::limited_file(&root.join("installed.json"), 4096)
        .ok()
        .and_then(|text| serde_json::from_str::<Installed>(&text).ok())
        .is_some_and(|m| m.format == 1 && Path::new(&m.owner) == base && m.source == URL)
}
pub fn uninstall_owned(base: &Path) -> io::Result<()> {
    if owned(Path::new(ROOT), base) {
        fs::remove_dir_all(ROOT)?;
    }
    Ok(())
}
pub fn remove(base: &Path) -> io::Result<()> {
    let _lock = crate::file_lock(&base.join("receiver-state"), "install.lock")?;
    let state = base.join("receiver-state");
    let _supervisor = crate::file_lock(&state, "supervisor.lock")
        .map_err(|_| error("Stop Spotiya before removing Soloist"))?;
    let _receiver = crate::file_lock(&state, "receiver.lock")
        .map_err(|_| error("Stop Spotiya before removing Soloist"))?;
    if !owned(Path::new(ROOT), base) {
        return Err(error(
            "Soloist is not a managed add-on; manual files were kept",
        ));
    }
    fs::remove_dir_all(ROOT)
}

fn extract(archive: &Path, destination: &Path, compat: bool, space: u64) -> io::Result<()> {
    let mut tar = tar::Archive::new(
        flate2::read::GzDecoder::new(fs::File::open(archive)?).take(EXPANDED_LIMIT + 1024 * 1024),
    );
    let mut total = 0u64;
    for (index, item) in tar.entries()?.enumerate() {
        if index >= 256 {
            return Err(error("Too many Soloist archive entries"));
        }
        let mut item = item?;
        let path = item.path()?.into_owned();
        if path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(error("Unsafe Soloist archive path"));
        }
        let path = if compat {
            path.strip_prefix("spotiya-soloist-compat")
                .map_err(|_| error("Invalid compatibility archive root"))?
        } else {
            &path
        };
        if item.header().entry_type().is_dir() {
            continue;
        }
        let name = path.to_str().ok_or_else(|| error("Invalid archive path"))?;
        let allowed = if compat {
            REQUIRED.contains(&name)
                || matches!(name, "manifest.json" | "sources.json")
                || (name.starts_with("licenses/") && path.components().count() == 2)
        } else {
            matches!(
                name,
                "soloist" | "CHANGELOG.md" | "THIRD_PARTY_LICENSES.txt"
            )
        };
        if !item.header().entry_type().is_file() || !allowed {
            return Err(error("Unexpected file or link in Soloist archive"));
        }
        total = total
            .checked_add(item.size())
            .ok_or_else(|| error("Archive size overflow"))?;
        if total > EXPANDED_LIMIT {
            return Err(error("Expanded Soloist archive exceeds 48 MiB"));
        }
        if total > space {
            return Err(error(
                "Not enough space for a staged Soloist update; old installation kept",
            ));
        }
        let target = destination.join(path);
        fs::create_dir_all(target.parent().unwrap())?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)?;
        io::copy(&mut item, &mut file)?;
        file.sync_all()?;
    }
    Ok(())
}
fn manifest(payload: &Path) -> io::Result<()> {
    let m: Manifest = serde_json::from_str(&update::limited_file(
        &payload.join("manifest.json"),
        16384,
    )?)
    .map_err(|_| error("Invalid compatibility manifest"))?;
    if m.format != 1
        || m.target != "armv7-unknown-linux-gnueabihf"
        || m.spotiya_version != env!("CARGO_PKG_VERSION")
        || m.files.len() != REQUIRED.len()
    {
        return Err(error(
            "Compatibility package does not match this Spotiya version",
        ));
    }
    for name in REQUIRED {
        let expected = m
            .files
            .get(*name)
            .ok_or_else(|| error("Incomplete compatibility manifest"))?;
        let path = payload.join(name);
        if !regular(&path) || hex(update::sha256(&path)?) != *expected {
            return Err(error("Compatibility file SHA256 mismatch"));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

// Read only the ELF header and interpreter, not the entire ~28 MiB executable.
fn interpreter(file: &Path) -> io::Result<()> {
    let mut f = OpenOptions::new().read(true).write(true).open(file)?;
    let len = f.metadata()?.len();
    let mut header = [0u8; 52];
    f.read_exact(&mut header)?;
    if &header[..6] != b"\x7fELF\x01\x01" || header[18..20] != [40, 0] {
        return Err(error("Spotify download is not an ARM32 ELF"));
    }
    let u32at = |bytes: &[u8], at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as u64;
    let offset = u32at(&header, 28);
    let stride = u16::from_le_bytes([header[42], header[43]]) as u64;
    let count = u16::from_le_bytes([header[44], header[45]]) as u64;
    if stride != 32
        || count > 128
        || offset
            .checked_add(stride * count)
            .is_none_or(|end| end > len)
    {
        return Err(error("Invalid ELF program headers"));
    }
    let mut location = None;
    for index in 0..count {
        f.seek(SeekFrom::Start(offset + index * stride))?;
        let mut ph = [0u8; 32];
        f.read_exact(&mut ph)?;
        if u32at(&ph, 0) != 3 {
            continue;
        }
        if location.is_some() {
            return Err(error("Multiple ELF interpreters"));
        }
        location = Some((u32at(&ph, 4), u32at(&ph, 16)));
    }
    let (offset, size) = location.ok_or_else(|| error("ELF interpreter missing"))?;
    let replacement = b"/data/opt/solo/ld.so\0";
    if size < replacement.len() as u64
        || size > 256
        || offset < 52
        || offset.checked_add(size).is_none_or(|end| end > len)
    {
        return Err(error("ELF interpreter path does not fit"));
    }
    let mut path = vec![0; size as usize];
    path[..replacement.len()].copy_from_slice(replacement);
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(&path)?;
    f.sync_all()
}
fn validate(payload: &Path) -> io::Result<()> {
    let libs = format!(
        "{}:/system/vendor/quasar/libs:/usr/lib:/lib",
        payload.join("lib").display()
    );
    let bin = payload.join("soloist");
    let output = installation::command(
        &payload.join("ld.so"),
        &[
            "--library-path".as_ref(),
            libs.as_ref(),
            bin.as_os_str(),
            "--version".as_ref(),
        ],
        Duration::from_secs(10),
    )?;
    if !output.status.success()
        || !String::from_utf8_lossy(&output.stdout)
            .to_ascii_lowercase()
            .contains("soloist")
    {
        return Err(error(
            "Official Soloist cannot run with this compatibility runtime",
        ));
    }
    Ok(())
}
fn commit(
    payload: &Path,
    target: &Path,
    rename: impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<Outcome> {
    let installed = target.symlink_metadata().is_ok();
    if installed && !target.symlink_metadata()?.is_dir() {
        return Err(error("Soloist location is not a directory"));
    }
    if installed {
        let old = target.with_file_name(format!(".solo-previous-{}", uuid::Uuid::new_v4()));
        fs::rename(target, &old)?;
        if let Err(e) = rename(payload, target) {
            if let Err(restore) = rename(&old, target) {
                return Err(error(format!(
                    "{e}; previous Soloist kept at {} ({restore})",
                    old.display()
                )));
            }
            return Err(e);
        }
        let _ = fs::remove_dir_all(old);
    } else {
        rename(payload, target)?;
    }
    fs::File::open(target.parent().unwrap())?.sync_all()?;
    Ok(if installed {
        Outcome::Updated
    } else {
        Outcome::Installed
    })
}

pub async fn install(
    base: &Path,
    archive: &Path,
    sums: &str,
    mut progress: impl FnMut(String),
) -> io::Result<Outcome> {
    if !cfg!(target_arch = "arm") {
        return Err(error("Install Soloist on the ARM station"));
    }
    if update::sha256(archive)? != update::checksum(sums, ASSET)? {
        return Err(error("Compatibility archive SHA256 mismatch"));
    }
    let _lock = crate::file_lock(&base.join("receiver-state"), "install.lock")
        .map_err(|_| error("Another installation is running"))?;
    let root = Path::new(ROOT);
    let stage = Temporary::new(root.parent().unwrap(), "solo-install")?;
    let payload = stage.0.join("runtime");
    fs::create_dir(&payload)?;
    let space = installation::free_bytes(&stage.0)?.saturating_sub(2 * 1024 * 1024);
    extract(archive, &payload, true, space)?;
    manifest(&payload)?;
    let download = Temporary::new(Path::new("/tmp"), "solo-download")?;
    if installation::free_bytes(&download.0)? < ARCHIVE_LIMIT + 2 * 1024 * 1024 {
        return Err(error("Not enough temporary space for Spotify download"));
    }
    progress("Downloading official Soloist directly from Spotify...".into());
    let client = reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(io::Error::other)?;
    let official = download.0.join("spotify.tar.gz");
    let mut last = 0;
    update::download_progress(&client, URL, &official, ARCHIVE_LIMIT, |n, total| {
        let mb = n / (1024 * 1024);
        if mb != last {
            last = mb;
            progress(format!(
                "Spotify: {} KiB{}",
                n / 1024,
                total
                    .map(|t| format!(" / {} KiB", t / 1024))
                    .unwrap_or_default()
            ));
        }
    })
    .await?;
    progress("Verifying ARM client and installing Soloist...".into());
    extract(
        &official,
        &payload,
        false,
        installation::free_bytes(&stage.0)?.saturating_sub(2 * 1024 * 1024),
    )?;
    for name in ["soloist", "CHANGELOG.md", "THIRD_PARTY_LICENSES.txt"] {
        if !regular(&payload.join(name)) {
            return Err(error("Incomplete official Spotify archive"));
        }
    }
    let original_sha256 = hex(update::sha256(&payload.join("soloist"))?);
    interpreter(&payload.join("soloist"))?;
    fs::set_permissions(payload.join("soloist"), fs::Permissions::from_mode(0o755))?;
    let info = Installed {
        format: 1,
        owner: base.to_string_lossy().into_owned(),
        source: URL.into(),
        archive_sha256: hex(update::sha256(&official)?),
        original_sha256,
        installed_sha256: hex(update::sha256(&payload.join("soloist"))?),
    };
    validate(&payload)?;
    fs::write(
        payload.join("installed.json"),
        serde_json::to_vec_pretty(&info).map_err(io::Error::other)?,
    )?;
    if owned(root, base)
        && fs::read(root.join("installed.json")).ok()
            == fs::read(payload.join("installed.json")).ok()
        && fs::read(root.join("manifest.json")).ok() == fs::read(payload.join("manifest.json")).ok()
        && manifest(root).is_ok()
        && update::sha256(&root.join("soloist")).is_ok_and(|h| hex(h) == info.installed_sha256)
    {
        return Ok(Outcome::Current);
    }
    commit(&payload, root, |a, b| fs::rename(a, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp() -> Temporary {
        Temporary::new(&std::env::temp_dir(), "soloist-install-test").unwrap()
    }
    fn archive(path: &Path, files: &[(&str, &[u8])]) {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            fs::File::create(path).unwrap(),
            flate2::Compression::fast(),
        ));
        for (name, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, name, *data).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
    }
    #[test]
    fn interpreter_changes_only_reserved_path_and_rejects_bad_headers() {
        let dir = temp();
        let path = dir.0.join("client");
        let mut elf = vec![0u8; 256];
        elf[..6].copy_from_slice(b"\x7fELF\x01\x01");
        elf[18] = 40;
        elf[28..32].copy_from_slice(&52u32.to_le_bytes());
        elf[42] = 32;
        elf[44] = 1;
        elf[52..56].copy_from_slice(&3u32.to_le_bytes());
        elf[56..60].copy_from_slice(&128u32.to_le_bytes());
        elf[68..72].copy_from_slice(&32u32.to_le_bytes());
        elf[128..160].fill(0x61);
        fs::write(&path, &elf).unwrap();
        interpreter(&path).unwrap();
        let patched = fs::read(&path).unwrap();
        assert_eq!(&patched[..128], &elf[..128]);
        assert_eq!(&patched[160..], &elf[160..]);
        assert!(patched[128..].starts_with(b"/data/opt/solo/ld.so\0"));
        elf[68..72].copy_from_slice(&500u32.to_le_bytes());
        fs::write(&path, &elf).unwrap();
        assert!(interpreter(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), elf);
        elf[18] = 62;
        fs::write(&path, &elf).unwrap();
        assert!(interpreter(&path).is_err());
    }
    #[test]
    fn compatibility_archive_cannot_contain_client_or_keys() {
        for name in ["soloist", "api-key", "../escape", "lib/not-allowed.so"] {
            let dir = temp();
            let tar = dir.0.join("package.gz");
            let out = dir.0.join("out");
            fs::create_dir(&out).unwrap();
            if name.contains("..") {
                continue;
            } // tar::Builder itself rejects traversal.
            archive(&tar, &[(&format!("spotiya-soloist-compat/{name}"), b"bad")]);
            assert!(extract(&tar, &out, true, 10000).is_err());
        }
    }
    #[test]
    fn official_archive_rejects_unknown_duplicate_and_oversized_files() {
        let dir = temp();
        let path = dir.0.join("spotify.gz");
        for (index, files) in [
            vec![("soloist", &b"elf"[..]), ("run.sh", &b"bad"[..])],
            vec![("soloist", &b"one"[..]), ("soloist", &b"two"[..])],
        ]
        .iter()
        .enumerate()
        {
            let out = dir.0.join(index.to_string());
            fs::create_dir(&out).unwrap();
            archive(&path, files);
            assert!(extract(&path, &out, false, 100).is_err());
        }
        let out = dir.0.join("small");
        fs::create_dir(&out).unwrap();
        archive(&path, &[("soloist", b"12345")]);
        assert!(extract(&path, &out, false, 4).is_err());
    }
    #[test]
    fn failed_swap_restores_old_runtime_and_double_failure_preserves_backup() {
        for double in [false, true] {
            let dir = temp();
            let target = dir.0.join("solo");
            let stage = dir.0.join("new");
            fs::create_dir(&target).unwrap();
            fs::create_dir(&stage).unwrap();
            fs::write(target.join("soloist"), "old").unwrap();
            fs::write(stage.join("soloist"), "new").unwrap();
            let calls = std::cell::Cell::new(0);
            let e = commit(&stage, &target, |a, b| {
                calls.set(calls.get() + 1);
                if double || calls.get() == 1 {
                    Err(error("injected"))
                } else {
                    fs::rename(a, b)
                }
            })
            .unwrap_err();
            if double {
                let previous = fs::read_dir(&dir.0)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .find(|p| {
                        p.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".solo-previous-")
                    })
                    .unwrap();
                assert_eq!(fs::read(previous.join("soloist")).unwrap(), b"old");
                assert!(e.to_string().contains(previous.to_str().unwrap()));
            } else {
                assert_eq!(fs::read(target.join("soloist")).unwrap(), b"old");
            }
        }
    }
    #[test]
    fn manifest_checks_every_runtime_file_and_version() {
        let dir = temp();
        let mut hashes = BTreeMap::new();
        for name in REQUIRED {
            let path = dir.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"fixture").unwrap();
            hashes.insert(*name, hex(update::sha256(&path).unwrap()));
        }
        let mut doc = serde_json::json!({"format":1,"target":"armv7-unknown-linux-gnueabihf", "spotiya_version":env!("CARGO_PKG_VERSION"),"files":hashes});
        let write = |doc: &serde_json::Value| {
            fs::write(
                dir.0.join("manifest.json"),
                serde_json::to_vec(doc).unwrap(),
            )
            .unwrap()
        };
        write(&doc);
        manifest(&dir.0).unwrap();
        fs::write(dir.0.join("run.sh"), b"changed").unwrap();
        assert!(manifest(&dir.0).is_err());
        fs::write(dir.0.join("run.sh"), b"fixture").unwrap();
        doc["spotiya_version"] = "0.0.0".into();
        write(&doc);
        assert!(manifest(&dir.0).is_err());
    }

    #[test]
    fn removal_requires_both_supervisor_and_receiver_to_stop() {
        let dir = temp();
        for lock in ["supervisor.lock", "receiver.lock"] {
            let _held = crate::file_lock(&dir.0.join("receiver-state"), lock).unwrap();
            assert!(
                remove(&dir.0)
                    .unwrap_err()
                    .to_string()
                    .contains("Stop Spotiya")
            );
        }
    }

    #[test]
    fn successful_swap_keeps_state_outside_runtime() {
        let dir = temp();
        let target = dir.0.join("solo");
        let stage = dir.0.join("new");
        let state = dir.0.join("receiver-state");
        fs::create_dir(&state).unwrap();
        fs::write(state.join("api-key"), "test-private").unwrap();
        fs::create_dir(&target).unwrap();
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join("soloist"), "new").unwrap();
        assert_eq!(
            commit(&stage, &target, |a, b| fs::rename(a, b)).unwrap(),
            Outcome::Updated
        );
        assert_eq!(fs::read(state.join("api-key")).unwrap(), b"test-private");
        assert_eq!(fs::read(target.join("soloist")).unwrap(), b"new");
    }
}
