//! Navidrome (Subsonic/OpenSubsonic API) client: configuration, search and stream requests.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, IsTerminal, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::Duration,
};

pub const CONFIG: &str = "navidrome.json";
const API_VERSION: &str = "1.16.1";
const API_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
const MAX_QUEUE: usize = 500;
const MAX_ARTIST_ALBUMS: usize = 5;

// The password is never stored: Subsonic token auth only needs md5(password + salt).
#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    pub url: String,
    pub user: String,
    pub salt: String,
    pub token: String,
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default = "default_bitrate")]
    pub max_bitrate: u32,
    #[serde(default = "default_true")]
    pub scrobble: bool,
}
fn default_format() -> String {
    "mp3".into()
}
fn default_bitrate() -> u32 {
    256
}
fn default_true() -> bool {
    true
}
fn is_md5(text: &str) -> bool {
    text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit())
}
impl Config {
    pub fn new(url: &str, user: &str, password: &str) -> Result<Self, String> {
        if password.is_empty() || password.len() > 256 || password.contains(['\0', '\n', '\r']) {
            return Err("Invalid Navidrome password".into());
        }
        let salt = uuid::Uuid::new_v4().simple().to_string();
        let token = format!("{:x}", md5::compute(format!("{password}{salt}")));
        let config = Self {
            url: url.trim().trim_end_matches('/').to_owned(),
            user: user.trim().to_owned(),
            salt,
            token,
            format: default_format(),
            max_bitrate: default_bitrate(),
            scrobble: true,
        };
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<(), String> {
        let rest = self
            .url
            .strip_prefix("https://")
            .or_else(|| self.url.strip_prefix("http://"))
            .ok_or("Navidrome URL must start with http:// or https://")?;
        if rest.is_empty()
            || self.url.len() > 200
            || self
                .url
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '?' | '#' | '@'))
        {
            return Err("Invalid Navidrome URL".into());
        }
        if self.user.is_empty() || self.user.len() > 128 || self.user.chars().any(char::is_control)
        {
            return Err("Invalid Navidrome user name".into());
        }
        if !is_md5(&self.salt) || !is_md5(&self.token) {
            return Err("Invalid Navidrome credentials".into());
        }
        if !matches!(self.format.as_str(), "mp3" | "raw") {
            return Err("Navidrome format must be mp3 or raw".into());
        }
        if !(32..=320).contains(&self.max_bitrate) {
            return Err("Navidrome max_bitrate must be 32..320".into());
        }
        Ok(())
    }
    pub fn load(dir: &Path) -> Option<Self> {
        let text = fs::read_to_string(dir.join(CONFIG)).ok()?;
        match serde_json::from_str::<Self>(&text)
            .map_err(|e| e.to_string())
            .and_then(|c| c.validate().map(|_| c))
        {
            Ok(config) => Some(config),
            Err(e) => {
                eprintln!("navidrome: ignoring invalid {CONFIG}: {e}");
                None
            }
        }
    }
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        let path = dir.join(CONFIG);
        let temp = path.with_extension("new");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(
            serde_json::to_string(self)
                .map_err(io::Error::other)?
                .as_bytes(),
        )?;
        file.sync_all()?;
        fs::rename(temp, path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Track {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u32,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Envelope {
    #[serde(rename = "subsonic-response")]
    response: Response,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct Response {
    status: String,
    error: Option<ApiError>,
    #[serde(rename = "searchResult3")]
    search: Option<Search>,
    album: Option<Entry>,
    artist: Option<Entry>,
    playlist: Option<Entry>,
    playlists: Option<Playlists>,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiError {
    message: String,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct Search {
    artist: Vec<Named>,
    album: Vec<Named>,
    song: Vec<Song>,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct Playlists {
    playlist: Vec<Named>,
}
#[derive(Deserialize, Default, Clone)]
#[serde(default)]
struct Named {
    id: String,
    name: String,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct Entry {
    album: Vec<Named>,
    song: Vec<Song>,
    entry: Vec<Song>,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct Song {
    id: String,
    title: String,
    artist: String,
    album: String,
    duration: f64,
}
impl Song {
    fn track(self) -> Option<Track> {
        (!self.id.is_empty()).then(|| Track {
            id: self.id,
            title: self.title,
            artist: self.artist,
            album: self.album,
            duration_ms: (self.duration.clamp(0.0, 86_400.0) * 1000.0) as u32,
        })
    }
}
fn tracks(songs: Vec<Song>) -> Vec<Track> {
    songs
        .into_iter()
        .filter_map(Song::track)
        .take(MAX_QUEUE)
        .collect()
}

fn norm(text: &str) -> String {
    text.to_lowercase()
        .replace('ё', "е")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, PartialEq, Eq)]
enum Pick {
    Playlist(String),
    Artist(String),
    Album(String),
    Songs,
    Nothing,
}
// An exact name wins; otherwise search ranking decides.
fn pick(query: &str, search: &Search, playlists: &[Named]) -> Pick {
    let q = norm(query);
    let exact = |items: &[Named]| {
        items
            .iter()
            .find(|n| norm(&n.name) == q)
            .map(|n| n.id.clone())
    };
    if let Some(id) = exact(playlists) {
        Pick::Playlist(id)
    } else if let Some(id) = exact(&search.artist) {
        Pick::Artist(id)
    } else if let Some(id) = exact(&search.album) {
        Pick::Album(id)
    } else if !search.song.is_empty() {
        Pick::Songs
    } else if let Some(artist) = search.artist.first() {
        Pick::Artist(artist.id.clone())
    } else if let Some(album) = search.album.first() {
        Pick::Album(album.id.clone())
    } else {
        Pick::Nothing
    }
}

pub struct Client {
    http: reqwest::Client,
    cfg: Config,
}
impl Client {
    pub fn new(cfg: Config) -> Result<Self, String> {
        cfg.validate()?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .user_agent("spotiya")
            .build()
            .map_err(|e| e.without_url().to_string())?;
        Ok(Self { http, cfg })
    }
    pub fn config(&self) -> &Config {
        &self.cfg
    }
    fn auth(&self) -> [(&'static str, &str); 6] {
        [
            ("u", &self.cfg.user),
            ("t", &self.cfg.token),
            ("s", &self.cfg.salt),
            ("v", API_VERSION),
            ("c", "spotiya"),
            ("f", "json"),
        ]
    }
    fn get(&self, method: &str) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{}/rest/{method}", self.cfg.url))
            .query(&self.auth())
    }
    // Errors never include the request URL: it carries the authentication token.
    async fn call(&self, request: reqwest::RequestBuilder) -> Result<Response, String> {
        let response = request
            .timeout(API_TIMEOUT)
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        if !response.status().is_success() {
            return Err(format!("Navidrome HTTP {}", response.status().as_u16()));
        }
        let body = response
            .bytes()
            .await
            .map_err(|e| e.without_url().to_string())?;
        if body.len() > MAX_RESPONSE {
            return Err("Navidrome response is too large".into());
        }
        let envelope: Envelope =
            serde_json::from_slice(&body).map_err(|_| "Not a Subsonic response".to_owned())?;
        let response = envelope.response;
        if response.status != "ok" {
            return Err(response
                .error
                .map(|e| {
                    e.message
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(200)
                        .collect()
                })
                .filter(|m: &String| !m.is_empty())
                .unwrap_or_else(|| "Navidrome request failed".into()));
        }
        Ok(response)
    }
    pub async fn ping(&self) -> Result<(), String> {
        self.call(self.get("ping")).await.map(|_| ())
    }
    async fn search(&self, query: &str) -> Result<Search, String> {
        let request = self.get("search3").query(&[
            ("query", query),
            ("artistCount", "5"),
            ("albumCount", "5"),
            ("songCount", "50"),
        ]);
        Ok(self.call(request).await?.search.unwrap_or_default())
    }
    async fn playlists(&self) -> Vec<Named> {
        match self.call(self.get("getPlaylists")).await {
            Ok(r) => r.playlists.unwrap_or_default().playlist,
            Err(_) => Vec::new(),
        }
    }
    async fn album(&self, id: &str) -> Result<Vec<Track>, String> {
        let r = self.call(self.get("getAlbum").query(&[("id", id)])).await?;
        Ok(tracks(r.album.unwrap_or_default().song))
    }
    async fn artist(&self, id: &str) -> Result<Vec<Track>, String> {
        let r = self
            .call(self.get("getArtist").query(&[("id", id)]))
            .await?;
        let mut queue = Vec::new();
        for album in r
            .artist
            .unwrap_or_default()
            .album
            .iter()
            .take(MAX_ARTIST_ALBUMS)
        {
            queue.extend(self.album(&album.id).await.unwrap_or_default());
            if queue.len() >= MAX_QUEUE {
                queue.truncate(MAX_QUEUE);
                break;
            }
        }
        Ok(queue)
    }
    async fn playlist(&self, id: &str) -> Result<Vec<Track>, String> {
        let r = self
            .call(self.get("getPlaylist").query(&[("id", id)]))
            .await?;
        Ok(tracks(r.playlist.unwrap_or_default().entry))
    }
    /// Turns a spoken or typed query into a play queue: playlist, artist, album or songs.
    pub async fn resolve(&self, query: &str) -> Result<Vec<Track>, String> {
        let (search, playlists) = tokio::join!(self.search(query), self.playlists());
        let search = search?;
        let queue = match pick(query, &search, &playlists) {
            Pick::Playlist(id) => self.playlist(&id).await?,
            Pick::Artist(id) => self.artist(&id).await?,
            Pick::Album(id) => self.album(&id).await?,
            Pick::Songs => tracks(search.song),
            Pick::Nothing => Vec::new(),
        };
        if queue.is_empty() {
            Err("Nothing found in Navidrome".into())
        } else {
            Ok(queue)
        }
    }
    pub fn stream(&self, id: &str) -> reqwest::RequestBuilder {
        let request = self.get("stream").query(&[("id", id)]);
        if self.cfg.format == "raw" {
            request.query(&[("format", "raw")])
        } else {
            request.query(&[
                ("format", self.cfg.format.as_str()),
                ("maxBitRate", &self.cfg.max_bitrate.to_string()),
            ])
        }
    }
    pub async fn scrobble(&self, id: &str, submission: bool) {
        let _ = self
            .call(self.get("scrobble").query(&[
                ("id", id),
                ("submission", if submission { "true" } else { "false" }),
            ]))
            .await;
    }
}

/// `spotiya navidrome-setup URL USER`: the password comes from stdin and only its
/// salted hash is sent to the receiver, which verifies it against the server first.
pub fn setup_cli(url: &str, user: &str) -> Result<(), String> {
    let interactive = io::stdin().is_terminal();
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    let hidden = interactive && unsafe { libc::tcgetattr(0, &mut saved) } == 0;
    if hidden {
        let mut quiet = saved;
        quiet.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &quiet) };
    }
    if interactive {
        eprint!("Navidrome password for {user}: ");
        let _ = io::stderr().flush();
    }
    let mut password = String::new();
    let read = io::stdin().read_line(&mut password);
    if hidden {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
        eprintln!();
    }
    read.map_err(|e| e.to_string())?;
    let config = Config::new(url, user, password.trim_end_matches(['\n', '\r']))?;
    let json = serde_json::to_string(&config).map_err(|e| e.to_string())?;
    crate::control::request_within(&format!("NAVIDROME SETUP {json}"), Duration::from_secs(25))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
pub mod mock {
    //! A tiny HTTP/1.1 server for tests: `route(path_and_query)` returns (content type, body).
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    pub async fn serve(route: fn(&str) -> (&'static str, Vec<u8>)) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut request = vec![0u8; 4096];
                    let n = socket.read(&mut request).await.unwrap_or(0);
                    let text = String::from_utf8_lossy(&request[..n]);
                    let target = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let (kind, body) = route(&target);
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        url
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn named(id: &str, name: &str) -> Named {
        Named {
            id: id.into(),
            name: name.into(),
        }
    }
    fn song(id: &str) -> Song {
        Song {
            id: id.into(),
            title: id.into(),
            ..Default::default()
        }
    }
    fn config(url: &str) -> Config {
        let mut config = Config::new("https://music.example", "alice", "secret").unwrap();
        config.url = url.into();
        config
    }
    #[test]
    fn credentials_are_hashed_validated_and_never_contain_the_password() {
        let config = Config::new(" https://music.example/ ", "alice", "secret").unwrap();
        assert_eq!(config.url, "https://music.example");
        assert_eq!(
            config.token,
            format!("{:x}", md5::compute(format!("secret{}", config.salt)))
        );
        assert!(!serde_json::to_string(&config).unwrap().contains("secret"));
        assert!(Config::new("ftp://music.example", "alice", "secret").is_err());
        assert!(Config::new("https://user@music.example", "alice", "secret").is_err());
        assert!(Config::new("https://music.example", "", "secret").is_err());
        assert!(Config::new("https://music.example", "alice", "").is_err());
        let mut bad = config.clone();
        bad.format = "opus".into();
        assert!(bad.validate().is_err());
        bad = config;
        bad.max_bitrate = 9;
        assert!(bad.validate().is_err());
    }
    #[test]
    fn config_file_is_private_and_reloads() {
        let dir = std::env::temp_dir().join(format!("spotiya-nd-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let config = Config::new("http://192.168.1.2:4533", "bob", "pw").unwrap();
        config.save(&dir).unwrap();
        let mode = fs::metadata(dir.join(CONFIG)).unwrap();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
            0o600
        );
        let loaded = Config::load(&dir).unwrap();
        assert_eq!((loaded.user.as_str(), loaded.token), ("bob", config.token));
        fs::write(dir.join(CONFIG), "{\"url\":\"x\"}").unwrap();
        assert!(Config::load(&dir).is_none());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn exact_names_beat_search_ranking() {
        let search = Search {
            artist: vec![named("a1", "Radiohead Tribute"), named("a2", "Radiohead")],
            album: vec![named("b1", "OK Computer")],
            song: vec![song("s1")],
        };
        let lists = [named("p1", "Утро")];
        assert_eq!(
            pick("radiohead", &search, &lists),
            Pick::Artist("a2".into())
        );
        assert_eq!(
            pick("ok  computer!", &search, &lists),
            Pick::Album("b1".into())
        );
        assert_eq!(pick("утро", &search, &lists), Pick::Playlist("p1".into()));
        assert_eq!(pick("no surprises", &search, &lists), Pick::Songs);
        let no_songs = Search {
            song: Vec::new(),
            ..search
        };
        assert_eq!(pick("radio", &no_songs, &[]), Pick::Artist("a1".into()));
        assert_eq!(pick("x", &Search::default(), &[]), Pick::Nothing);
    }
    #[test]
    fn songs_without_ids_are_dropped_and_duration_is_bounded() {
        let parsed: Response = serde_json::from_str(
            r#"{"searchResult3":{"song":[{"id":"1","title":"A","duration":183},{"title":"no id"},{"id":"2","duration":-5}]}}"#,
        )
        .unwrap();
        let list = tracks(parsed.search.unwrap().song);
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].duration_ms, list[1].duration_ms), (183_000, 0));
    }
    fn route(target: &str) -> (&'static str, Vec<u8>) {
        let ok = |body: &str| {
            (
                "application/json",
                format!("{{\"subsonic-response\":{{\"status\":\"ok\",{body}}}}}").into_bytes(),
            )
        };
        match target.split('?').next().unwrap() {
            "/rest/ping" => ok("\"version\":\"1.16.1\""),
            "/rest/getPlaylists" => ok("\"playlists\":{}"),
            "/rest/search3" => ok("\"searchResult3\":{\"song\":[{\"id\":\"t1\",\"title\":\"Hit\",\"artist\":\"Band\",\"album\":\"LP\",\"duration\":200}]}"),
            _ => (
                "application/json",
                br#"{"subsonic-response":{"status":"failed","error":{"code":70,"message":"Not found"}}}"#.to_vec(),
            ),
        }
    }
    fn hostile(_: &str) -> (&'static str, Vec<u8>) {
        (
            "application/json",
            br#"{"subsonic-response":{"status":"failed","error":{"message":"\u001b[2Jtext\r\n"}}}"#
                .to_vec(),
        )
    }
    #[tokio::test]
    async fn resolves_a_query_through_the_api_and_reports_errors_without_urls() {
        let client = Client::new(config(&mock::serve(route).await)).unwrap();
        client.ping().await.unwrap();
        let queue = client.resolve("hit").await.unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(
            (queue[0].id.as_str(), queue[0].duration_ms),
            ("t1", 200_000)
        );
        assert_eq!(client.album("x").await.unwrap_err(), "Not found");
        let hostile = Client::new(config(&mock::serve(hostile).await)).unwrap();
        assert_eq!(hostile.ping().await.unwrap_err(), "[2Jtext");
        let down = Client::new(config("http://127.0.0.1:1")).unwrap();
        let error = down.ping().await.unwrap_err();
        assert!(!error.contains(&client.config().token) && !error.contains("127.0.0.1"));
        let stream = client.stream("t1").build().unwrap();
        let query = stream.url().query().unwrap();
        assert!(query.contains("format=mp3") && query.contains("maxBitRate=256"));
    }
}
