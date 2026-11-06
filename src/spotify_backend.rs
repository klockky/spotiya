use librespot::connect::{LoadRequest, LoadRequestOptions, Spirc};
use std::{fs, io, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Soloist,
    Librespot,
}
impl Backend {
    pub fn parse(value: &str) -> io::Result<Self> {
        match value.trim() {
            "soloist" => Ok(Self::Soloist),
            "librespot" => Ok(Self::Librespot),
            _ => Err(io::Error::other(
                "Spotify backend must be soloist or librespot",
            )),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Soloist => "soloist",
            Self::Librespot => "librespot",
        }
    }
    pub fn read(state: &Path) -> io::Result<Self> {
        match fs::read_to_string(state.join("spotify-backend")) {
            Ok(s) => Self::parse(&s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::Soloist),
            Err(e) => Err(e),
        }
    }
    pub fn save(self, state: &Path) -> io::Result<()> {
        fs::create_dir_all(state)?;
        let file = state.join("spotify-backend");
        let temp = file.with_extension("new");
        fs::write(&temp, format!("{}\n", self.name()))?;
        fs::rename(temp, file)
    }
}

pub enum Handle {
    Librespot(Spirc),
    Soloist(crate::soloist::Handle),
}
macro_rules! transport {
    ($name:ident, $command:literal) => {
        pub fn $name(&self) -> io::Result<()> {
            match self {
                Self::Librespot(s) => s.$name().map_err(io::Error::other),
                Self::Soloist(s) => s.command($command),
            }
        }
    };
}
impl Handle {
    transport!(play, "play");
    transport!(pause, "pause");
    transport!(next, "skip_next");
    transport!(prev, "skip_prev");
    transport!(activate, "activate");
    pub fn play_pause(&self) -> io::Result<()> {
        match self {
            Self::Librespot(s) => s.play_pause().map_err(io::Error::other),
            Self::Soloist(s) => s.toggle(),
        }
    }
    pub fn disconnect(&self, pause: bool) -> io::Result<()> {
        match self {
            Self::Librespot(s) => s.disconnect(pause).map_err(io::Error::other),
            Self::Soloist(s) => s.release(),
        }
    }
    pub fn shutdown(&self) -> io::Result<()> {
        match self {
            Self::Librespot(s) => s.shutdown().map_err(io::Error::other),
            Self::Soloist(s) => s.shutdown(),
        }
    }
    pub fn set_volume(&self, volume: u16) -> io::Result<()> {
        match self {
            Self::Librespot(s) => s.set_volume(volume).map_err(io::Error::other),
            Self::Soloist(s) => s.volume(crate::percent(volume)),
        }
    }
    pub fn load_uri(&self, uri: String) -> io::Result<()> {
        match self {
            Self::Librespot(s) => s
                .load(LoadRequest::from_context_uri(
                    uri,
                    LoadRequestOptions {
                        start_playing: true,
                        ..Default::default()
                    },
                ))
                .map_err(io::Error::other),
            Self::Soloist(s) => s.play_uri(uri),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_selection_uses_soloist_and_unknown_selection_fails_closed() {
        let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        assert_eq!(Backend::read(&dir).unwrap(), Backend::Soloist);
        Backend::Librespot.save(&dir).unwrap();
        assert_eq!(Backend::read(&dir).unwrap(), Backend::Librespot);
        fs::write(dir.join("spotify-backend"), "typo").unwrap();
        assert!(Backend::read(&dir).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
