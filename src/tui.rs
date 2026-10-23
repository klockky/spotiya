use crate::control::{self, Status};
use std::{
    fs,
    io::{self, IsTerminal, Read, Write},
    os::fd::AsRawFd,
    sync::mpsc,
    time::{Duration, Instant},
};

struct Terminal {
    saved: libc::termios,
}
impl Terminal {
    fn enter() -> io::Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::other(
                "TUI needs a terminal; use adb shell -t or spotiya tui",
            ));
        }
        let mut saved = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut saved) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = saved;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 1;
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        print!("\x1b[?1049h\x1b[?25l");
        io::stdout().flush()?;
        Ok(Self { saved })
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.saved);
        }
        print!("\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = io::stdout().flush();
    }
}
fn size() -> (usize, usize) {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe {
        libc::ioctl(io::stdout().as_raw_fd(), libc::TIOCGWINSZ, &mut size);
    }
    (
        if size.ws_col == 0 {
            80
        } else {
            usize::from(size.ws_col).max(20)
        },
        if size.ws_row == 0 {
            24
        } else {
            usize::from(size.ws_row).max(5)
        },
    )
}
fn safe(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}
fn clock(ms: u32) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}
fn or_spotify(source: &str) -> String {
    safe(if source.is_empty() { "spotify" } else { source })
}
fn lines(status: &Status, notice: &str, prompt: Option<&[u8]>, width: usize) -> Vec<String> {
    let state = if !status.connected && !status.navidrome {
        "CONNECTING"
    } else if !status.active {
        "READY"
    } else if status.paused {
        "PAUSED"
    } else {
        "PLAYING"
    };
    let voice = if status.voice { "on" } else { "off" };
    let guard = if status.native_guard {
        "Music priority"
    } else {
        "idle"
    };
    let cells = width.saturating_sub(18).min(42);
    let filled = if status.duration_ms == 0 {
        0
    } else {
        ((u64::from(status.position_ms) * cells as u64) / u64::from(status.duration_ms))
            .min(cells as u64) as usize
    };
    let mut out = vec![
        format!(
            "SPOTIYA                         v{}",
            env!("CARGO_PKG_VERSION")
        ),
        String::new(),
        format!("{}   |   {}", safe(&status.name), state),
        format!(
            "Volume {:3}%   Voice {}   Audio: {}",
            status.volume, voice, guard
        ),
        format!(
            "Spectrum: {}",
            if !status.spectrum_supported {
                "unavailable"
            } else if status.spectrum {
                "on"
            } else {
                "off"
            }
        ),
        format!(
            "Source: {}   Search: {}   Navidrome: {}",
            or_spotify(&status.source),
            or_spotify(&status.default_source),
            if status.navidrome {
                "set up"
            } else {
                "not set up"
            }
        ),
        String::new(),
        safe(if status.title.is_empty() {
            "No track selected"
        } else {
            &status.title
        }),
        safe(&status.artist),
        safe(&status.album),
        format!(
            "{} [{}{}] {}",
            clock(status.position_ms),
            "=".repeat(filled),
            "-".repeat(cells - filled),
            clock(status.duration_ms)
        ),
        String::new(),
        "Space pause/play   n next   p previous   +/- volume".into(),
        "/ search   s search source   l Spotify login   v voice".into(),
        "e spectrum on/off   x release to Alice   q quit".into(),
        String::new(),
    ];
    out.extend(notice.lines().map(safe));
    if let Some(input) = prompt {
        out.push(format!(
            "Search {}: {}_",
            or_spotify(&status.default_source),
            safe(&String::from_utf8_lossy(input))
        ));
    }
    out
}
fn draw(status: &Status, notice: &str, prompt: Option<&[u8]>) -> io::Result<()> {
    let (width, height) = size();
    let mut frame = String::from("\x1b[H");
    for (i, line) in lines(status, notice, prompt, width)
        .into_iter()
        .take(height.saturating_sub(1))
        .enumerate()
    {
        if i == 0 {
            frame.push_str("\x1b[1;32m");
        }
        // Most metadata is Latin/Cyrillic; conservative clipping leaves room for
        // double-width characters and keeps control sequences out of the terminal.
        let mut used = 0;
        for c in line.chars() {
            let cells = if c as u32 >= 0x1100 { 2 } else { 1 };
            if used + cells > width.saturating_sub(1) {
                break;
            }
            frame.push(c);
            used += cells;
        }
        frame.push_str("\x1b[0m\x1b[K\r\n");
    }
    frame.push_str("\x1b[J");
    print!("{frame}");
    io::stdout().flush()
}
pub fn run() -> io::Result<()> {
    use std::io::BufRead;
    let _terminal = Terminal::enter()?;
    let mut status = Status::default();
    let mut notice =
        "Ready. / searches the chosen source; s switches Spotify/Navidrome.".to_owned();
    let mut prompt: Option<Vec<u8>> = None;
    let (updates, rx) = mpsc::channel::<String>();
    let mut login_socket: Option<std::os::unix::net::UnixStream> = None;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    loop {
        if let Ok(text) = fs::read_to_string(control::STATUS)
            && let Ok(fresh) = serde_json::from_str(&text)
        {
            status = fresh;
        }
        while let Ok(message) = rx.try_recv() {
            if message.starts_with("DONE") || message.starts_with("ERROR") {
                login_socket = None;
            }
            notice = message;
        }
        if last_draw.elapsed() >= Duration::from_millis(250) {
            draw(&status, &notice, prompt.as_deref())?;
            last_draw = Instant::now();
        }
        let mut input = [0u8; 64];
        let n = io::stdin().read(&mut input)?;
        for &key in &input[..n] {
            if key == 3 || key == 4 {
                if let Some(s) = login_socket.take() {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                }
                return Ok(());
            }
            if let Some(text) = prompt.as_mut() {
                match key {
                    27 => prompt = None,
                    127 | 8 => {
                        while let Some(byte) = text.pop() {
                            if byte & 0xc0 != 0x80 {
                                break;
                            }
                        }
                    }
                    b'\r' | b'\n' => {
                        let query = String::from_utf8_lossy(text).into_owned();
                        prompt = None;
                        notice = if query.trim().is_empty() {
                            "Search cancelled".into()
                        } else {
                            match control::request(&format!("PLAY {}", query.trim())) {
                                Ok(_) => {
                                    format!("{} search queued", or_spotify(&status.default_source))
                                }
                                Err(e) => e.to_string(),
                            }
                        };
                    }
                    32..=255 if text.len() < 400 => text.push(key),
                    _ => {}
                }
                last_draw = Instant::now() - Duration::from_secs(1);
                continue;
            }
            let command = match key {
                b'q' => {
                    if let Some(s) = login_socket.take() {
                        let _ = s.shutdown(std::net::Shutdown::Both);
                    }
                    return Ok(());
                }
                b' ' => Some("TOGGLE".to_owned()),
                b'n' => Some("NEXT".to_owned()),
                b'p' => Some("PREVIOUS".to_owned()),
                b'+' | b'=' => Some(format!("VOLUME {}", (status.volume + 5).min(100))),
                b'-' => Some(format!("VOLUME {}", status.volume.saturating_sub(5))),
                b'v' => Some(format!("VOICE {}", if status.voice { "OFF" } else { "ON" })),
                b'e' => {
                    if status.spectrum_supported {
                        Some(format!(
                            "SPECTRUM {}",
                            if status.spectrum { "OFF" } else { "ON" }
                        ))
                    } else {
                        notice = "Spectrum display is unavailable on this device".into();
                        None
                    }
                }
                b'x' => Some("RELEASE".to_owned()),
                b's' => {
                    if or_spotify(&status.default_source) == "spotify" {
                        if status.navidrome {
                            Some("SOURCE NAVIDROME".to_owned())
                        } else {
                            notice =
                                "Navidrome is not set up: spotiya navidrome-setup URL USER".into();
                            None
                        }
                    } else {
                        Some("SOURCE SPOTIFY".to_owned())
                    }
                }
                b'/' => {
                    prompt = Some(Vec::new());
                    None
                }
                b'l' => {
                    if login_socket.is_some() {
                        notice = "Login pending; open the link above. q cancels.".into();
                    } else {
                        match control::login() {
                            Ok(reader) => {
                                login_socket = Some(reader.get_ref().try_clone()?);
                                let tx = updates.clone();
                                std::thread::Builder::new()
                                    .name("pairing-ui".into())
                                    .stack_size(64 * 1024)
                                    .spawn(move || {
                                        for line in reader.lines() {
                                            match line {
                                                Ok(line) => {
                                                    let _ = tx.send(control::login_line(&line));
                                                }
                                                Err(_) => {
                                                    let _ = tx.send(
                                                        "ERROR Pairing connection closed".into(),
                                                    );
                                                    return;
                                                }
                                            }
                                        }
                                    })?;
                                notice = "Requesting a Spotify pairing code...".into();
                            }
                            Err(e) => notice = e.to_string(),
                        }
                    }
                    None
                }
                _ => None,
            };
            if let Some(command) = command {
                notice = match control::request(&command) {
                    Ok(_) => "Command queued".into(),
                    Err(e) => e.to_string(),
                };
            }
            last_draw = Instant::now() - Duration::from_secs(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn spectrum_state_is_visible_for_supported_and_other_devices() {
        let mut status = Status::default();
        assert!(
            lines(&status, "", None, 80)
                .iter()
                .any(|s| s == "Spectrum: unavailable")
        );
        status.spectrum_supported = true;
        for (enabled, label) in [(false, "off"), (true, "on")] {
            status.spectrum = enabled;
            let frame = lines(&status, "", None, 80);
            assert!(frame.iter().any(|s| s == &format!("Spectrum: {label}")));
            assert!(frame.iter().any(|s| s.contains("e spectrum on/off")));
        }
    }
    #[test]
    fn sources_are_shown_with_spotify_as_the_default_for_older_status_files() {
        let mut status = Status::default();
        let frame = lines(&status, "", Some(b"x"), 80);
        assert!(
            frame
                .iter()
                .any(|s| s == "Source: spotify   Search: spotify   Navidrome: not set up")
        );
        assert!(frame.iter().any(|s| s == "Search spotify: x_"));
        assert!(lines(&status, "", None, 80)[2].ends_with("|   CONNECTING"));
        status.navidrome = true;
        status.source = "navidrome\x1b[2J".into();
        status.default_source = "navidrome".into();
        let frame = lines(&status, "", None, 80);
        assert!(
            frame
                .iter()
                .any(|s| s == "Source: navidrome[2J   Search: navidrome   Navidrome: set up")
        );
        assert!(frame.iter().any(|s| s.contains("|   READY")));
    }
    #[test]
    fn metadata_cannot_inject_terminal_controls_and_empty_progress_is_safe() {
        assert_eq!(safe("Song\x1b[2J\r\n"), "Song[2J");
        let mut s = Status {
            title: "\x1b[31mTitle".into(),
            ..Default::default()
        };
        assert!(lines(&s, "", None, 20).iter().all(|s| !s.contains('\x1b')));
        s.duration_ms = 100;
        s.position_ms = 999;
        let _ = lines(&s, "", None, 20);
    }
}
