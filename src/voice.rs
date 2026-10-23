use crate::source::Source;
use std::{collections::VecDeque, io, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
    time::Instant,
};

const ALICE: &str = "127.0.0.1:9875";
pub const CONTROL: &str = "/run/spotiya.control";
const MAX_FRAME: usize = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// A search query, optionally for a specific source instead of the default one.
    Play(String, Option<Source>),
    Source(Source),
    NavidromeReload,
    Resume,
    Pause,
    Next,
    Previous,
    Toggle,
    Volume(u16),
    Voice(bool),
    Spectrum(bool),
    Release,
}
pub struct Event {
    pub intent: Intent,
    pub received: Instant,
    pub from_voice: bool,
}

// "... в навидроме", "... из спотифая": the last two words pick the source.
fn split_source(query: &str) -> (String, Option<Source>) {
    let words: Vec<&str> = query.split_whitespace().collect();
    if let [head @ .., preposition, name] = words.as_slice()
        && !head.is_empty()
        && ["в", "из", "на", "с", "со", "через"].contains(preposition)
    {
        let source = if name.starts_with("навидром") || name.starts_with("navidrom") {
            Some(Source::Navidrome)
        } else if name.starts_with("спотиф") || name.starts_with("spotif") {
            Some(Source::Spotify)
        } else {
            None
        };
        if source.is_some() {
            return (head.join(" "), source);
        }
    }
    (query.to_owned(), None)
}
pub fn intent(text: &str) -> Option<Intent> {
    let text = text
        .trim()
        .trim_matches(['.', '!', '?'])
        .to_lowercase()
        .replace('ё', "е");
    let text = text.strip_prefix("алиса, ").unwrap_or(&text).trim();
    match text {
        "стоп"
        | "пауза"
        | "останови музыку"
        | "выключи музыку"
        | "поставь на паузу"
        | "останови воспроизведение" => return Some(Intent::Pause),
        "дальше" | "следующий" | "следующий трек" | "следующая песня" | "переключи трек" =>
        {
            return Some(Intent::Next);
        }
        "назад" | "предыдущий трек" | "предыдущая песня" => {
            return Some(Intent::Previous);
        }
        "продолжи" | "продолжи музыку" | "продолжай музыку" | "включи музыку" =>
        {
            return Some(Intent::Resume);
        }
        _ => {}
    }
    let rest = ["включи ", "поставь ", "запусти ", "сыграй "]
        .iter()
        .find_map(|p| text.strip_prefix(p))?
        .trim();
    let music_prefix = [
        "песню ",
        "трек ",
        "музыку ",
        "группу ",
        "исполнителя ",
        "альбом ",
        "плейлист ",
    ];
    let explicit = music_prefix.iter().find_map(|p| rest.strip_prefix(p));
    let query = explicit.unwrap_or(rest).trim().trim_matches('"').trim();
    let (query, source) = split_source(query);
    let query = query.as_str();
    if query.is_empty() || query.len() > 400 {
        return None;
    }
    let home = [
        "свет",
        "освещение",
        "ламп",
        "люстр",
        "торшер",
        "ночник",
        "розетк",
        "телевизор",
        "тв",
        "кондиционер",
        "вентилятор",
        "обогревател",
        "пылесос",
        "робот",
        "чайник",
        "будильник",
        "таймер",
        "блютуз",
        "bluetooth",
        "режим",
        "экран",
        "микрофон",
        "громк",
        "звук",
        "отоплен",
        "увлажнител",
        "очистител",
        "зарядк",
    ];
    if explicit.is_none()
        && query
            .split_whitespace()
            .any(|word| home.iter().any(|p| word.starts_with(p)))
    {
        return None;
    }
    Some(Intent::Play(query.to_owned(), source))
}

