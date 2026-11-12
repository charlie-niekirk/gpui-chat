//! Command-line flags. Deliberately tiny (std only): a handful of developer flags for offline
//! runs, not a user-facing CLI.

use std::fmt;
use std::path::PathBuf;

/// Default seed of the synthetic source, so two runs see the same chat unless `--seed` says
/// otherwise.
pub const DEFAULT_SEED: u64 = 0x6770_7569_6368_6174;

/// Upper bound accepted for `--firehose`; beyond this a developer typo is more likely than a test.
pub const MAX_FIREHOSE_RATE: f64 = 1_000_000.0;

/// Upper bound accepted for `--replay-speed` (a typo guard; `0` means as fast as possible).
pub const MAX_REPLAY_SPEED: f64 = 1_000_000.0;

/// Upper bound for `--firehose-channels`.
pub const MAX_FIREHOSE_CHANNELS: usize = 64;

pub const USAGE: &str = "\
Usage: gpui-chat [OPTIONS]

Options:
  --firehose <msgs/sec>        Feed the app synthetic chat at a steady rate (offline)
  --seed <n>                   Seed of the synthetic chat (default: fixed, so runs repeat)
  --firehose-channels <n>      Spread the synthetic chat over n channels (default: 1)
  --replay <file>              Replay a recorded raw IRC log (one line per message, offline)
  --replay-speed <x>           Replay speed: 1 follows the log's tmi-sent-ts spacing (or a fixed
                               100 msgs/s without timestamps), 2 is twice as fast, 0 is as fast
                               as possible (default: 1)
  --hud                        Start with the debug HUD visible (toggle: View > Toggle Debug HUD)
  -h, --help                   Print this help
";

/// What was asked for on the command line.
#[derive(Debug, Clone, PartialEq)]
pub struct Cli {
    /// Synthetic chat source rate in messages per second.
    pub firehose: Option<f64>,
    pub seed: u64,
    pub firehose_channels: usize,
    /// Raw IRC log to replay.
    pub replay: Option<PathBuf>,
    /// Replay speed factor; `0` is as fast as possible.
    pub replay_speed: f64,
    /// Open with the debug HUD visible.
    pub hud: bool,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            firehose: None,
            seed: DEFAULT_SEED,
            firehose_channels: 1,
            replay: None,
            replay_speed: 1.0,
            hud: false,
        }
    }
}

