#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Spotify,
    Navidrome,
}
impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Self::Spotify => "spotify",
            Self::Navidrome => "navidrome",
        }
    }
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "spotify" => Some(Self::Spotify),
            "navidrome" => Some(Self::Navidrome),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_round_trip_and_unknown_is_rejected() {
        for source in [Source::Spotify, Source::Navidrome] {
            assert_eq!(Source::parse(source.name()), Some(source));
        }
        assert_eq!(Source::parse(" NAVIDROME\n"), Some(Source::Navidrome));
        assert_eq!(Source::parse("tidal"), None);
    }
}
