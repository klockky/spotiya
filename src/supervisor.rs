use crate::{Options, file_lock};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    future::Future,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    process::{Child, Command},
    time::{Instant, MissedTickBehavior},
};

struct Config {
    program: PathBuf,
    args: Vec<OsString>,
    state: PathBuf,
    parent_pid: PathBuf,
    child_pid: PathBuf,
    stop: PathBuf,
    log: PathBuf,
    status: PathBuf,
    first_delay: Duration,
    max_delay: Duration,
    stable_after: Duration,
    trim_every: Duration,
}
fn remove_pid(path: &Path, pid: u32) {
    if fs::read_to_string(path).is_ok_and(|s| s.trim() == pid.to_string()) {
        let _ = fs::remove_file(path);
    }
}
pub const LOG: &str = "/tmp/spotify-connect.log";
const LOG_LIMIT: u64 = 256 * 1024;
pub fn open_log(path: &Path) -> io::Result<File> {
    match trim_log(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    OpenOptions::new().create(true).append(true).open(path)
}
/// Running children keep the log open for appending, so it is copied aside and
/// truncated in place; their next write lands at the new end of the file.
pub fn trim_log(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    if file.metadata()?.len() < LOG_LIMIT {
        return Ok(());
    }
    let temporary = path.with_extension("log.1.new");
    io::copy(&mut file, &mut File::create(&temporary)?)?;
    fs::rename(temporary, path.with_extension("log.1"))?;
    file.set_len(0)
}
fn offline_status(config: &Config, message: &str) {
    let mut status = fs::read_to_string(&config.status)
        .ok()
        .and_then(|s| serde_json::from_str::<crate::control::Status>(&s).ok())
        .unwrap_or_default();
    status.active = false;
    status.airplay_active = false;
    status.airplay_running = false;
    status.youtube_active = false;
    status.youtube_running = false;
    status.paused = false;
    status.loading = false;
    status.connected = false;
    status.native_guard = false;
    status.receiver_pid = 0;
    status.ducking = false;
    status.retry_required = false;
    status.login_refused.clear();
    status.backend_error.clear();
    status.retry_after_secs = 0;
    status.failed_tracks = 0;
    status.error = message.to_owned();
    let temporary = config.status.with_extension("status.new");
    if let Ok(text) = serde_json::to_string(&status) {
        let _ =
            fs::write(&temporary, text + "\n").and_then(|_| fs::rename(&temporary, &config.status));
    }
}
async fn stop_child(child: &mut Child) {
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    if tokio::time::timeout(Duration::from_secs(4), child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}
async fn monitor(config: &Config, shutdown: impl Future<Output = ()>) -> io::Result<()> {
    let _lock = file_lock(&config.state, "supervisor.lock")?;
    let parent = std::process::id();
    fs::write(&config.parent_pid, parent.to_string())?;
    tokio::pin!(shutdown);
    let mut poll = tokio::time::interval(Duration::from_millis(100));
    poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut trim = tokio::time::interval(config.trim_every);
    trim.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut child: Option<Child> = None;
    let mut child_pid = None;
    let mut started = Instant::now();
    let mut next_start = Instant::now();
    let mut delay = config.first_delay;
    let result = loop {
        tokio::select! {
            _=&mut shutdown => break Ok(()),
            _=poll.tick()=>{if config.stop.exists() {break Ok(());}},
            _=trim.tick(),if child.is_some()=>{
                if let Err(e)=trim_log(&config.log) {eprintln!("supervisor: log trim failed: {e}");}
            },
            _=tokio::time::sleep_until(next_start),if child.is_none()=>{
                if config.stop.exists() {break Ok(());}
                offline_status(config,"Receiver starting");
                let output=match open_log(&config.log) {Ok(file)=>file,Err(e)=>break Err(e)};
                let error=match output.try_clone() {Ok(file)=>file,Err(e)=>break Err(e)};
                let mut command=Command::new(&config.program);
                command.args(&config.args).stdin(Stdio::null()).stdout(output).stderr(error).kill_on_drop(true);
                // kill_on_drop does not run when the supervisor is killed, so let the kernel stop
                // the receiver; SIGTERM lets it shut down its own AirPlay children cleanly.
                unsafe {
                    command.pre_exec(move || {
                        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                            return Err(io::Error::last_os_error());
                        }
                        if libc::getppid() as u32 != parent {
                            return Err(io::Error::other("Supervisor exited"));
                        }
                        Ok(())
                    });
                }
                match command.spawn() {
                    Ok(process)=>{
                        let Some(pid)=process.id() else {break Err(io::Error::other("Child has no PID"));};
                        child=Some(process);child_pid=Some(pid);started=Instant::now();
                        if let Err(e)=fs::write(&config.child_pid,pid.to_string()) {break Err(e);}
                        eprintln!("supervisor: receiver started (pid {pid})");
                    },
                    Err(e)=>{
                        eprintln!("supervisor: receiver spawn failed: {e}; retry in {}s",delay.as_secs());
                        next_start=Instant::now()+delay;delay=(delay*2).min(config.max_delay);
                    },
                }
            },
            status=async {child.as_mut().unwrap().wait().await},if child.is_some()=>{
                if let Some(pid)=child_pid.take() {remove_pid(&config.child_pid,pid);}
                child=None;
                offline_status(config,"Receiver exited; restarting automatically");
                if started.elapsed()>=config.stable_after {delay=config.first_delay;}
                eprintln!("supervisor: receiver exited ({status:?}); retry in {}s",delay.as_secs());
                next_start=Instant::now()+delay;delay=(delay*2).min(config.max_delay);
            },
        }
    };
    if let Some(process) = child.as_mut() {
        stop_child(process).await;
    }
    if let Some(pid) = child_pid {
        remove_pid(&config.child_pid, pid);
    }
    offline_status(config, "");
    remove_pid(&config.parent_pid, parent);
    result
}
pub async fn run(options: &Options) -> io::Result<()> {
    let mut args = vec![
        OsString::from("--state-dir"),
        options.state_dir.clone().into_os_string(),
        OsString::from("--name"),
        OsString::from(&options.name),
    ];
    if let Some(backend) = options.backend {
        args.push("--backend".into());
        args.push(backend.name().into());
    }
    if let Some(profile) = &options.profile {
        args.push("--profile".into());
        args.push(profile.into());
    }
    if !options.buttons {
        args.push("--no-buttons".into());
    }
    if !options.voice {
        args.push("--no-voice".into());
    }
    let config = Config {
        program: std::env::current_exe()?,
        args,
        state: options.state_dir.clone(),
        parent_pid: crate::installation::SUPERVISOR_PID.into(),
        child_pid: crate::installation::RECEIVER_PID.into(),
        stop: "/run/spotify-connect-stop".into(),
        log: LOG.into(),
        status: crate::control::STATUS.into(),
        first_delay: Duration::from_secs(2),
        max_delay: Duration::from_secs(60),
        stable_after: Duration::from_secs(60),
        trim_every: Duration::from_secs(30),
    };
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    monitor(&config, async {
        tokio::select! {_=terminate.recv()=>{},_=tokio::signal::ctrl_c()=>{}}
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;
    fn fixture(script: &str) -> (PathBuf, Config) {
        let root =
            std::env::temp_dir().join(format!("spotiya-supervisor-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let path = root.join("child.sh");
        fs::write(&path, script).unwrap();
        let config = Config {
            program: "/bin/sh".into(),
            args: vec![path.into_os_string()],
            state: root.clone(),
            parent_pid: root.join("parent.pid"),
            child_pid: root.join("child.pid"),
            stop: root.join("stop"),
            log: root.join("child.log"),
            status: root.join("status.json"),
            first_delay: Duration::from_millis(40),
            max_delay: Duration::from_millis(160),
            stable_after: Duration::from_secs(60),
            trim_every: Duration::from_secs(60),
        };
        (root, config)
    }
    async fn wait_for(path: &Path) -> String {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(s) = fs::read_to_string(path) {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
    #[test]
    fn trimming_keeps_the_open_append_handle_writing_from_the_start() {
        use std::io::Write;
        let (root, config) = fixture("");
        let mut writer = open_log(&config.log).unwrap();
        writer
            .write_all(&vec![b'x'; LOG_LIMIT as usize - 1])
            .unwrap();
        trim_log(&config.log).unwrap();
        assert!(!root.join("child.log.1").exists());
        writer.write_all(b"y\n").unwrap();
        trim_log(&config.log).unwrap();
        assert_eq!(fs::metadata(&config.log).unwrap().len(), 0);
        assert_eq!(
            fs::metadata(root.join("child.log.1")).unwrap().len(),
            LOG_LIMIT + 1
        );
        writer.write_all(b"after\n").unwrap();
        assert_eq!(fs::read(&config.log).unwrap(), b"after\n");
        // A new child must not move the file away from writers that already hold it.
        writer.write_all(&vec![b'z'; LOG_LIMIT as usize]).unwrap();
        let mut second = open_log(&config.log).unwrap();
        writer.write_all(b"first\n").unwrap();
        second.write_all(b"second\n").unwrap();
        assert_eq!(fs::read(&config.log).unwrap(), b"first\nsecond\n");
        assert_eq!(
            fs::metadata(root.join("child.log.1")).unwrap().len(),
            LOG_LIMIT + 6
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn crash_restarts_child_and_manual_stop_kills_and_reaps_it() {
        let (root, config) = fixture(
            "cd \"$(dirname \"$0\")\"\nif [ ! -f attempted ]; then touch attempted; exit 7; fi\necho $$ > ready\nexec sleep 60\n",
        );
        fs::write(&config.log, vec![b'x'; 256 * 1024]).unwrap();
        let began = Instant::now();
        let (stop, rx) = oneshot::channel::<()>();
        let ready = root.join("ready");
        let pidfile = config.child_pid.clone();
        let parentfile = config.parent_pid.clone();
        let stopfile = config.stop.clone();
        let task = tokio::spawn(async move {
            monitor(&config, async {
                let _ = rx.await;
            })
            .await
        });
        let pid = wait_for(&ready).await.trim().parse::<i32>().unwrap();
        wait_for(&pidfile).await;
        assert!(began.elapsed() >= Duration::from_millis(40));
        fs::write(stopfile, "").unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!pidfile.exists() && !parentfile.exists());
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert!(
            fs::read_to_string(root.join("child.log"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fs::metadata(root.join("child.log.1")).unwrap().len(),
            256 * 1024
        );
        let status: crate::control::Status =
            serde_json::from_str(&fs::read_to_string(root.join("status.json")).unwrap()).unwrap();
        assert!(!status.active && !status.connected && status.error.is_empty());
        drop(stop);
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn duplicate_supervisor_is_rejected_and_shutdown_interrupts_backoff() {
        let (root, mut config) = fixture("exit 7\n");
        config.first_delay = Duration::from_secs(30);
        config.max_delay = Duration::from_secs(60);
        let duplicate = Config {
            program: config.program.clone(),
            args: config.args.clone(),
            state: root.clone(),
            parent_pid: config.parent_pid.clone(),
            child_pid: config.child_pid.clone(),
            stop: config.stop.clone(),
            log: config.log.clone(),
            status: config.status.clone(),
            first_delay: config.first_delay,
            max_delay: config.max_delay,
            stable_after: config.stable_after,
            trim_every: config.trim_every,
        };
        let parentfile = config.parent_pid.clone();
        let pidfile = config.child_pid.clone();
        let (stop, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            monitor(&config, async {
                let _ = rx.await;
            })
            .await
        });
        wait_for(&parentfile).await;
        assert!(monitor(&duplicate, std::future::pending()).await.is_err());
        tokio::time::sleep(Duration::from_millis(150)).await;
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!parentfile.exists() && !pidfile.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