/// Result of parsing: run the app, or print the help text and exit.
#[derive(Debug, PartialEq)]
pub enum Parsed {
    Run(Cli),
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError(String);

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CliError {}

fn error(message: impl Into<String>) -> CliError {
    CliError(message.into())
}

/// Parses the arguments after the program name. Both `--flag value` and `--flag=value` work.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Parsed, CliError> {
    let mut cli = Cli::default();
    let mut speed_given = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => {
                (flag.to_owned(), Some(value.to_owned()))
            }
            _ => (arg, None),
        };
        let mut value = |name: &str| {
            inline
                .clone()
                .or_else(|| args.next())
                .ok_or_else(|| error(format!("{name} needs a value")))
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--hud" => cli.hud = true,
            "--firehose" => {
                let raw = value("--firehose")?;
                let rate: f64 = raw
                    .parse()
                    .map_err(|_| error(format!("--firehose: `{raw}` is not a number")))?;
                if !rate.is_finite() || rate <= 0.0 || rate > MAX_FIREHOSE_RATE {
                    return Err(error(format!(
                        "--firehose: rate must be above 0 and at most {MAX_FIREHOSE_RATE}"
                    )));
                }
                cli.firehose = Some(rate);
            }
            "--seed" => {
                let raw = value("--seed")?;
                cli.seed = raw
                    .parse()
                    .map_err(|_| error(format!("--seed: `{raw}` is not an unsigned integer")))?;
            }
            "--firehose-channels" => {
                let raw = value("--firehose-channels")?;
                let count: usize = raw.parse().map_err(|_| {
                    error(format!("--firehose-channels: `{raw}` is not an integer"))
                })?;
                if !(1..=MAX_FIREHOSE_CHANNELS).contains(&count) {
                    return Err(error(format!(
                        "--firehose-channels: must be between 1 and {MAX_FIREHOSE_CHANNELS}"
                    )));
                }
                cli.firehose_channels = count;
            }
            "--replay" => cli.replay = Some(PathBuf::from(value("--replay")?)),
            "--replay-speed" => {
                let raw = value("--replay-speed")?;
                let speed: f64 = raw
                    .parse()
                    .map_err(|_| error(format!("--replay-speed: `{raw}` is not a number")))?;
                if !speed.is_finite() || !(0.0..=MAX_REPLAY_SPEED).contains(&speed) {
                    return Err(error(format!(
                        "--replay-speed: must be between 0 and {MAX_REPLAY_SPEED}"
                    )));
                }
                cli.replay_speed = speed;
                speed_given = true;
            }
            other => return Err(error(format!("unknown argument `{other}`"))),
        }
    }
    if cli.replay.is_some() && cli.firehose.is_some() {
        return Err(error("--replay and --firehose are two sources; pick one"));
    }
    if speed_given && cli.replay.is_none() {
        return Err(error("--replay-speed needs --replay"));
    }
    Ok(Parsed::Run(cli))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Parsed, CliError> {
        parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn no_arguments_is_a_plain_run() {
        assert_eq!(parse_args(&[]), Ok(Parsed::Run(Cli::default())));
    }

    #[test]
    fn firehose_flags_in_both_spellings() {
        let expected = Parsed::Run(Cli {
            firehose: Some(1000.0),
            seed: 7,
            firehose_channels: 3,
            replay: None,
            replay_speed: 1.0,
            hud: true,
        });
        assert_eq!(
            parse_args(&[
                "--firehose",
                "1000",
                "--seed=7",
                "--firehose-channels",
                "3",
                "--hud"
            ]),
            Ok(expected)
        );
    }

    #[test]
    fn fractional_rates_are_fine() {
        assert_eq!(
            parse_args(&["--firehose=0.5"]),
            Ok(Parsed::Run(Cli {
                firehose: Some(0.5),
                ..Cli::default()
            }))
        );
    }

    #[test]
    fn replay_flags() {
        assert_eq!(
            parse_args(&["--replay", "log.irc", "--replay-speed=0"]),
            Ok(Parsed::Run(Cli {
                replay: Some(PathBuf::from("log.irc")),
                replay_speed: 0.0,
                ..Cli::default()
            }))
        );
        assert_eq!(
            parse_args(&["--replay=a b.irc", "--replay-speed", "2.5"]),
            Ok(Parsed::Run(Cli {
                replay: Some(PathBuf::from("a b.irc")),
                replay_speed: 2.5,
                ..Cli::default()
            }))
        );
    }

    #[test]
    fn replay_conflicts_are_reported() {
        for args in [
            &["--replay", "x", "--firehose", "10"][..],
            &["--replay-speed", "2"],
            &["--replay"],
            &["--replay", "x", "--replay-speed", "-1"],
            &["--replay", "x", "--replay-speed", "fast"],
            &["--replay", "x", "--replay-speed", "1e9"],
        ] {
            assert!(parse_args(args).is_err(), "{args:?} should be rejected");
        }
    }

    #[test]
    fn help_wins() {
        assert_eq!(parse_args(&["--hud", "--help"]), Ok(Parsed::Help));
        assert_eq!(parse_args(&["-h"]), Ok(Parsed::Help));
    }

    #[test]
    fn bad_input_is_reported() {
        for args in [
            &["--firehose"][..],
            &["--firehose", "fast"],
            &["--firehose", "0"],
            &["--firehose", "-5"],
            &["--firehose", "NaN"],
            &["--firehose", "1e9"],
            &["--seed", "-1"],
            &["--firehose-channels", "0"],
            &["--firehose-channels", "1000"],
            &["--bogus"],
            &["positional"],
        ] {
            assert!(parse_args(args).is_err(), "{args:?} should be rejected");
        }
    }
}
