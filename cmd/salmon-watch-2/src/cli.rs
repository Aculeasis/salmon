use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::logging::LogLevel;

#[derive(Debug, Default, PartialEq)]
pub struct Options {
    pub start_hidden: bool,
    pub scale: Option<f32>,
    pub log_level: Option<LogLevel>,
    pub config: Option<PathBuf>,
    pub help: bool,
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Options> {
    let mut options = Options::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--start-hidden") => options.start_hidden = true,
            Some("--config") => {
                options.config = Some(PathBuf::from(
                    args.next().context("--config requires a filename")?,
                ));
            }
            Some(arg) if arg.starts_with("--config=") => {
                options.config = Some(PathBuf::from(&arg[9..]));
            }
            Some("--scale") => {
                let value = args.next().context("--scale requires a numeric factor")?;
                options.scale = Some(parse_scale(&value)?);
            }
            Some(arg) if arg.starts_with("--scale=") => {
                options.scale = Some(parse_scale(OsString::from(&arg[8..]).as_ref())?);
            }
            Some("--log-level") => {
                let value = args.next().context("--log-level requires a level")?;
                options.log_level = Some(parse_log_level(&value)?);
            }
            Some(arg) if arg.starts_with("--log-level=") => {
                options.log_level = Some(parse_log_level(OsString::from(&arg[12..]).as_ref())?);
            }
            Some("-h" | "--help") => options.help = true,
            Some(arg) => anyhow::bail!("unknown argument {arg:?}"),
            None => anyhow::bail!("arguments must be valid UTF-8"),
        }
    }
    Ok(options)
}

fn parse_log_level(value: &std::ffi::OsStr) -> Result<LogLevel> {
    value
        .to_str()
        .context("--log-level must be valid UTF-8")?
        .parse()
}

fn parse_scale(value: &std::ffi::OsStr) -> Result<f32> {
    let value = value.to_str().context("--scale must be valid UTF-8")?;
    let scale: f32 = value
        .parse()
        .with_context(|| format!("invalid scale factor {value:?}"))?;
    anyhow::ensure!(
        scale.is_finite() && scale > 0.0,
        "scale factor must be greater than zero"
    );
    Ok(scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_empty() {
        assert_eq!(parse([]).unwrap(), Options::default());
    }

    #[test]
    fn parses_config_start_hidden_and_scale() {
        let options = parse([
            "--config=config.yml".into(),
            "--start-hidden".into(),
            "--scale=1.25".into(),
            "--log-level=debug".into(),
        ])
        .unwrap();
        assert_eq!(options.config, Some(PathBuf::from("config.yml")));
        assert!(options.start_hidden);
        assert_eq!(options.scale, Some(1.25));
        assert_eq!(options.log_level, Some(LogLevel::Debug));
    }

    #[test]
    fn rejects_invalid_and_missing_values() {
        for value in ["0", "-1", "NaN", "inf", "wat"] {
            assert!(parse(["--scale".into(), value.into()]).is_err());
        }
        assert!(parse(["--scale".into()]).is_err());
        assert!(parse(["--config".into()]).is_err());
        assert!(parse(["--log-level".into()]).is_err());
        assert!(parse(["--log-level=verbose".into()]).is_err());
        assert!(parse(["--wat".into()]).is_err());
    }
}
