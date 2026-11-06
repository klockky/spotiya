use std::env;

fn builder(filter: Option<&str>) -> env_logger::Builder {
    let mut builder = env_logger::Builder::new();
    builder.parse_filters(filter.unwrap_or("warn,librespot_discovery::server=info"));
    builder.format(|buffer, record| {
        use std::io::Write;
        crate::recovery::note(record);
        writeln!(
            buffer,
            "[{} {}] {}",
            record.level(),
            record.target(),
            record.args()
        )
    });
    builder
}
pub fn init() -> Result<(), log::SetLoggerError> {
    builder(env::var("RUST_LOG").ok().as_deref()).try_init()
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log;
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn librespot_errors_are_visible_and_diagnostic_filters_work() {
        for (filter, debug) in [(None, false), (Some("warn,librespot_playback=debug"), true)] {
            let output = Arc::new(Mutex::new(Vec::new()));
            let logger = builder(filter)
                .target(env_logger::Target::Pipe(Box::new(Capture(output.clone()))))
                .build();
            for level in [log::Level::Warn, log::Level::Error, log::Level::Debug] {
                logger.log(
                    &log::Record::builder()
                        .level(level)
                        .target("librespot_playback::player")
                        .args(format_args!("diagnostic {level}"))
                        .build(),
                );
            }
            let bytes = output.lock().unwrap();
            let text = std::str::from_utf8(&bytes).unwrap();
            assert!(text.contains("librespot_playback::player"));
            assert!(text.contains("diagnostic WARN"));
            assert!(text.contains("diagnostic ERROR"));
            assert_eq!(text.contains("diagnostic DEBUG"), debug);
        }
    }
}
