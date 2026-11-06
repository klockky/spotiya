use crate::{
    control::{self, Status},
    screen::{self, Area, Canvas, Rgb, Screen, Style},
};
use std::{
    fs,
    io::{self, IsTerminal, Write},
    os::fd::AsRawFd,
    path::Path,
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
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut out = io::stdout();
        out.write_all(b"\x1b[?1049h\x1b[?25l")?;
        out.flush()?;
        if let Ok(mut active) = ACTIVE.lock() {
            *active = Some(saved);
        }
        Ok(Self { saved })
    }
}
/// Terminal settings to put back if the TUI panics, since release builds abort without `Drop`.
static ACTIVE: std::sync::Mutex<Option<libc::termios>> = std::sync::Mutex::new(None);
fn leave(saved: &libc::termios) {
    unsafe {
        libc::tcsetattr(0, libc::TCSANOW, saved);
    }
    // A closed terminal must not turn cleanup into a panic.
    let mut out = io::stdout();
    let _ = out.write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l");
    let _ = out.flush();
}
pub fn restore_terminal_on_panic() {
    if let Ok(mut active) = ACTIVE.try_lock()
        && let Some(saved) = active.take()
    {
        leave(&saved);
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE.lock() {
            *active = None;
        }
        leave(&self.saved);
    }
}
/// Waits up to 100 ms for keys; `None` means the terminal hung up. A raw read alone
/// cannot tell end of input from a timeout, and would spin once the terminal closes.
fn read_keys(input: &mut [u8]) -> io::Result<Option<usize>> {
    let mut pfd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    match unsafe { libc::poll(&mut pfd, 1, 100) } {
        0 => return Ok(Some(0)),
        n if n < 0 => {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted {
                Ok(Some(0))
            } else {
                Err(e)
            };
        }
        _ => {}
    }
    if pfd.revents & libc::POLLIN == 0 {
        return Ok(None);
    }
    // Read the descriptor directly: buffered stdin could hold keys that poll no longer sees.
    match unsafe { libc::read(0, input.as_mut_ptr().cast(), input.len()) } {
        0 => Ok(None),
        n if n < 0 => {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                Ok(Some(0))
            } else {
                Err(e)
            }
        }
        n => Ok(Some(n as usize)),
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
fn state(status: &Status) -> (&'static str, Rgb) {
    if status.airplay_active {
        ("AIRPLAY 2", screen::AIRPLAY)
    } else if status.youtube_active && status.paused {
        ("YOUTUBE PAUSED", screen::WARN)
    } else if status.youtube_active {
        ("YOUTUBE", screen::YOUTUBE)
    } else if status.retry_required {
        ("RETRY REQUIRED", screen::ERROR)
    } else if status.login_refused == "premium" {
        ("PREMIUM REQUIRED", screen::ERROR)
    } else if !status.login_refused.is_empty() {
        ("LOGIN REQUIRED", screen::ERROR)
    } else if status.spotify_backend == "soloist" && !status.backend_error.is_empty() {
        ("SOLOIST ERROR", screen::ERROR)
    } else if status.spotify_backend == "soloist" && !status.connected {
        ("PAIR IN SPOTIFY", screen::DIM)
    } else if !status.connected {
        ("CONNECTING", screen::INFO)
    } else if !status.error.is_empty() {
        ("UNAVAILABLE", screen::ERROR)
    } else if status.loading {
        ("LOADING", screen::INFO)
    } else if !status.active {
        ("READY", screen::DIM)
    } else if status.paused {
        ("PAUSED", screen::WARN)
    } else {
        ("PLAYING", screen::ACCENT)
    }
}
/// Two samples per column; enough for any terminal a person can open.
const HISTORY: usize = 8192;
/// Recent spectrum levels, newest last, for the scrolling graph.
#[derive(Default)]
struct History(std::collections::VecDeque<[u8; 4]>);
impl History {
    fn push(&mut self, levels: [u8; 4]) {
        if self.0.len() == HISTORY {
            self.0.pop_front();
        }
        self.0.push_back(levels);
    }
    fn series(&self, bands: std::ops::Range<usize>) -> Vec<u8> {
        self.0
            .iter()
            .map(|levels| levels[bands.clone()].iter().copied().max().unwrap_or(0))
            .collect()
    }
}
const GEOSYNC: &str = "/data/quasar/data/geosync.state";
/// Reads one protobuf varint from the front of `data`.
fn varint(data: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = data.split_first()?;
        *data = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}
/// Skips one protobuf field value of `wire` type; `None` for types Quasar does not use here.
fn skip(data: &mut &[u8], wire: u64) -> Option<()> {
    match wire {
        0 => {
            varint(data)?;
        }
        1 => *data = data.get(8..)?,
        2 => {
            let length = usize::try_from(varint(data)?).ok()?;
            *data = data.get(length..)?;
        }
        5 => *data = data.get(4..)?,
        _ => return None,
    }
    Some(())
}
/// The station clock runs in UTC. Quasar keeps the speaker's time zone in `geosync.state`
/// as field 2 { 1: name, 2: offset in seconds }; field 1, the location, is skipped unread.
fn geosync_offset(mut data: &[u8]) -> Option<i64> {
    while !data.is_empty() {
        let key = varint(&mut data)?;
        if key != (2 << 3 | 2) {
            skip(&mut data, key & 7)?;
            continue;
        }
        let length = usize::try_from(varint(&mut data)?).ok()?;
        let mut zone = data.get(..length)?;
        data = data.get(length..)?;
        while !zone.is_empty() {
            let key = varint(&mut zone)?;
            if key == 2 << 3 {
                // int32: a negative offset arrives as a sign-extended 64-bit varint.
                return Some(varint(&mut zone)? as i64);
            }
            skip(&mut zone, key & 7)?;
        }
    }
    None
}
/// Seconds east of UTC for the header clock: the host wrapper's value, else the station's.
fn utc_offset() -> Option<i64> {
    std::env::var("SPOTIYA_UTC_OFFSET")
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            fs::read(GEOSYNC)
                .ok()
                .and_then(|data| geosync_offset(&data))
        })
        .filter(|s: &i64| s.abs() <= 14 * 3600)
}
fn local_clock(offset: Option<i64>) -> String {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    let converted = match offset {
        Some(offset) => unsafe { libc::gmtime_r(&(now + offset as libc::time_t), &mut tm) },
        None => unsafe { libc::localtime_r(&now, &mut tm) },
    };
    if converted.is_null() {
        return String::new();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}
/// One feature row of the receiver panel: name, a coloured dot and its state.
fn feature(c: &mut Canvas, area: Area, row: usize, name: &str, value: &str, colour: Rgb) {
    let y = area.y + row;
    c.text(area.x, y, area.w, name, Style::new(screen::DIM));
    let x = area.x + 10;
    if x + 2 < area.x + area.w {
        let dot = if colour == screen::DIM { '○' } else { '●' };
        c.put(x, y, dot, Style::new(colour));
        c.text(
            x + 2,
            y,
            area.x + area.w - x - 2,
            value,
            Style::new(screen::TEXT),
        );
    }
}
fn dashboard(
    status: &Status,
    notice: &str,
    prompt: Option<(&str, &[u8])>,
    history: &History,
    clock: &str,
    width: usize,
    height: usize,
) -> Canvas {
    let mut c = Canvas::new(width, height);
    if width < 40 || height < 12 {
        c.text(
            0,
            0,
            width,
            "Enlarge the terminal for spotiya",
            Style::new(screen::WARN),
        );
        let (label, colour) = state(status);
        c.text(0, 1, width, label, Style::bold(colour));
        for (i, line) in status
            .error
            .lines()
            .chain(notice.lines())
            .flat_map(|line| screen::wrap(line, width.max(1)))
            .take(height.saturating_sub(2))
            .enumerate()
        {
            c.text(0, 2 + i, width, &line, Style::new(screen::TEXT));
        }
        return c;
    }
    let global: &[(&str, &str)] = &[
        // Most needed first: a narrow edge keeps as many as fit, in this order.
        ("/", "search"),
        ("y", "youtube"),
        ("q", "quit"),
        ("l", "login"),
        ("d", "profile"),
        ("x", "release"),
        ("r", "retry"),
    ];
    // Header: state, speaker and profile.
    let header = Area {
        x: 0,
        y: 0,
        w: width,
        h: 3,
    };
    c.panel(header, "spotiya", &[]);
    c.text_right(
        width - 2,
        0,
        &format!(" v{} · {clock} ", env!("CARGO_PKG_VERSION")),
        Style::new(screen::DIM),
    );
    let (label, colour) = state(status);
    let inner = header.inner();
    let mut x = inner.x;
    x += c.text(x, inner.y, inner.w, "● ", Style::new(colour));
    x += c.text(
        x,
        inner.y,
        inner.w.saturating_sub(x - inner.x),
        label,
        Style::bold(colour),
    );
    let place = [
        status.name.as_str(),
        status.profile.as_str(),
        status.spotify_backend.as_str(),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join(" · ");
    if !place.is_empty() {
        let room = inner.w.saturating_sub(x - inner.x);
        c.text(
            x,
            inner.y,
            room,
            &format!("  {place}"),
            Style::new(screen::TEXT),
        );
    }
    let output = if status.airplay_active {
        "AirPlay priority"
    } else if status.youtube_active {
        "YouTube priority"
    } else if status.native_guard {
        "Spotify priority"
    } else {
        ""
    };
    if !output.is_empty() && inner.w > 60 {
        c.text_right(inner.x + inner.w, inner.y, output, Style::new(screen::DIM));
    }
    // Messages under the track: search prompt, errors, then notices.
    let mut messages: Vec<(String, Style)> = vec![];
    if let Some((label, input)) = prompt {
        messages.push((
            format!("{label} › {}▏", String::from_utf8_lossy(input)),
            Style::bold(screen::ACCENT),
        ));
    }
    if !status.error.is_empty() {
        messages.push((status.error.clone(), Style::new(screen::ERROR)));
        if status.retry_required {
            messages.push((
                if status.retry_after_secs > 0 {
                    format!("Retry available in {}s", status.retry_after_secs)
                } else {
                    "Press r to retry".into()
                },
                Style::new(screen::WARN),
            ));
        }
    }
    messages.extend(
        notice
            .lines()
            .map(|l| (l.to_owned(), Style::new(screen::DIM))),
    );
    let side = width >= 76;
    let right_w = if side { 36 } else { width };
    let left_w = if side { width - right_w } else { width };
    let text_w = left_w.saturating_sub(4).max(1);
    // Long names and messages wrap; a pairing URL and code must stay readable in full.
    let title = if status.airplay_active {
        "AirPlay audio"
    } else if status.youtube_active && status.title.is_empty() {
        "YouTube audio"
    } else if status.title.is_empty() {
        "No track selected"
    } else {
        &status.title
    };
    let mut titles = screen::wrap(title, text_w);
    titles.truncate(2);
    let detail = [status.artist.as_str(), status.album.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    let mut details = screen::wrap(&detail, text_w);
    details.truncate(2);
    let mut lines: Vec<(String, Style)> = messages
        .iter()
        .flat_map(|(text, style)| screen::wrap(text, text_w).into_iter().map(|l| (l, *style)))
        .collect();
    lines.truncate(5);
    // Title, artist, gap, progress, gap, then the messages.
    let middle_h = 2 + titles.len() + details.len() + 3 + lines.len().max(1);
    let left = Area {
        x: 0,
        y: header.h,
        w: left_w,
        h: middle_h,
    };
    let right = if side {
        Area {
            x: left.w,
            y: header.h,
            w: right_w,
            h: middle_h,
        }
    } else {
        Area {
            x: 0,
            y: header.h + middle_h,
            w: width,
            h: 7,
        }
    };
    // Now playing.
    let toggle = if status.active && !status.paused {
        "pause"
    } else {
        "play"
    };
    c.panel(
        left,
        "now playing",
        &[("␣", toggle), ("n", "next"), ("p", "prev"), ("+-", "vol")],
    );
    let inner = left.inner();
    let title_style = if status.title.is_empty() && !status.airplay_active && !status.youtube_active
    {
        Style::new(screen::DIM)
    } else {
        Style::bold(screen::TITLE)
    };
    let mut y = inner.y;
    for line in &titles {
        c.text(inner.x, y, inner.w, line, title_style);
        y += 1;
    }
    for line in &details {
        c.text(inner.x, y, inner.w, line, Style::new(screen::TEXT));
        y += 1;
    }
    let elapsed = clock_text(status.position_ms);
    let total = clock_text(status.duration_ms);
    let bar_w = inner.w.saturating_sub(elapsed.len() + total.len() + 2);
    let progress_y = y + 1;
    c.text(
        inner.x,
        progress_y,
        inner.w,
        &elapsed,
        Style::new(screen::TEXT),
    );
    let fraction = if status.duration_ms == 0 {
        0.0
    } else {
        status.position_ms.min(status.duration_ms) as f32 / status.duration_ms as f32
    };
    c.bar(
        inner.x + elapsed.len() + 1,
        progress_y,
        bar_w,
        fraction,
        ('━', '─'),
        &[screen::ACCENT, screen::INFO],
    );
    c.text_right(
        inner.x + inner.w,
        progress_y,
        &total,
        Style::new(screen::TEXT),
    );
    for (i, (text, style)) in lines.iter().enumerate() {
        c.text(inner.x, progress_y + 2 + i, inner.w, text, *style);
    }
    // Receiver features.
    c.panel(
        right,
        "receiver",
        &[("v", "voice"), ("e", "spectrum"), ("a", "add-ons")],
    );
    let inner = right.inner();
    c.text(inner.x, inner.y, inner.w, "volume", Style::new(screen::DIM));
    let percent = format!("{:>3}%", status.volume);
    let meter_w = inner.w.saturating_sub(10 + percent.len() + 1).min(24);
    c.bar(
        inner.x + 10,
        inner.y,
        meter_w,
        f32::from(status.volume) / 100.0,
        ('■', '■'),
        &screen::HEAT,
    );
    c.text(
        inner.x + 11 + meter_w,
        inner.y,
        percent.len(),
        &percent,
        Style::new(screen::TEXT),
    );
    let toggle = |supported: bool, on: bool| {
        if !supported {
            ("n/a", screen::DIM)
        } else if on {
            ("on", screen::ACCENT)
        } else {
            ("off", screen::DIM)
        }
    };
    let (value, colour) = toggle(status.voice_supported, status.voice);
    feature(&mut c, inner, 1, "voice", value, colour);
    let (value, colour) = toggle(status.spectrum_supported, status.spectrum);
    feature(&mut c, inner, 2, "spectrum", value, colour);
    let (value, colour) = if !status.airplay_available {
        ("not installed", screen::DIM)
    } else if !status.airplay_enabled {
        ("off", screen::DIM)
    } else if !status.airplay_error.is_empty() {
        ("error", screen::ERROR)
    } else if status.airplay_running {
        ("on", screen::ACCENT)
    } else {
        ("starting", screen::WARN)
    };
    feature(&mut c, inner, 3, "airplay", value, colour);
    // The YouTube row needs a sixth line; on a short panel it stays in the add-ons menu only.
    let alice_row = if inner.h >= 6 {
        let (value, colour) = if !status.youtube_available {
            ("not installed", screen::DIM)
        } else if !status.youtube_enabled {
            ("off", screen::DIM)
        } else if !status.youtube_error.is_empty() {
            ("error", screen::ERROR)
        } else if status.youtube_active {
            ("playing", screen::ACCENT)
        } else if status.youtube_running {
            ("ready", screen::ACCENT)
        } else {
            ("starting", screen::WARN)
        };
        feature(&mut c, inner, 4, "youtube", value, colour);
        5
    } else {
        4
    };
    let (value, colour) = if status.ducking {
        ("ducked for Alice", screen::WARN)
    } else {
        ("idle", screen::DIM)
    };
    feature(&mut c, inner, alice_row, "alice", value, colour);
    // Spectrum history fills the rest of the screen.
    let top = right.y + right.h;
    let rest = height.saturating_sub(top);
    if rest < 4 {
        // No room for the graph; the global keys move to the bottom edge of the header.
        c.panel(header, "spotiya", global);
        return c;
    }
    let area = Area {
        x: 0,
        y: top,
        w: width,
        h: rest,
    };
    c.panel(area, "spectrum", global);
    let inner = area.inner();
    let hint = if !status.spectrum_supported {
        Some("The spectrum is not available on this device")
    } else if !status.spectrum {
        Some("Spectrum is off; press e to turn it on")
    } else {
        None
    };
    if let Some(hint) = hint {
        let w = screen::text_width(hint).min(inner.w);
        c.text(
            inner.x + (inner.w - w) / 2,
            inner.y + inner.h / 2,
            inner.w,
            hint,
            Style::new(screen::DIM),
        );
        return c;
    }
    let groups: &[(&str, std::ops::Range<usize>)] = match inner.h {
        0 => &[],
        1 => &[("all", 0..4)],
        2 | 3 => &[("low", 0..2), ("high", 2..4)],
        _ => &[
            ("bass", 0..1),
            ("low mid", 1..2),
            ("high mid", 2..3),
            ("treble", 3..4),
        ],
    };
    let label_w = 9;
    for (i, (name, bands)) in groups.iter().enumerate() {
        let y0 = inner.y + i * inner.h / groups.len();
        let y1 = inner.y + (i + 1) * inner.h / groups.len();
        let graph = Area {
            x: inner.x,
            y: y0,
            w: inner.w.saturating_sub(label_w),
            h: y1 - y0,
        };
        c.graph(graph, &history.series(bands.clone()), &screen::HEAT);
        let level = history
            .0
            .back()
            .map_or(0, |l| l[bands.clone()].iter().copied().max().unwrap_or(0));
        c.text_right(inner.x + inner.w, y0, name, Style::new(screen::DIM));
        if y1 - y0 > 1 {
            c.text_right(
                inner.x + inner.w,
                y0 + 1,
                &format!("{level}%"),
                Style::new(screen::TEXT),
            );
        }
    }
    c
}
fn clock_text(ms: u32) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}
/// Menus keep their text layout; this frames it like the dashboard.
fn menu(content: Vec<String>, width: usize, height: usize) -> Canvas {
    let mut c = Canvas::new(width, height);
    let mut lines = content.into_iter();
    let title = lines
        .next()
        .unwrap_or_default()
        .trim_start_matches("SPOTIYA — ")
        .to_lowercase();
    let area = Area {
        x: 0,
        y: 0,
        w: width,
        h: height,
    };
    c.panel(area, &format!("spotiya · {title}"), &[]);
    let inner = area.inner();
    let mut y = inner.y;
    for line in lines.skip_while(|l| l.is_empty()) {
        let style = if line.starts_with('>') {
            Style::bold(screen::ACCENT)
        } else if line.starts_with("Press x again") {
            Style::bold(screen::WARN)
        } else if line.contains("   [") || line.starts_with("  ") {
            Style::new(screen::TEXT)
        } else {
            Style::new(screen::DIM)
        };
        // List entries keep their indent on wrapped lines.
        let indent = if line.starts_with(['>', ' ']) { 2 } else { 0 };
        let parts = screen::wrap(&line, inner.w.saturating_sub(indent).max(1));
        for (i, part) in parts.iter().enumerate() {
            if y >= inner.y + inner.h {
                return c;
            }
            let x = inner.x + if i > 0 { indent } else { 0 };
            c.text(x, y, inner.w, part, style);
            y += 1;
        }
    }
    c
}
fn show(screen: &mut Screen, canvas: &Canvas) -> io::Result<()> {
    let frame = screen.frame(canvas);
    if frame.is_empty() {
        return Ok(());
    }
    let mut out = io::stdout();
    out.write_all(frame.as_bytes())?;
    out.flush()
}
fn draw_lines(screen: &mut Screen, content: Vec<String>) -> io::Result<()> {
    let (width, height) = size();
    show(screen, &menu(content, width, height))
}
struct ProfileMenu {
    entries: Vec<crate::profile_selection::Choice>,
    selected: usize,
    escape: u8,
    escape_at: Instant,
    current: String,
    notice: String,
}
enum ProfileAction {
    Apply,
    Cancel,
}
impl ProfileMenu {
    fn new(state: &Path, current: &str) -> io::Result<Self> {
        let entries = crate::profile_selection::choices(state)?;
        let selected = entries.iter().position(|c| c.key() == current).unwrap_or(0);
        Ok(Self {
            entries,
            selected,
            escape: 0,
            escape_at: Instant::now(),
            current: current.into(),
            notice: String::new(),
        })
    }
    fn key(&mut self, key: u8) -> Option<ProfileAction> {
        if key == 27 {
            self.escape = 1;
            self.escape_at = Instant::now();
            return None;
        }
        if self.escape == 1 && key == b'[' {
            self.escape = 2;
            return None;
        }
        let key = if self.escape == 2 {
            self.escape = 0;
            match key {
                b'A' => b'k',
                b'B' => b'j',
                _ => return None,
            }
        } else {
            key
        };
        match key {
            b'q' => Some(ProfileAction::Cancel),
            b'\r' | b'\n' => Some(ProfileAction::Apply),
            b'j' | b'n' => {
                self.selected = (self.selected + 1) % self.entries.len();
                None
            }
            b'k' | b'p' => {
                self.selected = (self.selected + self.entries.len() - 1) % self.entries.len();
                None
            }
            _ => None,
        }
    }
    fn lines(&self, height: usize) -> Vec<String> {
        let count = height.saturating_sub(9).max(1);
        let start = self.selected.saturating_sub(count - 1);
        let mut out = vec!["SPOTIYA — DEVICE PROFILE".into(), String::new()];
        for (index, choice) in self.entries.iter().enumerate().skip(start).take(count) {
            out.push(format!(
                "{} {}{}",
                if index == self.selected { ">" } else { " " },
                safe(&choice.name),
                if choice.key() == self.current {
                    " [current]"
                } else {
                    ""
                }
            ));
        }
        out.push(String::new());
        out.push(safe(&self.entries[self.selected].detail));
        out.push("Up/down or j/k: choose   Enter: apply   Esc/q: back".into());
        out.push("Applying restarts a running receiver and stops playback.".into());
        out.push(safe(&self.notice));
        out
    }
    fn draw(&self, screen: &mut Screen) -> io::Result<()> {
        draw_lines(screen, self.lines(size().1))
    }
}
#[derive(Default)]
struct AddonMenu {
    selected: usize,
    escape: u8,
    escape_at: Option<Instant>,
    confirm_remove: bool,
}
enum AddonAction {
    Apply,
    Cancel,
    StopInstall,
    Update,
    Remove,
}
impl AddonMenu {
    fn key(&mut self, key: u8) -> Option<AddonAction> {
        // Removal needs a second x; any other byte, including escape sequences, cancels it.
        let confirmed = std::mem::take(&mut self.confirm_remove);
        if key == 27 {
            self.escape = 1;
            self.escape_at = Some(Instant::now());
            return None;
        }
        if self.escape == 1 && key == b'[' {
            self.escape = 2;
            return None;
        }
        let key = if self.escape == 2 {
            self.escape = 0;
            match key {
                b'A' => b'k',
                b'B' => b'j',
                _ => return None,
            }
        } else {
            key
        };
        match key {
            b'x' if confirmed => Some(AddonAction::Remove),
            b'x' => {
                self.confirm_remove = true;
                None
            }
            b'u' => Some(AddonAction::Update),
            b'q' => Some(AddonAction::Cancel),
            b'c' => Some(AddonAction::StopInstall),
            b'\r' | b'\n' | b'e' => Some(AddonAction::Apply),
            b'j' => {
                self.selected = (self.selected + 1) % crate::addons::CATALOG.len();
                None
            }
            b'k' => {
                self.selected = (self.selected + crate::addons::CATALOG.len() - 1)
                    % crate::addons::CATALOG.len();
                None
            }
            _ => None,
        }
    }
    fn lines(
        &self,
        status: &Status,
        height: usize,
        installing: Option<&str>,
        notice: &str,
    ) -> Vec<String> {
        let count = height.saturating_sub(11).max(1);
        let start = self.selected.saturating_sub(count - 1);
        let mut out = vec!["SPOTIYA — ADD-ONS".into(), String::new()];
        for (index, addon) in crate::addons::CATALOG
            .iter()
            .enumerate()
            .skip(start)
            .take(count)
        {
            let label = if installing == Some(addon.id) {
                "Installing"
            } else {
                (addon.state)(status).label()
            };
            out.push(format!(
                "{} {}   [{}]",
                if index == self.selected { ">" } else { " " },
                addon.name,
                label
            ));
        }
        let addon = &crate::addons::CATALOG[self.selected];
        let state = (addon.state)(status);
        out.push(String::new());
        out.push(addon.description.into());
        if installing == Some(addon.id) {
            out.push("Installation in progress. c: cancel download".into());
        } else if self.confirm_remove {
            out.push(format!("Press x again to remove {}", addon.name));
        } else if state.installed && addon.id == "soloist" {
            out.push("Enter: set API key (hidden input)".into());
            out.push("Select backend: spotiya backend soloist; spotiya restart".into());
        } else if state.installed {
            out.push(format!(
                "Enter: {} {}",
                if state.enabled { "disable" } else { "enable" },
                addon.name
            ));
        } else {
            out.push(format!(
                "Enter: download{} {}",
                if addon.command.is_some() {
                    " and enable"
                } else {
                    ""
                },
                addon.name
            ));
            out.push(format!(
                "GitHub Releases: klockky/spotiya v{}",
                env!("CARGO_PKG_VERSION")
            ));
        }
        if state.installed && installing != Some(addon.id) {
            out.push("u: update   x: remove".into());
        }
        if !state.error.is_empty() {
            out.push(safe(state.error));
        }
        out.push("Up/down or j/k: choose   Esc/q: back".into());
        out.push(String::new());
        out.extend(notice.lines().map(safe));
        out
    }
    fn draw(
        &self,
        screen: &mut Screen,
        status: &Status,
        installing: Option<&str>,
        notice: &str,
    ) -> io::Result<()> {
        draw_lines(screen, self.lines(status, size().1, installing, notice))
    }
}
pub fn run(state_dir: &Path, show_addons: bool) -> io::Result<()> {
    match session(state_dir, show_addons) {
        // The terminal went away, possibly before the first frame; that ends the session.
        Err(e) if e.raw_os_error() == Some(libc::EIO) || e.kind() == io::ErrorKind::BrokenPipe => {
            Ok(())
        }
        result => result,
    }
}
#[derive(Default)]
struct KeyPrompt {
    value: String,
    error: String,
}
impl KeyPrompt {
    fn lines(&self) -> Vec<String> {
        vec![
            "SPOTIFY SOLOIST — API KEY".into(),
            "Create a key: https://developer.spotify.com/dashboard/soloist".into(),
            "Paste your API key. Input is hidden.".into(),
            format!("{} characters entered", self.value.len()),
            "Enter: save   Esc: cancel".into(),
            self.error.clone(),
        ]
    }
    // Returns true when the dialog should close; errors never include the input.
    fn key(&mut self, key: u8, state: &Path, notice: &mut String) -> bool {
        match key {
            27 => {
                *notice =
                    "Key entry cancelled. Select Soloist and press Enter to try again.".into();
                return true;
            }
            b'\r' | b'\n' => match crate::soloist::store_key(state, &self.value) {
                Ok(()) => {
                    *notice =
                        "Key saved. Run spotiya retry; after switching backend, spotiya restart."
                            .into();
                    return true;
                }
                Err(_) => {
                    self.error = "Could not save key: check the key and available storage.".into()
                }
            },
            8 | 127 => {
                self.value.pop();
            }
            c if (c.is_ascii_graphic() || c == b' ') && self.value.len() < 4097 => {
                self.value.push(c as char)
            }
            _ => {}
        }
        false
    }
}

fn session(state_dir: &Path, show_addons: bool) -> io::Result<()> {
    use std::io::BufRead;
    // Declared before the terminal so it drops after it: waiting for an install to stop
    // must not leave the terminal in raw mode.
    let mut addon_job: Option<crate::addons::InstallJob> = None;
    let _terminal = Terminal::enter()?;
    let mut status = Status::default();
    // Key hints live in the panel borders, so the message line starts empty.
    let mut notice = String::new();
    // (is_youtube, typed bytes): a YouTube link prompt vs. a Spotify search prompt.
    let mut prompt: Option<(bool, Vec<u8>)> = None;
    let mut profiles: Option<ProfileMenu> = None;
    let mut addons = show_addons.then(AddonMenu::default);
    let (addon_messages, addon_rx) = mpsc::channel();
    let mut addon_notice = String::new();
    let mut key_prompt: Option<KeyPrompt> = None;
    let (updates, rx) = mpsc::channel::<String>();
    let mut login_socket: Option<std::os::unix::net::UnixStream> = None;
    let mut screen = Screen::new(screen::Colors::detect());
    let mut history = History::default();
    let offset = utc_offset();
    let mut last_level = Instant::now();
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    loop {
        if addons.as_ref().is_some_and(|a| {
            a.escape != 0
                && a.escape_at
                    .is_some_and(|at| at.elapsed() > Duration::from_millis(180))
        }) {
            addons = None;
        }
        if profiles
            .as_ref()
            .is_some_and(|p| p.escape != 0 && p.escape_at.elapsed() > Duration::from_millis(180))
        {
            profiles = None;
        }
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
        while let Ok(message) = addon_rx.try_recv() {
            match message {
                crate::addons::Message::Progress(text) => addon_notice = text,
                crate::addons::Message::Finished(result) => {
                    if let Some(job) = addon_job.take() {
                        let addon = crate::addons::find(job.id)?;
                        if result.is_ok()
                            && addon.id == "soloist"
                            && crate::soloist::key_required(state_dir)
                        {
                            key_prompt = Some(KeyPrompt::default());
                        }
                        addon_notice = match result {
                            Err(e) => e.to_string(),
                            Ok(outcome) => match addon.command {
                                Some(command) if outcome == crate::addons::Outcome::Installed => {
                                    match control::request(&format!("{command} ON")) {
                                        Ok(_) => {
                                            format!("{} installed; enabling...", addon.name)
                                        }
                                        Err(e) => format!(
                                            "Installed. Enable it after starting the receiver: {e}"
                                        ),
                                    }
                                }
                                _ => outcome.message(addon),
                            },
                        };
                    }
                }
            }
        }
        // The graph scrolls at 10 Hz while it is on screen; it asks only while audio plays.
        let graph = addons.is_none() && profiles.is_none() && status.spectrum;
        if graph && last_level.elapsed() >= Duration::from_millis(100) {
            last_level = Instant::now();
            let levels = (status.active && !status.paused)
                .then(|| control::request("LEVELS").ok())
                .flatten()
                .and_then(|reply| control::parse_levels(&reply));
            history.push(levels.unwrap_or_default());
            last_draw = Instant::now() - Duration::from_secs(1);
        }
        if last_draw.elapsed() >= Duration::from_millis(250) {
            if let Some(prompt) = &key_prompt {
                draw_lines(&mut screen, prompt.lines())?;
            } else if let Some(menu) = &addons {
                menu.draw(
                    &mut screen,
                    &status,
                    addon_job.as_ref().map(|j| j.id),
                    &addon_notice,
                )?;
            } else if let Some(menu) = &profiles {
                menu.draw(&mut screen)?;
            } else {
                let (width, height) = size();
                let canvas = dashboard(
                    &status,
                    &notice,
                    prompt.as_ref().map(|(yt, text)| {
                        (if *yt { "youtube" } else { "search" }, text.as_slice())
                    }),
                    &history,
                    &local_clock(offset),
                    width,
                    height,
                );
                show(&mut screen, &canvas)?;
            }
            last_draw = Instant::now();
        }
        let mut input = [0u8; 64];
        let Some(n) = read_keys(&mut input)? else {
            if let Some(s) = login_socket.take() {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
            return Ok(());
        };
        for &key in &input[..n] {
            if key == 3 || key == 4 {
                if let Some(s) = login_socket.take() {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                }
                return Ok(());
            }
            if let Some(prompt) = key_prompt.as_mut() {
                let finished = prompt.key(key, state_dir, &mut addon_notice);
                if finished {
                    key_prompt = None;
                }
                last_draw = Instant::now() - Duration::from_secs(1);
                // Discard the rest of this input batch after saving/cancelling so a
                // pasted newline or escape sequence cannot trigger an add-on action.
                if finished {
                    break;
                }
                continue;
            }
            if let Some(menu) = addons.as_mut() {
                match menu.key(key) {
                    Some(AddonAction::Cancel) => addons = None,
                    Some(AddonAction::StopInstall) => {
                        if let Some(job) = &mut addon_job {
                            job.cancel();
                            addon_notice = "Cancelling download...".into();
                        }
                    }
                    Some(AddonAction::Remove) => {
                        let addon = &crate::addons::CATALOG[menu.selected];
                        addon_notice = if addon_job.is_some() {
                            "An add-on installation is already running".into()
                        } else {
                            match crate::addons::remove(
                                addon.id,
                                Path::new(crate::installation::BASE),
                            ) {
                                Ok(()) => format!("{} removed", addon.name),
                                Err(e) => e.to_string(),
                            }
                        };
                    }
                    Some(AddonAction::Update) => {
                        let addon = &crate::addons::CATALOG[menu.selected];
                        if addon_job.is_some() {
                            addon_notice = "An add-on installation is already running".into();
                        } else if !(addon.state)(&status).installed {
                            addon_notice = format!("{} is not installed", addon.name);
                        } else {
                            match crate::addons::InstallJob::start(addon, addon_messages.clone()) {
                                Ok(job) => {
                                    addon_job = Some(job);
                                    addon_notice = "Checking for an add-on update...".into();
                                }
                                Err(e) => addon_notice = e.to_string(),
                            }
                        }
                    }
                    Some(AddonAction::Apply) => {
                        let addon = &crate::addons::CATALOG[menu.selected];
                        if addon_job.is_some() {
                            addon_notice = "An add-on installation is already running".into();
                        } else if (addon.state)(&status).installed {
                            addon_notice = match addon.command {
                                None if addon.id == "soloist" => {
                                    key_prompt = Some(KeyPrompt::default());
                                    String::new()
                                }
                                None => {
                                    format!("{} ready; used automatically when needed", addon.name)
                                }
                                Some(command) => match control::request(&format!(
                                    "{} {}",
                                    command,
                                    if (addon.state)(&status).enabled {
                                        "OFF"
                                    } else {
                                        "ON"
                                    }
                                )) {
                                    Ok(_) => "Command queued".into(),
                                    Err(e) => e.to_string(),
                                },
                            };
                        } else {
                            match crate::addons::InstallJob::start(addon, addon_messages.clone()) {
                                Ok(job) => {
                                    addon_job = Some(job);
                                    addon_notice = "Preparing add-on installation...".into();
                                }
                                Err(e) => addon_notice = e.to_string(),
                            }
                        }
                    }
                    None => {}
                }
                last_draw = Instant::now() - Duration::from_secs(1);
                continue;
            }
            if let Some(menu) = profiles.as_mut() {
                match menu.key(key) {
                    Some(ProfileAction::Cancel) => profiles = None,
                    Some(ProfileAction::Apply) => {
                        let choice = menu.entries[menu.selected].key().to_owned();
                        menu.notice = "Applying profile...".into();
                        menu.draw(&mut screen)?;
                        if let Some(socket) = login_socket.take() {
                            let _ = socket.shutdown(std::net::Shutdown::Both);
                        }
                        notice = match crate::profile_selection::select(state_dir, &choice) {
                            Ok(message) => message,
                            Err(e) => e.to_string(),
                        };
                        profiles = None;
                    }
                    None => {}
                }
                last_draw = Instant::now() - Duration::from_secs(1);
                continue;
            }
            if let Some((youtube, text)) = prompt.as_mut() {
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
                        let youtube = *youtube;
                        let query = String::from_utf8_lossy(text).into_owned();
                        prompt = None;
                        notice = if query.trim().is_empty() {
                            "Cancelled".into()
                        } else if youtube {
                            match control::request(&format!("YOUTUBE URL {}", query.trim())) {
                                Ok(_) => "Playing on YouTube".into(),
                                Err(e) => e.to_string(),
                            }
                        } else {
                            // The receiver plays YouTube links itself; say which one happens.
                            let link = crate::youtube::link(query.trim()).is_some();
                            match control::request(&format!("PLAY {}", query.trim())) {
                                Ok(_) if link => "Playing on YouTube".into(),
                                Ok(_) => "Spotify search queued".into(),
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
                b'v' => {
                    if status.voice_supported {
                        Some(format!("VOICE {}", if status.voice { "OFF" } else { "ON" }))
                    } else {
                        notice = "Voice control is unavailable in this device profile".into();
                        None
                    }
                }
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
                b'a' => {
                    addons = Some(AddonMenu::default());
                    None
                }
                b'r' => {
                    if status.retry_required && status.retry_after_secs == 0 {
                        Some("RETRY".into())
                    } else {
                        notice =
                            format!("Retry unavailable; cooldown {}s", status.retry_after_secs);
                        None
                    }
                }
                b'x' => Some("RELEASE".to_owned()),
                b'd' => {
                    match ProfileMenu::new(state_dir, &status.profile_selection) {
                        Ok(menu) => profiles = Some(menu),
                        Err(e) => notice = e.to_string(),
                    }
                    None
                }
                b'/' => {
                    prompt = Some((false, Vec::new()));
                    None
                }
                b'y' => {
                    prompt = Some((true, Vec::new()));
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
    fn soloist_key_dialog_hides_input_and_preserves_key_on_cancel() {
        let dir = crate::installation::Temporary::new(&std::env::temp_dir(), "key-dialog").unwrap();
        let mut prompt = KeyPrompt::default();
        let mut notice = String::new();
        for b in b"synthetic-key!" {
            assert!(!prompt.key(*b, &dir.0, &mut notice));
        }
        assert!(!prompt.key(127, &dir.0, &mut notice));
        assert!(!prompt.lines().join("\n").contains("synthetic-key"));
        assert!(prompt.key(b'\r', &dir.0, &mut notice));
        assert!(!notice.contains("synthetic-key"));
        let key = dir.0.join("soloist/api-key");
        assert_eq!(fs::read_to_string(&key).unwrap(), "synthetic-key\n");
        let mut prompt = KeyPrompt::default();
        assert!(!prompt.key(b'\r', &dir.0, &mut notice));
        assert!(!prompt.error.is_empty());
        assert!(prompt.key(27, &dir.0, &mut notice));
        assert_eq!(fs::read_to_string(&key).unwrap(), "synthetic-key\n");
    }
    #[test]
    fn soloist_key_dialog_rejects_overlong_pastes_instead_of_truncating() {
        let dir = crate::installation::Temporary::new(&std::env::temp_dir(), "key-dialog").unwrap();
        let mut prompt = KeyPrompt::default();
        let mut notice = String::new();
        for _ in 0..5000 {
            prompt.key(b'x', &dir.0, &mut notice);
        }
        assert!(!prompt.key(b'\n', &dir.0, &mut notice));
        assert!(crate::soloist::key_required(&dir.0));
    }
    #[test]
    fn addons_screen_shows_install_progress_and_service_states() {
        let mut menu = AddonMenu::default();
        let mut status = Status::default();
        let frame = menu.lines(&status, 24, None, "");
        assert!(frame.iter().any(|s| s == "SPOTIYA — ADD-ONS"));
        assert!(
            frame
                .iter()
                .any(|s| s.contains("AirPlay 2   [Not installed]"))
        );
        assert!(
            frame
                .iter()
                .any(|s| s.contains("Enter: download and enable"))
        );
        assert!(matches!(menu.key(b'\n'), Some(AddonAction::Apply)));
        let frame = menu.lines(&status, 24, Some("airplay"), "Downloading: 45%");
        assert!(frame.iter().any(|s| s.contains("[Installing]")));
        assert!(frame.iter().any(|s| s == "Downloading: 45%"));
        status.airplay_available = true;
        assert!(
            menu.lines(&status, 24, None, "")
                .iter()
                .any(|s| s.contains("[Off]"))
        );
        status.airplay_enabled = true;
        status.airplay_running = true;
        let frame = menu.lines(&status, 24, None, "");
        assert!(frame.iter().any(|s| s.contains("[Running]")));
        assert!(frame.iter().any(|s| s == "Enter: disable AirPlay 2"));
        assert!(frame.iter().any(|s| s == "u: update   x: remove"));
        assert!(matches!(menu.key(b'u'), Some(AddonAction::Update)));
        assert!(menu.key(b'x').is_none());
        assert!(
            menu.lines(&status, 24, None, "")
                .iter()
                .any(|s| s == "Press x again to remove AirPlay 2")
        );
        assert!(menu.key(b'j').is_none());
        assert!(menu.key(b'k').is_none());
        assert!(menu.key(b'x').is_none());
        for key in b"\x1b[C" {
            assert!(menu.key(*key).is_none());
        }
        assert!(menu.key(b'x').is_none());
        assert!(matches!(menu.key(b'x'), Some(AddonAction::Remove)));
        for key in b"\x1b[B" {
            assert!(menu.key(*key).is_none());
        }
        assert_eq!(menu.selected, 1);
        let frame = menu.lines(&status, 24, None, "");
        assert!(
            frame
                .iter()
                .any(|s| s.contains("Enter: download and enable YouTube"))
        );
        status.youtube_available = true;
        let frame = menu.lines(&status, 24, None, "");
        assert!(frame.iter().any(|s| s.contains("Enter: enable YouTube")));
        assert!(matches!(menu.key(b'c'), Some(AddonAction::StopInstall)));
        assert!(matches!(menu.key(b'q'), Some(AddonAction::Cancel)));
    }
    #[test]
    fn soloist_addon_shows_install_update_and_personal_key_instructions() {
        let menu = AddonMenu {
            selected: crate::addons::CATALOG
                .iter()
                .position(|a| a.id == "soloist")
                .unwrap(),
            ..Default::default()
        };
        let mut status = Status::default();
        let frame = menu.lines(&status, 30, None, "");
        assert!(frame.iter().any(|s| s == "Enter: download Spotify Soloist"));
        assert!(
            frame
                .iter()
                .any(|s| s.contains("Official client from Spotify"))
        );
        status.soloist_available = true;
        status.spotify_backend = "soloist".into();
        status.connected = true;
        let frame = menu.lines(&status, 30, None, "");
        assert!(
            frame
                .iter()
                .any(|s| s.contains("Spotify Soloist   [Running]"))
        );
        assert!(frame.iter().any(|s| s.contains("Enter: set API key")));
        assert!(frame.iter().any(|s| s == "u: update   x: remove"));
        assert!(
            !frame
                .iter()
                .any(|s| s.contains("ordinary Spotify audio keys"))
        );
    }
    #[test]
    fn profile_menu_navigation_scrolls_and_cancel_does_not_save() {
        let root = std::env::temp_dir().join(format!("spotiya-menu-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let mut menu = ProfileMenu::new(&root, "auto").unwrap();
        for key in b"\x1b[B\x1b[B\x1b[B" {
            assert!(menu.key(*key).is_none());
        }
        assert_eq!(menu.entries[menu.selected].key(), "generic");
        assert!(
            menu.lines(10)
                .iter()
                .any(|line| line.contains("> Generic ALSA"))
        );
        assert!(matches!(menu.key(b'q'), Some(ProfileAction::Cancel)));
        assert!(crate::profile_selection::read(&root).unwrap().is_none());
        menu.key(b'j');
        assert_eq!(menu.entries[menu.selected].key(), "auto");
        assert!(matches!(menu.key(b'\r'), Some(ProfileAction::Apply)));
        assert!(crate::profile_selection::read(&root).unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn station_time_zone_offset_is_read_without_the_location() {
        // The station's real layout with the coordinates zeroed.
        let mut data = vec![0x0a, 0x12, 0x09];
        data.extend([0; 8]);
        data.push(0x11);
        data.extend([0; 8]);
        data.extend(b"\x12\x12\x0a\x0dEurope/Moscow\x10\xb0\x54");
        assert_eq!(geosync_offset(&data), Some(10800));
        let west = b"\x12\x0b\x10\xe0\xc7\xff\xff\xff\xff\xff\xff\xff\x01";
        assert_eq!(geosync_offset(west), Some(-7200));
        assert_eq!(geosync_offset(b"\x12\x04\x0a\x02ab"), None);
        assert_eq!(geosync_offset(b"\x12\x09"), None);
        assert_eq!(geosync_offset(b"\x0b"), None);
        assert_eq!(geosync_offset(b""), None);
    }
    fn rows(status: &Status, notice: &str, width: usize, height: usize) -> Vec<String> {
        let canvas = dashboard(
            status,
            notice,
            None,
            &History::default(),
            "12:00",
            width,
            height,
        );
        (0..height).map(|y| canvas.row(y)).collect()
    }
    fn has(rows: &[String], text: &str) -> bool {
        rows.iter().any(|row| row.contains(text))
    }
    #[test]
    fn youtube_playback_is_labelled_youtube_with_its_title() {
        let mut status = Status {
            connected: true,
            active: true,
            youtube_active: true,
            title: "linkin kla$ - given up".into(),
            ..Default::default()
        };
        let frame = rows(&status, "", 100, 30);
        assert!(has(&frame, "YOUTUBE"));
        assert!(has(&frame, "linkin kla$ - given up"));
        assert!(has(&frame, "YouTube priority"));
        assert!(!has(&frame, "AIRPLAY") && !has(&frame, "AirPlay"));
        status.paused = true;
        assert!(has(&rows(&status, "", 100, 30), "YOUTUBE PAUSED"));
        status.title.clear();
        assert!(has(&rows(&status, "", 100, 30), "YouTube audio"));
    }
    #[test]
    fn youtube_link_prompt_and_status_row_are_shown() {
        let status = Status {
            connected: true,
            youtube_available: true,
            youtube_enabled: true,
            youtube_running: true,
            youtube_active: true,
            ..Default::default()
        };
        let canvas = dashboard(
            &status,
            "",
            Some(("youtube", b"https://youtu.be/O5sjrLI5mSs")),
            &History::default(),
            "12:00",
            100,
            40,
        );
        let frame: Vec<String> = (0..40).map(|y| canvas.row(y)).collect();
        assert!(has(&frame, "youtube › https://youtu.be/O5sjrLI5mSs"));
        assert!(
            frame
                .iter()
                .any(|r| r.contains("youtube") && r.contains("playing"))
        );
    }
    #[test]
    fn loading_and_failed_tracks_are_not_shown_as_playing() {
        let mut status = Status {
            connected: true,
            loading: true,
            ..Default::default()
        };
        assert!(has(&rows(&status, "", 80, 24), "● LOADING"));
        status.loading = false;
        status.error = "Track unavailable".into();
        let frame = rows(&status, "", 80, 24);
        assert!(has(&frame, "● UNAVAILABLE"));
        assert!(has(&frame, "Track unavailable"));
        assert!(!has(&frame, "PLAYING"));
    }
    #[test]
    fn latched_failure_is_visible_while_spotify_is_disconnected() {
        let status = Status {
            retry_required: true,
            retry_after_secs: 12,
            error: "Audio key request failed".into(),
            ..Default::default()
        };
        let frame = rows(&status, "", 90, 24);
        assert!(has(&frame, "● RETRY REQUIRED"));
        assert!(has(&frame, "Retry available in 12s"));
        assert!(has(&frame, "Audio key request failed"));
    }
    #[test]
    fn refused_login_replaces_connecting_while_disconnected() {
        let mut status = Status {
            login_refused: "credentials".into(),
            error: "Saved Spotify login was rejected; pair again with spotiya login".into(),
            ..Default::default()
        };
        let frame = rows(&status, "", 90, 24);
        assert!(has(&frame, "● LOGIN REQUIRED"));
        assert!(has(&frame, "pair again"));
        status.login_refused = "premium".into();
        assert!(has(&rows(&status, "", 90, 24), "● PREMIUM REQUIRED"));
        status.login_refused.clear();
        assert!(has(&rows(&status, "", 90, 24), "● CONNECTING"));
    }
    #[test]
    fn spectrum_state_is_visible_for_supported_and_other_devices() {
        let mut status = Status::default();
        let frame = rows(&status, "", 80, 24);
        assert!(has(&frame, "not available on this device"));
        assert!(
            frame
                .iter()
                .any(|r| r.contains("spectrum") && r.contains("○ n/a"))
        );
        status.spectrum_supported = true;
        let frame = rows(&status, "", 80, 24);
        assert!(has(&frame, "press e to turn it on"));
        assert!(
            frame
                .iter()
                .any(|r| r.contains("spectrum") && r.contains("○ off"))
        );
        status.spectrum = true;
        let frame = rows(&status, "", 80, 24);
        assert!(!has(&frame, "press e"));
        assert!(
            frame
                .iter()
                .any(|r| r.contains("spectrum") && r.contains("● on"))
        );
        assert!(has(&frame, "e spectrum"));
        assert!(has(&frame, "bass") && has(&frame, "treble"));
    }
    #[test]
    fn the_graph_scrolls_with_new_levels_and_groups_bands_on_short_screens() {
        let status = Status {
            spectrum_supported: true,
            spectrum: true,
            ..Default::default()
        };
        let mut history = History::default();
        for _ in 0..HISTORY + 10 {
            history.push([100, 0, 0, 0]);
        }
        assert_eq!(history.0.len(), HISTORY);
        let canvas = dashboard(&status, "", None, &history, "", 80, 24);
        let full: Vec<String> = (0..24).map(|y| canvas.row(y)).collect();
        assert!(has(&full, "⣿⣿⣿"));
        assert!(has(&full, "100%"));
        // A very wide window still fills from the left edge once enough audio has played.
        let width = 1000;
        let canvas = dashboard(&status, "", None, &history, "", width, 40);
        let graph = (0..40)
            .map(|y| canvas.row(y))
            .find(|r| r.contains('⣿'))
            .unwrap();
        assert_eq!(graph.matches('⣿').count(), width - 4 - 9);
        // Short terminals merge the four bands into low and high.
        let canvas = dashboard(&status, "", None, &history, "", 80, 15);
        let short: Vec<String> = (0..15).map(|y| canvas.row(y)).collect();
        assert!(has(&short, "low") && has(&short, "high") && !has(&short, "bass"));
    }
    #[test]
    fn layouts_fit_narrow_and_tiny_terminals() {
        let status = Status {
            title: "No Surprises".into(),
            artist: "Radiohead".into(),
            volume: 80,
            ..Default::default()
        };
        for (width, height) in [(50, 30), (72, 20), (120, 40)] {
            let frame = rows(&status, "", width, height);
            assert!(frame.iter().all(|r| screen::text_width(r) == width));
            assert!(has(&frame, "No Surprises") && has(&frame, "Radiohead"));
            assert!(has(&frame, " 80%"));
        }
        let tiny = rows(&status, "Code: ABC", 30, 6);
        assert!(has(&tiny, "Enlarge the terminal") && has(&tiny, "Code: ABC"));
    }
    #[test]
    fn long_names_and_pairing_messages_wrap_instead_of_being_cut() {
        let status = Status {
            title: "Bohemian Rhapsody - Remastered 2011 Version With A Very Long Subtitle".into(),
            artist: "Queen".into(),
            album: "A Night at the Opera (Deluxe Remastered Version)".into(),
            ..Default::default()
        };
        let url = "https://accounts.spotify.com/pair?code=ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let notice = format!("Open {url}\nCode: QX7-42\nWaiting for approval... (Ctrl+C cancels)");
        let frame = rows(&status, &notice, 80, 30);
        // The now playing panel is the left 44 columns; wrapped lines rejoin with a space.
        let text = frame
            .iter()
            .map(|r| {
                r.chars()
                    .take(42)
                    .skip(2)
                    .collect::<String>()
                    .trim()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("Bohemian Rhapsody") && text.contains("Subtitle"));
        assert!(text.contains("(Deluxe Remastered Version)"));
        // The URL has no spaces, so it splits between characters.
        assert!(text.replace(' ', "").contains(url));
        assert!(text.contains("Code: QX7-42") && text.contains("(Ctrl+C cancels)"));
        // The progress bar and spectrum panel still fit below the taller panel.
        assert!(has(&frame, "0:00") && has(&frame, "spectrum"));
    }
    #[test]
    fn metadata_cannot_inject_terminal_controls_and_empty_progress_is_safe() {
        assert_eq!(safe("Song\x1b[2J\r\n"), "Song[2J");
        let mut s = Status {
            title: "\x1b[31mTitle".into(),
            artist: "\x07bell\x1b]0;owned\x07".into(),
            ..Default::default()
        };
        let canvas = dashboard(&s, "notice\x1b[2J", None, &History::default(), "", 80, 24);
        let frame = Screen::new(screen::Colors::True).frame(&canvas);
        assert!(
            !frame.contains("\x1b[31m") && !frame.contains("\x1b]0") && !frame.contains('\x07')
        );
        assert!(!frame.contains("\x1b[2J\x1b") || frame.starts_with("\x1b[0m\x1b[2J"));
        s.duration_ms = 100;
        s.position_ms = 999;
        let _ = rows(&s, "", 20, 5);
        let _ = rows(&s, "", 80, 24);
    }
}