// QuasarMessage uses {252: serialized payload, 253: message type}.
// Only the AliceState fields needed for a *final* voice recognition are read.
fn varint(data: &[u8], offset: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *data.get(*offset)?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Some(value);
        }
    }
    None
}
#[derive(Clone, Copy)]
enum Value<'a> {
    Number(u64),
    Bytes(&'a [u8]),
}
fn field(data: &[u8], wanted: u64) -> Option<Value<'_>> {
    let mut offset = 0;
    while offset < data.len() {
        let key = varint(data, &mut offset)?;
        if key >> 3 == 0 {
            return None;
        }
        let value = match key & 7 {
            0 => Value::Number(varint(data, &mut offset)?),
            2 => {
                let len = usize::try_from(varint(data, &mut offset)?).ok()?;
                let end = offset.checked_add(len)?;
                let bytes = data.get(offset..end)?;
                offset = end;
                Value::Bytes(bytes)
            }
            1 | 5 => {
                let len = if key & 7 == 1 { 8 } else { 4 };
                offset = offset.checked_add(len)?;
                if offset > data.len() {
                    return None;
                }
                continue;
            }
            _ => return None,
        };
        if key >> 3 == wanted {
            return Some(value);
        }
    }
    None
}
fn bytes(data: &[u8], number: u64) -> Option<&[u8]> {
    if let Value::Bytes(v) = field(data, number)? {
        Some(v)
    } else {
        None
    }
}
fn number(data: &[u8], field_number: u64) -> Option<u64> {
    if let Value::Number(v) = field(data, field_number)? {
        Some(v)
    } else {
        None
    }
}
fn recognized(data: &[u8]) -> Option<(String, String)> {
    if number(data, 253) != Some(3) {
        return None;
    }
    let state = bytes(data, 252)?;
    if number(state, 1) != Some(3) {
        return None;
    } // final recognition / THINKING
    let result = bytes(state, 15)?;
    if number(result, 2) != Some(1) || number(result, 4) != Some(1) {
        return None;
    }
    let text = std::str::from_utf8(bytes(result, 1)?).ok()?;
    if text.is_empty() || text.len() > 512 || Some(text.as_bytes()) != bytes(state, 2) {
        return None;
    }
    let request = std::str::from_utf8(bytes(state, 7)?).ok()?;
    if request.len() != 36 {
        return None;
    }
    Some((request.to_owned(), text.to_owned()))
}
fn put_var(out: &mut Vec<u8>, mut v: u64) {
    while v >= 128 {
        out.push((v as u8 & 127) | 128);
        v >>= 7;
    }
    out.push(v as u8);
}
fn put_number(out: &mut Vec<u8>, field: u64, value: u64) {
    put_var(out, field << 3);
    put_var(out, value);
}
fn put_bytes(out: &mut Vec<u8>, field: u64, value: &[u8]) {
    put_var(out, (field << 3) | 2);
    put_var(out, value.len() as u64);
    out.extend_from_slice(value);
}
fn rpc(capability: &str, field: u64, method: &[u8]) -> Vec<u8> {
    let mut remoting = Vec::new();
    put_bytes(&mut remoting, 7, capability.as_bytes());
    put_bytes(&mut remoting, field, method);
    let mut message = Vec::new();
    put_bytes(&mut message, 252, &remoting);
    put_number(&mut message, 253, 210);
    let mut frame = (message.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(&message);
    frame
}
pub async fn cancel_alice() -> io::Result<()> {
    // Native CLI 'c': AliceCapabilityMethod kind 3, cancel the conversation.
    send_native(rpc("AliceCapability", 8, &[8, 3])).await
}
pub async fn pause_native() -> io::Result<()> {
    // Native CLI 'o': PlaybackControlCapabilityMethod kind 1 (pause).
    let mut source = Vec::new();
    put_number(&mut source, 1, 14);
    put_number(&mut source, 2, 3);
    put_number(&mut source, 3, 1);
    put_bytes(&mut source, 4, uuid::Uuid::new_v4().to_string().as_bytes());
    put_bytes(&mut source, 6, b"SpotifyVoice");
    let mut method = vec![8, 1];
    put_bytes(&mut method, 5, &source);
    put_number(&mut method, 7, 0);
    send_native(rpc("PlaybackControlCapability", 13, &method)).await
}
pub fn guard_native_pause() {
    tokio::spawn(async {
        for delay in [200, 300, 500] {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            let _ = pause_native().await;
        }
    });
}
pub fn playable_track(text: &str) -> Option<String> {
    use librespot::core::spotify_uri::SpotifyUri;
    let uri = SpotifyUri::from_uri(text.trim()).ok()?;
    match uri {
        SpotifyUri::Track { .. } | SpotifyUri::Episode { .. } => uri.to_uri().ok(),
        _ => None,
    }
}
async fn send_native(frame: Vec<u8>) -> io::Result<()> {
    tokio::time::timeout(Duration::from_millis(500), async {
        let mut socket = TcpStream::connect(ALICE).await?;
        socket.write_all(&frame).await?;
        socket.shutdown().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "native command timeout"))?
}
pub async fn watch(sender: mpsc::Sender<Event>) {
    let mut seen: VecDeque<String> = VecDeque::new();
    loop {
        let result: io::Result<()> = async {
            let mut socket = TcpStream::connect(ALICE).await?;
            eprintln!("voice: connected to native Alice events");
            loop {
                let size = socket.read_u32_le().await? as usize;
                if size == 0 || size > MAX_FRAME {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "native frame length",
                    ));
                }
                let mut data = vec![0; size];
                socket.read_exact(&mut data).await?;
                if let Some((request, text)) = recognized(&data) {
                    if seen.contains(&request) {
                        continue;
                    }
                    seen.push_back(request);
                    if seen.len() > 64 {
                        seen.pop_front();
                    }
                    if let Some(intent) = intent(&text) {
                        let _ = sender.try_send(Event {
                            intent,
                            received: Instant::now(),
                            from_voice: true,
                        });
                    }
                }
            }
        }
        .await;
        if sender.is_closed() {
            return;
        }
        if result.is_err() {
            eprintln!("voice: native connection lost; retrying");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
pub fn context(query: &str) -> String {
    let mut value = String::from("spotify:search:");
    for byte in query.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                value.push(char::from(byte))
            }
            b' ' => value.push('+'),
            byte => {
                use std::fmt::Write;
                let _ = write!(value, "%{byte:02X}");
            }
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recognizes_music_and_leaves_household_commands_alone() {
        assert_eq!(
            intent("Включи radiohead"),
            Some(Intent::Play("radiohead".into(), None))
        );
        assert_eq!(
            intent("Включи песню Всё идёт по плану"),
            Some(Intent::Play("все идет по плану".into(), None))
        );
        assert_eq!(intent("Останови музыку"), Some(Intent::Pause));
        assert_eq!(intent("Включи музыку"), Some(Intent::Resume));
        assert_eq!(intent("Продолжи музыку"), Some(Intent::Resume));
        assert_eq!(intent("следующий трек"), Some(Intent::Next));
        for s in [
            "включи свет",
            "включи свет в спальне",
            "поставь таймер на пять минут",
            "включи телевизор",
            "включи блютуз",
            "какая погода",
        ] {
            assert_eq!(intent(s), None, "{s}");
        }
        for (phrase, query, source) in [
            (
                "Включи Radiohead в Навидроме",
                "radiohead",
                Source::Navidrome,
            ),
            (
                "включи песню No Surprises из навидрома",
                "no surprises",
                Source::Navidrome,
            ),
            (
                "Включи альбом OK Computer на Navidrome",
                "ok computer",
                Source::Navidrome,
            ),
            ("поставь Земфира в спотифае", "земфира", Source::Spotify),
            ("запусти queen через Spotify", "queen", Source::Spotify),
        ] {
            assert_eq!(
                intent(phrase),
                Some(Intent::Play(query.into(), Some(source))),
                "{phrase}"
            );
        }
        // The source words alone, or a household device, are not music requests.
        assert_eq!(
            intent("включи в навидроме"),
            Some(Intent::Play("в навидроме".into(), None))
        );
        assert_eq!(intent("включи свет в навидроме"), None);
        assert_eq!(context("never gonna"), "spotify:search:never+gonna");
        assert_eq!(context("a/b?c"), "spotify:search:a%2Fb%3Fc");
    }
    #[test]
    fn persisted_track_accepts_only_playable_spotify_items() {
        let track = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";
        assert_eq!(playable_track(track).as_deref(), Some(track));
        assert_eq!(
            playable_track(&format!("{track}\n")).as_deref(),
            Some(track)
        );
        for value in [
            "",
            "spotify:search:radiohead",
            "spotify:album:4uLU6hMCjMI75M1A2tKUQC",
            "spotify:track:invalid",
            "/etc/passwd",
        ] {
            assert_eq!(playable_track(value), None);
        }
    }
    #[test]
    fn final_voice_state_only_and_bounded_wire_parser() {
        let id = b"12345678-1234-1234-1234-123456789012";
        let text = "Включи radiohead".as_bytes();
        let mut asr = Vec::new();
        put_bytes(&mut asr, 1, text);
        put_number(&mut asr, 2, 1);
        put_number(&mut asr, 4, 1);
        let mut state = Vec::new();
        put_number(&mut state, 1, 3);
        put_bytes(&mut state, 2, text);
        put_bytes(&mut state, 7, id);
        put_bytes(&mut state, 15, &asr);
        let mut msg = Vec::new();
        put_bytes(&mut msg, 252, &state);
        put_number(&mut msg, 253, 3);
        assert_eq!(
            recognized(&msg),
            Some((
                String::from_utf8(id.to_vec()).unwrap(),
                "Включи radiohead".into()
            ))
        );
        for n in 0..msg.len() {
            assert_eq!(recognized(&msg[..n]), None);
        }
        for malformed in [
            vec![0],
            vec![0xff; 12],
            vec![0x12, 0xff, 0x7f],
            vec![0xe8, 0x0f, 210, 1],
        ] {
            assert_eq!(recognized(&malformed), None);
        }
        state[1] = 1;
        let mut partial = Vec::new();
        put_bytes(&mut partial, 252, &state);
        put_number(&mut partial, 253, 3);
        assert_eq!(recognized(&partial), None);
        assert_eq!(
            &rpc("AliceCapability", 8, &[8, 3])[4..],
            &[
                0xe2, 0x0f, 0x15, 0x3a, 0x0f, b'A', b'l', b'i', b'c', b'e', b'C', b'a', b'p', b'a',
                b'b', b'i', b'l', b'i', b't', b'y', 0x42, 2, 8, 3, 0xe8, 0x0f, 0xd2, 1
            ]
        );
    }
}
