use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    fn filter(self) -> log::LevelFilter {
        match self {
            Self::Trace => log::LevelFilter::Trace,
            Self::Debug => log::LevelFilter::Debug,
            Self::Info => log::LevelFilter::Info,
            Self::Warn => log::LevelFilter::Warn,
            Self::Error => log::LevelFilter::Error,
        }
    }
}

impl std::str::FromStr for LogLevel {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "trace" => Ok(Self::Trace),
            "debug" => Ok(Self::Debug),
            "info" => Ok(Self::Info),
            "warn" | "warning" => Ok(Self::Warn),
            "error" => Ok(Self::Error),
            _ => Err(anyhow!(
                "invalid log level {value:?}; expected trace, debug, info, warn, or error"
            )),
        }
    }
}

struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        (metadata.target() == "salmon_watch" || metadata.target().starts_with("salmon_watch::"))
            && metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            let component = component_name(record.target());
            if component.is_empty() {
                eprintln!(
                    "{timestamp} {} salmon-watch: {}",
                    record.level(),
                    record.args()
                );
            } else {
                eprintln!(
                    "{timestamp} {} salmon-watch[{component}]: {}",
                    record.level(),
                    record.args()
                );
            }
        }
    }

    fn flush(&self) {}
}

fn component_name(target: &str) -> &str {
    target.strip_prefix("salmon_watch::").unwrap_or_default()
}

static LOGGER: StderrLogger = StderrLogger;
static INITIALIZED: AtomicBool = AtomicBool::new(false);

pub fn init(level: LogLevel) -> Result<()> {
    log::set_logger(&LOGGER).map_err(|_| anyhow!("failed to initialize logger"))?;
    log::set_max_level(level.filter());
    INITIALIZED.store(true, Ordering::Release);
    Ok(())
}

pub fn is_initialized() -> bool {
    INITIALIZED.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_log_levels_case_insensitively() {
        assert_eq!("trace".parse::<LogLevel>().unwrap(), LogLevel::Trace);
        assert_eq!("DEBUG".parse::<LogLevel>().unwrap(), LogLevel::Debug);
        assert_eq!("info".parse::<LogLevel>().unwrap(), LogLevel::Info);
        assert_eq!("warn".parse::<LogLevel>().unwrap(), LogLevel::Warn);
        assert_eq!("warning".parse::<LogLevel>().unwrap(), LogLevel::Warn);
        assert_eq!("error".parse::<LogLevel>().unwrap(), LogLevel::Error);
        assert!("quiet".parse::<LogLevel>().is_err());
    }

    #[test]
    fn derives_readable_component_names_from_log_targets() {
        assert_eq!(component_name("salmon_watch"), "");
        assert_eq!(component_name("salmon_watch::runtime"), "runtime");
        assert_eq!(
            component_name("salmon_watch::network::client"),
            "network::client"
        );
        assert_eq!(component_name("some_dependency"), "");
    }
}
