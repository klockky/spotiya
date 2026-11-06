use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const BASE: &str = "/data/opt/spotify-connect";
pub const INIT: &str = "/etc/init.d/S99spotify-connect";
pub const RECEIVER_PID: &str = "/run/spotify-connect.pid";
pub const SUPERVISOR_PID: &str = "/run/spotiya-supervisor.pid";
pub const ASSET: &str = "spotiya-armv7-linux-gnueabihf.tar.gz";
const FILES: &[&str] = &[
    "spotiya",
    "run.sh",
    "start.sh",
    "stop.sh",
    "boot.sh",
    "service.sh",
    "cli.sh",
    "profiles/mini1.json",
    "profiles/mini2.json",
    "profiles/generic.json",
    "S99spotify-connect",
];
const KEEP: usize = 2;
pub fn error(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}
pub fn command(program: &Path, args: &[&OsStr], timeout: Duration) -> io::Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error("Command timed out"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
pub fn version(binary: &Path) -> io::Result<String> {
    let output = command(binary, &[OsStr::new("--version")], Duration::from_secs(5))?;
    let text = std::str::from_utf8(&output.stdout).map_err(|_| error("Invalid binary version"))?;
    let mut words = text.split_whitespace();
    if !output.status.success() || words.next() != Some("spotiya") {
        return Err(error("Binary version check failed"));
    }
    let value = words
        .next()
        .ok_or_else(|| error("Missing binary version"))?;
    crate::update::parse_version(value)?;
    Ok(value.into())
}
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    u64::try_from(u128::from(stat.f_bavail) * u128::from(stat.f_frsize))
        .map_err(|_| error("Filesystem capacity exceeds supported range"))
}
/// The pid in `pidfile`, if that process runs the binary installed in `base`.
pub fn live_pid(base: &Path, pidfile: &Path) -> Option<u32> {
    let pid: u32 = fs::read_to_string(pidfile).ok()?.trim().parse().ok()?;
    let expected = fs::canonicalize(base.join("spotiya")).ok()?;
    (fs::read_link(format!("/proc/{pid}/exe")).ok()? == expected).then_some(pid)
}
pub struct Temporary(pub PathBuf);
impl Temporary {
    pub fn new(parent: &Path, prefix: &str) -> io::Result<Self> {
        let path = parent.join(format!(".{prefix}-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(Self(path))
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
/// Set while this process holds a read-only root remounted read-write.
static ROOT_WRITABLE: AtomicBool = AtomicBool::new(false);
/// Release builds abort on panic without running `Drop`, so the panic hook calls this.
pub fn restore_root_on_panic() {
    if ROOT_WRITABLE.swap(false, Ordering::AcqRel) {
        let _ = RootMount::remount("remount,ro");
    }
}
struct RootMount(bool);
impl RootMount {
    fn writable() -> io::Result<Self> {
        let mounts = fs::read_to_string("/proc/mounts")?;
        let readonly = mounts.lines().any(|l| {
            let f: Vec<_> = l.split_whitespace().collect();
            f.len() > 3 && f[1] == "/" && f[3].split(',').any(|s| s == "ro")
        });
        if readonly {
            Self::remount("remount,rw")?;
            ROOT_WRITABLE.store(true, Ordering::Release);
        }
        Ok(Self(readonly))
    }
    fn remount(options: &str) -> io::Result<()> {
        let result = command(
            Path::new("/bin/mount"),
            &[OsStr::new("-o"), OsStr::new(options), OsStr::new("/")],
            Duration::from_secs(5),
        )?;
        if result.status.success() {
            Ok(())
        } else {
            Err(error("Cannot remount root filesystem"))
        }
    }
    fn restore(&mut self) -> io::Result<()> {
        if self.0 {
            Self::remount("remount,ro")?;
            self.0 = false;
            ROOT_WRITABLE.store(false, Ordering::Release);
        }
        Ok(())
    }
}
impl Drop for RootMount {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
#[derive(Clone)]
struct Layout {
    base: PathBuf,
    init: PathBuf,
    cli: PathBuf,
}
impl Layout {
    fn system() -> Self {
        Self {
            base: BASE.into(),
            init: INIT.into(),
            cli: "/usr/bin/spotiya".into(),
        }
    }
    fn destination(&self, file: &str) -> PathBuf {
        if file == "S99spotify-connect" {
            self.init.clone()
        } else {
            self.base.join(file)
        }
    }
}
trait Service {
    fn stop(&mut self) -> io::Result<()>;
    fn start(&mut self) -> io::Result<()>;
    fn ready(&mut self, profile: &str) -> io::Result<()>;
}
struct SystemService;
impl Service for SystemService {
    fn stop(&mut self) -> io::Result<()> {
        service("stop")
    }
    fn start(&mut self) -> io::Result<()> {
        service("start")
    }
    fn ready(&mut self, profile: &str) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut ready_since = None;
        while Instant::now() < deadline {
            let pid = live_pid(Path::new(BASE), Path::new(RECEIVER_PID));
            let status = fs::read(crate::control::STATUS)
                .ok()
                .and_then(|b| serde_json::from_slice::<crate::control::Status>(&b).ok());
            if pid.is_some()
                && status.is_some_and(|s| Some(s.receiver_pid) == pid && s.profile == profile)
            {
                if ready_since.get_or_insert_with(Instant::now).elapsed() >= Duration::from_secs(1)
                {
                    return Ok(());
                }
            } else {
                ready_since = None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(error("Receiver did not become ready after installation"))
    }
}
fn service(action: &str) -> io::Result<()> {
    let result = command(
        Path::new(INIT),
        &[OsStr::new(action)],
        Duration::from_secs(12),
    )?;
    if result.status.success() {
        Ok(())
    } else {
        Err(error(format!("Receiver {action} failed")))
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    files: Vec<String>,
    cli_existed: bool,
}
fn copy_atomic(source: &Path, destination: &Path) -> io::Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| error("Invalid install destination"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".spotiya-copy-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        fs::copy(source, &temporary)?;
        fs::File::open(&temporary)?.sync_all()?;
        fs::rename(&temporary, destination)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
fn source(root: &Path, binary: &Path, file: &str) -> PathBuf {
    if file == "spotiya" {
        binary.into()
    } else if file.starts_with("profiles/") {
        root.join(file)
    } else {
        root.join("packaging").join(file)
    }
}
fn sequence(name: &str) -> Option<u128> {
    name.strip_prefix("managed-")?
        .split('-')
        .next()?
        .parse()
        .ok()
}
/// The station has no battery clock, so wall time can jump back across a reboot before
/// NTP syncs. Backups are numbered past every existing one so the newest never sorts first.
fn next_sequence(parent: &Path) -> io::Result<u128> {
    let mut next = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for entry in fs::read_dir(parent)? {
        if let Some(existing) = entry?.file_name().to_str().and_then(sequence) {
            next = next.max(existing + 1);
        }
    }
    Ok(next)
}
fn snapshot(layout: &Layout) -> io::Result<PathBuf> {
    let parent = layout.base.join("backups");
    fs::create_dir_all(&parent)?;
    let path = parent.join(format!(
        "managed-{:020}-{}",
        next_sequence(&parent)?,
        uuid::Uuid::new_v4()
    ));
    fs::create_dir(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    let mut manifest = Manifest {
        version: 1,
        files: vec![],
        cli_existed: fs::symlink_metadata(&layout.cli).is_ok(),
    };
    let result = (|| {
        for &file in FILES {
            let from = layout.destination(file);
            if from.exists() {
                copy_atomic(&from, &path.join(file))?;
                manifest.files.push(file.into());
            }
        }
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path.join("manifest.json"))?;
        f.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
        f.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&path);
    }
    result.map(|()| path)
}
fn restore(layout: &Layout, backup: &Path) -> io::Result<()> {
    let manifest: Manifest = serde_json::from_slice(&fs::read(backup.join("manifest.json"))?)?;
    if manifest.version != 1 || manifest.files.iter().any(|f| !FILES.contains(&f.as_str())) {
        return Err(error("Invalid backup manifest"));
    }
    // Keep restoring after a failed file so one bad path does not leave a mixed install.
    let mut failed = vec![];
    for &file in FILES {
        let destination = layout.destination(file);
        let result = if manifest.files.iter().any(|f| f == file) {
            copy_atomic(&backup.join(file), &destination)
        } else if destination.exists() {
            fs::remove_file(destination)
        } else {
            Ok(())
        };
        if let Err(e) = result {
            failed.push(format!("{file}: {e}"));
        }
    }
    if !manifest.cli_existed
        && fs::symlink_metadata(&layout.cli).is_ok()
        && let Err(e) = fs::remove_file(&layout.cli)
    {
        failed.push(format!("{}: {e}", layout.cli.display()));
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(error(format!("Could not restore {}", failed.join("; "))))
    }
}
fn prune(base: &Path, keep: usize) -> io::Result<()> {
    let mut backups = vec![];
    let entries = match fs::read_dir(base.join("backups")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for item in entries {
        let item = item?;
        if !item.file_name().to_string_lossy().starts_with("managed-")
            || !item.file_type()?.is_dir()
        {
            continue;
        }
        let manifest = fs::read(item.path().join("manifest.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Manifest>(&b).ok());
        if manifest.is_some_and(|m| m.version == 1)
            && let Some(order) = item.file_name().to_str().and_then(sequence)
        {
            backups.push((order, item.path()));
        }
    }
    backups.sort();
    let remove = backups.len().saturating_sub(keep);
    for (_, path) in backups.into_iter().take(remove) {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}
fn transaction(
    layout: &Layout,
    root: &Path,
    binary: &Path,
    running: bool,
    profile: &str,
    service: &mut impl Service,
) -> io::Result<PathBuf> {
    let (new_bytes, old_bytes) =
        FILES
            .iter()
            .try_fold((0u64, 0u64), |(new, old), &f| -> io::Result<_> {
                let from = source(root, binary, f);
                if !fs::symlink_metadata(&from)?.is_file() {
                    return Err(error("Package contains a non-regular file"));
                }
                Ok((
                    new + fs::metadata(from)?.len(),
                    old + fs::metadata(layout.destination(f)).map_or(0, |m| m.len()),
                ))
            })?;
    // Leave room for the new snapshot while retaining one completed older backup.
    prune(&layout.base, KEEP - 1)?;
    let needed = new_bytes + old_bytes + new_bytes.max(old_bytes) + 2 * 1024 * 1024;
    if free_bytes(&layout.base)? < needed {
        return Err(error(
            "Not enough disk space for staging and rollback backup",
        ));
    }
    let stage = Temporary::new(&layout.base, "install")?;
    for &file in FILES {
        let from = source(root, binary, file);
        if !fs::symlink_metadata(&from)?.is_file() {
            return Err(error("Package contains a non-regular file"));
        }
        copy_atomic(&from, &stage.0.join(file))?;
        fs::set_permissions(
            stage.0.join(file),
            fs::Permissions::from_mode(if file.starts_with("profiles/") {
                0o644
            } else {
                0o755
            }),
        )?;
    }
    let backup = snapshot(layout)?;
    let had_service = layout.init.exists();
    if running
        && had_service
        && let Err(e) = service.stop()
    {
        // Nothing was replaced yet; bring back whatever part of the service did stop.
        return Err(error(match service.start() {
            Ok(()) => format!("Installation aborted: {e}"),
            Err(start) => {
                format!("Installation aborted: {e}; receiver restart also failed: {start}")
            }
        }));
    }
    let result: io::Result<()> = (|| {
        for &file in FILES {
            if file == "S99spotify-connect" {
                copy_atomic(&stage.0.join(file), &layout.destination(file))?;
            } else {
                if let Some(parent) = layout.destination(file).parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(stage.0.join(file), layout.destination(file))?;
            }
        }
        if fs::symlink_metadata(&layout.cli).is_err() {
            symlink(layout.base.join("cli.sh"), &layout.cli)?;
        }
        if running {
            service.start()?;
            service.ready(profile)?;
        }
        Ok(())
    })();
    if let Err(cause) = result {
        // A receiver that did not stop is still the new build; starting would only find it
        // running and report it ready, so the restored files wait for the next start.
        let stopped = service.stop();
        let restored = restore(layout, &backup);
        // Start the receiver even after a partial restore; whatever is on disk beats nothing.
        let started = match &stopped {
            Err(e) if running => Err(error(format!("receiver did not stop: {e}"))),
            _ if running && had_service => service.start().and_then(|()| service.ready(profile)),
            _ => Ok(()),
        };
        let problems: Vec<String> = [restored, started]
            .into_iter()
            .filter_map(|r| r.err().map(|e| e.to_string()))
            .collect();
        let rollback = if problems.is_empty() {
            "succeeded".into()
        } else {
            format!("needs attention ({})", problems.join("; "))
        };
        return Err(error(format!(
            "Installation failed: {cause}. Rollback {rollback}. Backup: {}",
            backup.display()
        )));
    }
    if let Err(e) = prune(&layout.base, KEEP) {
        eprintln!("Backup cleanup failed: {e}");
    }
    Ok(backup)
}
pub fn install(root: &Path, binary: &Path, newer_only: bool) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(error("Run the installer as root on the speaker"));
    }
    let layout = Layout::system();
    let state = layout.base.join("receiver-state");
    let _lock = crate::file_lock(&state, "install.lock")?;
    let _selection_lock = crate::file_lock(&state, "profile-selection.lock")?;
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?;
    if fs::symlink_metadata(&layout.cli).is_ok()
        && fs::read_link(&layout.cli)
            .ok()
            .is_none_or(|p| p != layout.base.join("cli.sh"))
    {
        return Err(error("/usr/bin/spotiya belongs to another installation"));
    }
    let version = version(binary)?;
    if newer_only
        && let Ok(installed) = self::version(&layout.base.join("spotiya"))
        && crate::update::parse_version(&installed)? >= crate::update::parse_version(&version)?
    {
        println!("An equal or newer version is already installed; installation skipped.");
        return Ok(());
    }
    let result = command(
        binary,
        &[
            OsStr::new("--state-dir"),
            state.as_os_str(),
            OsStr::new("--profile-info"),
        ],
        Duration::from_secs(5),
    )?;
    if !result.status.success() {
        return Err(error("Candidate rejected the saved device profile"));
    }
    let profile: serde_json::Value = serde_json::from_slice(&result.stdout)?;
    let profile = profile["profile"]["id"]
        .as_str()
        .ok_or_else(|| error("Candidate returned an invalid profile"))?;
    let installed = layout.base.join("spotiya").exists();
    let running = !installed
        || live_pid(&layout.base, Path::new(RECEIVER_PID)).is_some()
        || live_pid(&layout.base, Path::new(SUPERVISOR_PID)).is_some();
    let mut mount = RootMount::writable()?;
    let result = transaction(&layout, root, binary, running, profile, &mut SystemService);
    // The transaction outcome matters more than the remount, so neither error hides the other.
    let remount = mount.restore();
    let backup = match (result, remount) {
        (Ok(backup), Ok(())) => backup,
        (Ok(backup), Err(e)) => {
            eprintln!("Warning: {e}; root filesystem stays writable until reboot");
            backup
        }
        (Err(cause), Ok(())) => return Err(cause),
        (Err(cause), Err(e)) => return Err(error(format!("{cause}. Also: {e}"))),
    };
    println!(
        "Installed spotiya {version}; {}. Backup: {}",
        if running {
            "receiver ready"
        } else {
            "receiver remains stopped"
        },
        backup.display()
    );
    Ok(())
}

fn confirm_uninstall(input: &mut impl io::BufRead, output: &mut impl Write) -> io::Result<bool> {
    writeln!(
        output,
        "This removes Spotiya, its add-ons, saved login, settings and backups."
    )?;
    write!(output, "Type uninstall to confirm: ")?;
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(answer.trim() == "uninstall")
}

fn remove_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn managed_process(base: &Path, pid: u32) -> bool {
    if pid == std::process::id() {
        return false;
    }
    fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|exe| exe.starts_with(base))
}

/// Also finds orphaned add-ons after a forced receiver exit. Never signal by process name.
fn managed_processes(base: &Path) -> io::Result<Vec<u32>> {
    let mut pids = vec![];
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        if let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok())
            && managed_process(base, pid)
        {
            pids.push(pid);
        }
    }
    Ok(pids)
}

fn stop_remaining_processes(base: &Path) -> io::Result<()> {
    let base = fs::canonicalize(base)?;
    for (signal, wait) in [(libc::SIGTERM, 3), (libc::SIGKILL, 2)] {
        let pids = managed_processes(&base)?;
        for pid in pids {
            if managed_process(&base, pid) && unsafe { libc::kill(pid as i32, signal) } != 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::ESRCH) {
                    return Err(e);
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(wait);
        loop {
            if managed_processes(&base)?.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    Err(error(
        "Spotiya processes did not stop; no files were removed",
    ))
}

fn uninstall_files(
    layout: &Layout,
    runtime: &[PathBuf],
    stop: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    if !fs::symlink_metadata(&layout.base)?.is_dir() {
        return Err(error("Installation directory is not a regular directory"));
    }
    // Do not remove a replacement CLI or service belonging to another application.
    if fs::symlink_metadata(&layout.cli).is_ok()
        && fs::read_link(&layout.cli).ok().as_ref() != Some(&layout.base.join("cli.sh"))
    {
        return Err(error(format!(
            "{} belongs to another installation",
            layout.cli.display()
        )));
    }
    if let Ok(meta) = fs::symlink_metadata(&layout.init) {
        let expected = fs::read(layout.base.join("S99spotify-connect"))
            .unwrap_or_else(|_| include_bytes!("../packaging/S99spotify-connect").to_vec());
        if !meta.is_file() || fs::read(&layout.init)? != expected {
            return Err(error(
                "Init script belongs to another installation or was modified",
            ));
        }
    }
    let state = layout.base.join("receiver-state");
    let _install = crate::file_lock(&state, "install.lock")
        .map_err(|_| error("Another installation is running"))?;
    let _selection = crate::file_lock(&state, "profile-selection.lock")?;
    stop().map_err(|e| error(format!("Uninstall aborted: {e}")))?;
    // A manually started receiver must not survive removal or race the service stop.
    let _supervisor = crate::file_lock(&state, "supervisor.lock")?;
    let _receiver = crate::file_lock(&state, "receiver.lock")?;
    for path in [&layout.init, &layout.cli].into_iter().chain(runtime) {
        remove_path(path)
            .map_err(|e| error(format!("Could not remove {}: {e}", path.display())))?;
    }
    crate::soloist_install::uninstall_owned(&layout.base)?;
    remove_path(&layout.base)
        .map_err(|e| error(format!("Could not remove {}: {e}", layout.base.display())))
}

pub fn uninstall(assume_yes: bool) -> io::Result<()> {
    use io::IsTerminal;
    if !cfg!(target_arch = "arm") {
        return Err(error("Uninstall Spotiya on the ARM station"));
    }
    if unsafe { libc::geteuid() } != 0 {
        return Err(error("Run the uninstaller as root on the speaker"));
    }
    let layout = Layout::system();
    if !layout.base.exists() {
        println!("Spotiya is not installed.");
        return Ok(());
    }
    if !assume_yes {
        if !io::stdin().is_terminal() {
            return Err(error(
                "Uninstall needs an interactive terminal; use --uninstall --yes to confirm",
            ));
        }
        if !confirm_uninstall(&mut io::stdin().lock(), &mut io::stdout().lock())? {
            println!("Cancelled; nothing changed.");
            return Ok(());
        }
    }
    let mut runtime: Vec<PathBuf> = [
        RECEIVER_PID,
        SUPERVISOR_PID,
        crate::control::STATUS,
        "/run/spotiya.status.new",
        crate::voice::CONTROL,
        "/run/spotiya-airplay",
        "/run/spotiya-youtube.control",
        "/run/spotiya-youtube.levels",
        "/run/spotiya-soloist-levels",
        "/run/spotify-connect-stop",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    for log in [
        crate::supervisor::LOG,
        "/tmp/spotify-connect-supervisor.log",
        "/tmp/spotiya-airplay.log",
        "/tmp/spotiya-youtube.log",
        "/tmp/spotiya-soloist.log",
    ] {
        runtime.push(log.into());
        runtime.push(format!("{log}.1").into());
        runtime.push(format!("{log}.1.new").into());
    }
    let mut mount = RootMount::writable()?;
    let result = uninstall_files(&layout, &runtime, || {
        if layout.init.exists() {
            service("stop")?;
        } else if layout.base.join("stop.sh").exists() {
            let output = command(
                Path::new("/bin/sh"),
                &[layout.base.join("stop.sh").as_os_str()],
                Duration::from_secs(12),
            )?;
            if !output.status.success() {
                return Err(error("Receiver stop failed"));
            }
        }
        stop_remaining_processes(&layout.base)
    });
    match (result, mount.restore()) {
        (Ok(()), Ok(())) => {}
        (Ok(()), Err(e)) => {
            eprintln!("Warning: {e}; root filesystem stays writable until reboot");
        }
        (Err(e), Ok(())) => return Err(e),
        (Err(e), Err(remount)) => return Err(error(format!("{e}. Also: {remount}"))),
    }
    println!("Spotiya uninstalled; add-ons, saved login, settings and backups removed.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        calls: Vec<&'static str>,
        fail: bool,
        /// Index of the stop call that fails.
        failing_stop: Option<usize>,
        failing_start: bool,
        /// Backups directory whose binaries vanish when ready() runs, breaking the restore.
        sabotage: Option<PathBuf>,
    }
    impl Service for Fake {
        fn stop(&mut self) -> io::Result<()> {
            let index = self.calls.iter().filter(|c| **c == "stop").count();
            self.calls.push("stop");
            if self.failing_stop == Some(index) {
                return Err(error("fixture stop timed out"));
            }
            Ok(())
        }
        fn start(&mut self) -> io::Result<()> {
            self.calls.push("start");
            if self.failing_start {
                return Err(error("fixture start failed"));
            }
            Ok(())
        }
        fn ready(&mut self, _: &str) -> io::Result<()> {
            self.calls.push("ready");
            if let Some(backups) = self.sabotage.take() {
                for entry in fs::read_dir(backups).unwrap() {
                    let _ = fs::remove_file(entry.unwrap().path().join("spotiya"));
                }
            }
            if self.fail {
                self.fail = false;
                Err(error("fixture startup failed"))
            } else {
                Ok(())
            }
        }
    }
    fn fixture() -> (Temporary, Layout, PathBuf) {
        let temp = Temporary::new(&std::env::temp_dir(), "spotiya-install-test").unwrap();
        let layout = Layout {
            base: temp.0.join("installed"),
            init: temp.0.join("init"),
            cli: temp.0.join("cli"),
        };
        fs::create_dir(&layout.base).unwrap();
        let root = temp.0.join("package");
        for &f in FILES {
            let path = source(&root, &root.join("spotiya"), f);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"new").unwrap();
            let old = layout.destination(f);
            fs::create_dir_all(old.parent().unwrap()).unwrap();
            fs::write(old, b"old").unwrap();
        }
        symlink(layout.base.join("cli.sh"), &layout.cli).unwrap();
        fs::create_dir(layout.base.join("receiver-state")).unwrap();
        fs::write(
            layout.base.join("receiver-state/credentials.json"),
            b"private fixture",
        )
        .unwrap();
        (temp, layout, root)
    }
    fn uninstall_fixture() -> (Temporary, Layout, PathBuf) {
        let fixture = fixture();
        fs::write(
            &fixture.1.init,
            include_bytes!("../packaging/S99spotify-connect"),
        )
        .unwrap();
        fixture
    }
    #[test]
    fn uninstall_requires_the_explicit_confirmation_word() {
        for answer in ["", "\n", "yes\n", "UNINSTALL\n", "uninstall\n"] {
            let mut input = io::Cursor::new(answer);
            let mut output = vec![];
            assert_eq!(
                confirm_uninstall(&mut input, &mut output).unwrap(),
                answer == "uninstall\n"
            );
            assert!(String::from_utf8(output).unwrap().contains("saved login"));
        }
    }
    #[test]
    fn uninstall_removes_the_owned_installation_and_runtime_but_not_other_files() {
        let (temp, layout, _root) = uninstall_fixture();
        fs::create_dir_all(layout.base.join("airplay/bin")).unwrap();
        fs::write(layout.base.join("airplay/bin/addon"), b"helper").unwrap();
        fs::create_dir_all(layout.base.join("backups/old")).unwrap();
        let outside = temp.0.join("quasar.cfg");
        fs::write(&outside, b"factory settings").unwrap();
        symlink(&outside, layout.base.join("outside-link")).unwrap();
        let runtime = temp.0.join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::write(runtime.join("log"), b"log").unwrap();
        let mut stopped = false;
        uninstall_files(&layout, std::slice::from_ref(&runtime), || {
            stopped = true;
            assert!(layout.base.join("spotiya").exists());
            assert!(layout.init.exists());
            Ok(())
        })
        .unwrap();
        assert!(stopped);
        assert!(!layout.base.exists() && !layout.init.exists());
        assert!(fs::symlink_metadata(layout.cli).is_err());
        assert!(!runtime.exists());
        assert_eq!(fs::read(outside).unwrap(), b"factory settings");
    }
    #[test]
    fn failed_uninstall_stop_keeps_files_credentials_and_service() {
        let (_temp, layout, _root) = uninstall_fixture();
        let message = uninstall_files(&layout, &[], || Err(error("fixture stop failed")))
            .unwrap_err()
            .to_string();
        assert!(message.contains("Uninstall aborted: fixture stop failed"));
        assert_eq!(fs::read(layout.base.join("spotiya")).unwrap(), b"old");
        assert!(layout.base.join("receiver-state/credentials.json").exists());
        assert!(layout.init.exists() && layout.cli.exists());
    }
    #[test]
    fn uninstall_refuses_a_foreign_cli_service_and_an_existing_install_lock() {
        let (_temp, layout, _root) = uninstall_fixture();
        fs::remove_file(&layout.cli).unwrap();
        fs::write(&layout.cli, b"other application").unwrap();
        assert!(
            uninstall_files(&layout, &[], || panic!("must not stop"))
                .unwrap_err()
                .to_string()
                .contains("belongs to another installation")
        );
        fs::remove_file(&layout.cli).unwrap();
        symlink(layout.base.join("cli.sh"), &layout.cli).unwrap();
        fs::write(&layout.init, b"other service").unwrap();
        assert!(
            uninstall_files(&layout, &[], || panic!("must not stop"))
                .unwrap_err()
                .to_string()
                .contains("Init script belongs to another installation")
        );
        fs::write(
            &layout.init,
            include_bytes!("../packaging/S99spotify-connect"),
        )
        .unwrap();
        let _lock = crate::file_lock(&layout.base.join("receiver-state"), "install.lock").unwrap();
        assert!(
            uninstall_files(&layout, &[], || panic!("must not stop"))
                .unwrap_err()
                .to_string()
                .contains("Another installation is running")
        );
        assert!(layout.base.join("receiver-state/credentials.json").exists());
    }
    #[test]
    fn uninstall_stops_orphaned_helpers_only_from_its_own_directory() {
        use std::os::unix::process::ExitStatusExt;

        let temp = Temporary::new(&std::env::temp_dir(), "spotiya-uninstall-process-test").unwrap();
        let base = temp.0.join("installed");
        fs::create_dir(&base).unwrap();
        let helper = base.join("helper");
        assert!(
            command(
                Path::new("/bin/cp"),
                &[OsStr::new("/bin/sleep"), helper.as_os_str()],
                Duration::from_secs(5)
            )
            .unwrap()
            .status
            .success()
        );
        let mut child = Command::new(helper).arg("30").spawn().unwrap();
        let mut other = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let result = stop_remaining_processes(&base);
        // /proc/PID/exe can disappear before the kernel makes the exit status waitable.
        let deadline = Instant::now() + Duration::from_secs(2);
        let child_status = loop {
            let status = child.try_wait().unwrap();
            if status.is_some() || Instant::now() >= deadline {
                break status;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let other_running = other.try_wait().unwrap().is_none();
        let _ = child.kill();
        let _ = child.wait();
        let _ = other.kill();
        let _ = other.wait();
        result.unwrap();
        assert!(
            child_status.is_some_and(|s| s.signal() == Some(libc::SIGTERM)),
            "managed helper did not exit from SIGTERM: {child_status:?}"
        );
        assert!(other_running, "unrelated process was stopped");
    }
    #[test]
    fn failed_start_restores_binary_scripts_and_profiles_and_recovers() {
        let (_temp, layout, root) = fixture();
        let mut svc = Fake {
            calls: vec![],
            fail: true,
            failing_stop: None,
            failing_start: false,
            sabotage: None,
        };
        assert!(
            transaction(
                &layout,
                &root,
                &root.join("spotiya"),
                true,
                "mini2",
                &mut svc
            )
            .unwrap_err()
            .to_string()
            .contains("Rollback succeeded")
        );
        for &f in FILES {
            assert_eq!(fs::read(layout.destination(f)).unwrap(), b"old");
        }
        assert_eq!(
            svc.calls,
            ["stop", "start", "ready", "stop", "start", "ready"]
        );
        assert_eq!(
            fs::read(layout.base.join("receiver-state/credentials.json")).unwrap(),
            b"private fixture"
        );
    }
    #[test]
    fn receiver_that_does_not_stop_is_not_reported_as_rolled_back() {
        let (_temp, layout, root) = fixture();
        let mut svc = Fake {
            calls: vec![],
            fail: true,
            failing_stop: Some(1),
            failing_start: false,
            sabotage: None,
        };
        let message = transaction(
            &layout,
            &root,
            &root.join("spotiya"),
            true,
            "mini2",
            &mut svc,
        )
        .unwrap_err()
        .to_string();
        assert!(
            message.contains("Rollback needs attention (receiver did not stop"),
            "{message}"
        );
        assert_eq!(svc.calls, ["stop", "start", "ready", "stop"]);
        for &f in FILES {
            assert_eq!(fs::read(layout.destination(f)).unwrap(), b"old");
        }
        let mut svc = Fake {
            calls: vec![],
            fail: true,
            failing_stop: Some(1),
            failing_start: false,
            sabotage: Some(layout.base.join("backups")),
        };
        let message = transaction(
            &layout,
            &root,
            &root.join("spotiya"),
            true,
            "mini2",
            &mut svc,
        )
        .unwrap_err()
        .to_string();
        assert!(message.contains("Could not restore spotiya"), "{message}");
        assert!(message.contains("receiver did not stop"), "{message}");
    }
    #[test]
    fn failed_first_stop_changes_nothing_and_restarts_the_service() {
        let (_temp, layout, root) = fixture();
        let mut svc = Fake {
            calls: vec![],
            fail: false,
            failing_stop: Some(0),
            failing_start: false,
            sabotage: None,
        };
        let install = |svc: &mut Fake| {
            transaction(&layout, &root, &root.join("spotiya"), true, "mini2", svc)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            install(&mut svc),
            "Installation aborted: fixture stop timed out"
        );
        assert_eq!(svc.calls, ["stop", "start"]);
        let mut svc = Fake {
            calls: vec![],
            fail: false,
            failing_stop: Some(0),
            failing_start: true,
            sabotage: None,
        };
        assert!(install(&mut svc).ends_with("receiver restart also failed: fixture start failed"));
        for &f in FILES {
            assert_eq!(fs::read(layout.destination(f)).unwrap(), b"old");
        }
    }
    #[test]
    fn newest_backup_survives_a_clock_that_went_backwards() {
        let (_temp, layout, root) = fixture();
        let mut svc = Fake {
            calls: vec![],
            fail: false,
            failing_stop: None,
            failing_start: false,
            sabotage: None,
        };
        // Made while the clock was far ahead of the current time.
        let future = layout
            .base
            .join("backups/managed-90000000000000000000-future");
        fs::create_dir_all(&future).unwrap();
        fs::write(
            future.join("manifest.json"),
            br#"{"version":1,"files":[],"cli_existed":true}"#,
        )
        .unwrap();
        let install = |svc: &mut Fake| {
            transaction(&layout, &root, &root.join("spotiya"), false, "mini2", svc).unwrap()
        };
        let first = install(&mut svc);
        let second = install(&mut svc);
        assert!(first.exists() && second.exists());
        assert!(!future.exists());
        assert!(second > first);
    }
    #[test]
    fn restore_continues_past_a_file_it_cannot_write() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (_temp, layout, _root) = fixture();
        let backup = snapshot(&layout).unwrap();
        for &f in FILES {
            fs::write(layout.destination(f), b"new").unwrap();
        }
        let profiles = layout.base.join("profiles");
        fs::set_permissions(&profiles, fs::Permissions::from_mode(0o555)).unwrap();
        let failure = restore(&layout, &backup).unwrap_err().to_string();
        fs::set_permissions(&profiles, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(failure.contains("profiles/mini1.json"), "{failure}");
        for &f in FILES {
            let expected: &[u8] = if f.starts_with("profiles/") {
                b"new"
            } else {
                b"old"
            };
            assert_eq!(fs::read(layout.destination(f)).unwrap(), expected, "{f}");
        }
    }
    #[test]
    fn first_install_creates_profile_directory_and_does_not_stop_a_missing_service() {
        let (_temp, layout, root) = fixture();
        for &file in FILES {
            fs::remove_file(layout.destination(file)).unwrap();
        }
        fs::remove_file(&layout.cli).unwrap();
        fs::remove_dir(layout.base.join("profiles")).unwrap();
        let mut service = Fake {
            calls: vec![],
            fail: false,
            failing_stop: None,
            failing_start: false,
            sabotage: None,
        };
        transaction(
            &layout,
            &root,
            &root.join("spotiya"),
            true,
            "mini1",
            &mut service,
        )
        .unwrap();
        assert_eq!(service.calls, ["start", "ready"]);
        assert_eq!(
            fs::read(layout.base.join("profiles/mini1.json")).unwrap(),
            b"new"
        );
        assert!(layout.init.is_file());
        assert_eq!(
            fs::read_link(&layout.cli).unwrap(),
            layout.base.join("cli.sh")
        );
    }
    #[test]
    fn stopped_receiver_stays_stopped_and_only_managed_backups_are_pruned() {
        let (_temp, layout, root) = fixture();
        let mut svc = Fake {
            calls: vec![],
            fail: false,
            failing_stop: None,
            failing_start: false,
            sabotage: None,
        };
        fs::create_dir_all(layout.base.join("backups/manual")).unwrap();
        for _ in 0..4 {
            transaction(
                &layout,
                &root,
                &root.join("spotiya"),
                false,
                "mini2",
                &mut svc,
            )
            .unwrap();
        }
        assert!(svc.calls.is_empty());
        assert_eq!(fs::read(layout.base.join("spotiya")).unwrap(), b"new");
        assert_eq!(
            fs::read_dir(layout.base.join("backups")).unwrap().count(),
            KEEP + 1
        );
        assert!(layout.base.join("backups/manual").exists());
    }
}
