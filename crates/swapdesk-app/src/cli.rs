//! The command line of `Swapdesk.exe`.
//!
//! - `--mock <scenario>`: the scripted `MockBackend` (development builds only, cargo feature `mock`).
//! - `--service-pid <pid>`: development mode (C2.4): the pipe's server must be this process, not the service the SCM
//!   reports (`swapdesk-service --console` is no SCM service).
//! - `--instance <n>`: the pipe name gets the suffix `-<n>` (`ui-<session>-<n>`), so several development services of
//!   one user can run side by side.
//! - `--help`, `-h`, `/?`: the usage text.
//!
//! The two development flags exist only when [`DEV_FLAGS`] is on: debug builds, or any build with the cargo feature
//! `dev`. In a plain release build they are unknown arguments, because `--service-pid` replaces the check of who the
//! pipe's server is. Any unknown argument is an error (the program exits with code 2).

/// Whether `--service-pid` and `--instance` are accepted (debug builds, or the cargo feature `dev`).
pub const DEV_FLAGS: bool = cfg!(any(debug_assertions, feature = "dev"));

/// The arguments this build accepts, for the message of an unknown one.
pub fn accepted() -> &'static str {
    if DEV_FLAGS {
        "--mock <scenario>, --service-pid <pid>, --instance <n>, --help"
    } else {
        "--mock <scenario>, --help"
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Options {
    pub mock: Option<String>,
    pub service_pid: Option<u32>,
    pub instance: Option<u32>,
    pub help: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    MissingValue(&'static str),
    BadNumber { flag: &'static str, value: String },
    Unknown(String),
}

impl core::fmt::Display for CliError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CliError::MissingValue(flag) => write!(f, "{flag} needs a value."),
            CliError::BadNumber { flag, value } => write!(f, "{flag} needs a number, not \"{value}\"."),
            CliError::Unknown(arg) => write!(f, "Unknown argument \"{arg}\". Accepted: {}.", accepted()),
        }
    }
}

fn number(flag: &'static str, value: Option<&String>) -> Result<u32, CliError> {
    let v = value.ok_or(CliError::MissingValue(flag))?;
    v.parse().map_err(|_| CliError::BadNumber { flag, value: v.clone() })
}

/// Parses the arguments after the program name.
pub fn parse(args: &[String]) -> Result<Options, CliError> {
    parse_with(args, DEV_FLAGS)
}

fn parse_with(args: &[String], dev: bool) -> Result<Options, CliError> {
    let mut o = Options::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mock" => {
                o.mock = Some(args.get(i + 1).ok_or(CliError::MissingValue("--mock"))?.clone());
                i += 1;
            }
            "--service-pid" if dev => {
                o.service_pid = Some(number("--service-pid", args.get(i + 1))?);
                i += 1;
            }
            "--instance" if dev => {
                o.instance = Some(number("--instance", args.get(i + 1))?);
                i += 1;
            }
            "--help" | "-h" | "/?" => o.help = true,
            other => return Err(CliError::Unknown(other.to_owned())),
        }
        i += 1;
    }
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn no_arguments_means_production() {
        assert_eq!(parse(&[]).unwrap(), Options::default());
    }

    #[test]
    fn development_flags_and_mock_parse_in_any_order() {
        let o =
            parse_with(&args(&["--instance", "3", "--service-pid", "4242", "--mock", "visualizador"]), true).unwrap();
        assert_eq!(
            o,
            Options { mock: Some("visualizador".into()), service_pid: Some(4242), instance: Some(3), help: false }
        );
    }

    #[test]
    fn a_missing_or_bad_value_is_an_error() {
        assert_eq!(parse(&args(&["--mock"])), Err(CliError::MissingValue("--mock")));
        assert_eq!(parse_with(&args(&["--service-pid"]), true), Err(CliError::MissingValue("--service-pid")));
        assert_eq!(
            parse_with(&args(&["--instance", "x"]), true),
            Err(CliError::BadNumber { flag: "--instance", value: "x".into() })
        );
    }

    #[test]
    fn help_is_accepted_and_an_unknown_argument_names_itself() {
        assert!(parse(&args(&["/?"])).unwrap().help);
        let e = parse(&args(&["--whatever"])).unwrap_err();
        assert_eq!(e, CliError::Unknown("--whatever".into()));
        assert!(e.to_string().contains("--whatever") && e.to_string().contains("--mock"));
    }

    #[test]
    fn development_flags_are_unknown_when_the_gate_is_off() {
        for flag in ["--service-pid", "--instance"] {
            let e = parse_with(&args(&[flag, "1"]), false).unwrap_err();
            assert_eq!(e, CliError::Unknown(flag.into()));
        }
        assert!(parse_with(&args(&["--service-pid", "1"]), true).is_ok());
    }

    #[test]
    fn the_gate_follows_the_build() {
        assert_eq!(DEV_FLAGS, cfg!(debug_assertions) || cfg!(feature = "dev"));
        assert_eq!(parse(&args(&["--instance", "2"])).is_ok(), DEV_FLAGS);
    }
}
