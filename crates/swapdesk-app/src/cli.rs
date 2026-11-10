//! The command line of `Swapdesk.exe`.
//!
//! - `--mock <scenario>`: the scripted `MockBackend` (development builds only, cargo feature `mock`).
//! - `--service-pid <pid>`: development mode (C2.4): the pipe's server must be this process, not the service the SCM
//!   reports (`swapdesk-service --console` is no SCM service).
//! - `--instance <n>`: the pipe name gets the suffix `-<n>` (`ui-<session>-<n>`), so several development services of
//!   one user can run side by side.
//! - `--elevated <ticket>`: this process is only the elevated instance that confirms one action with the service
//!   (ELEVATION.md 3.5). The ticket is exactly 32 lowercase hex digits; the flag is given once and never with `--mock`.
//!   It is accepted in every build, and `--service-pid` and `--instance` pass through to it where they exist.
//! - `--help`, `-h`, `/?`: the usage text.
//!
//! The two development flags exist only when [`DEV_FLAGS`] is on: debug builds, or any build with the cargo feature
//! `dev`. In a plain release build they are unknown arguments, because `--service-pid` replaces the check of who the
//! pipe's server is. Any unknown argument is an error (the program exits with code 2).

use swapdesk_ipc::ElevationTicket;

/// Whether `--service-pid` and `--instance` are accepted (debug builds, or the cargo feature `dev`).
pub const DEV_FLAGS: bool = cfg!(any(debug_assertions, feature = "dev"));

/// The arguments this build accepts, for the message of an unknown one.
pub fn accepted() -> &'static str {
    if DEV_FLAGS {
        "--mock <scenario>, --elevated <ticket>, --service-pid <pid>, --instance <n>, --help"
    } else {
        "--mock <scenario>, --elevated <ticket>, --help"
    }
}

/// Digits of a ticket on the command line: its 16 bytes as lowercase hex.
pub const TICKET_DIGITS: usize = 32;

/// The ticket of 32 lowercase hex digits, or none for any other text (a length of 31 or 33, a capital letter, a sign).
pub fn parse_ticket(text: &str) -> Option<ElevationTicket> {
    if text.len() != TICKET_DIGITS || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(ElevationTicket(bytes))
}

/// The command-line form of a ticket: [`parse_ticket`]'s input.
pub fn ticket_hex(ticket: &ElevationTicket) -> String {
    use core::fmt::Write;
    ticket.0.iter().fold(String::with_capacity(TICKET_DIGITS), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Options {
    pub mock: Option<String>,
    /// Run only as the elevated instance of this ticket (its `Debug` prints no byte).
    pub elevated: Option<ElevationTicket>,
    pub service_pid: Option<u32>,
    pub instance: Option<u32>,
    pub help: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    MissingValue(&'static str),
    BadNumber {
        flag: &'static str,
        value: String,
    },
    /// `--elevated` without exactly 32 lowercase hex digits. The text is not repeated: it may be a ticket.
    BadTicket,
    /// `--elevated`, given again.
    Twice(&'static str),
    /// `--elevated` and `--mock` together: the elevated instance shows no scripted interface.
    ElevatedWithMock,
    Unknown(String),
}

impl core::fmt::Display for CliError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CliError::MissingValue(flag) => write!(f, "{flag} needs a value."),
            CliError::BadNumber { flag, value } => write!(f, "{flag} needs a number, not \"{value}\"."),
            CliError::BadTicket => write!(f, "--elevated needs exactly {TICKET_DIGITS} lowercase hex digits."),
            CliError::Twice(flag) => write!(f, "{flag} is given once."),
            CliError::ElevatedWithMock => write!(f, "--elevated does not go with --mock."),
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
            "--elevated" => {
                if o.elevated.is_some() {
                    return Err(CliError::Twice("--elevated"));
                }
                let value = args.get(i + 1).ok_or(CliError::MissingValue("--elevated"))?;
                o.elevated = Some(parse_ticket(value).ok_or(CliError::BadTicket)?);
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
    if o.elevated.is_some() && o.mock.is_some() {
        return Err(CliError::ElevatedWithMock);
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
            Options {
                mock: Some("visualizador".into()),
                service_pid: Some(4242),
                instance: Some(3),
                ..Options::default()
            }
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

    const TICKET: &str = "00112233445566778899aabbccddeeff";

    #[test]
    fn elevated_takes_exactly_32_hex_digits_and_no_mock() {
        let o = parse(&args(&["--elevated", TICKET])).unwrap();
        assert_eq!(o.elevated, parse_ticket(TICKET));
        assert_eq!(ticket_hex(&o.elevated.unwrap()), TICKET);
        // Any other form is a wrong command line, and the error never repeats the text it was given.
        for bad in [
            "",
            "00112233445566778899aabbccddeef",
            "00112233445566778899aabbccddeeff0",
            "00112233445566778899AABBCCDDEEFF",
            "0011223344556677889g9aabbccddeef",
            "+0112233445566778899aabbccddeeff",
            " 00112233445566778899aabbccddeef",
            "0x112233445566778899aabbccddeeff",
        ] {
            let e = parse(&args(&["--elevated", bad])).unwrap_err();
            assert_eq!(e, CliError::BadTicket, "{bad:?}");
            assert!(bad.is_empty() || !e.to_string().contains(bad));
        }
        assert_eq!(parse(&args(&["--elevated"])), Err(CliError::MissingValue("--elevated")));
        assert_eq!(parse(&args(&["--elevated", TICKET, "--elevated", TICKET])), Err(CliError::Twice("--elevated")));
        assert_eq!(parse(&args(&["--elevated", TICKET, "--mock", "x"])), Err(CliError::ElevatedWithMock));
        assert_eq!(parse(&args(&["--mock", "x", "--elevated", TICKET])), Err(CliError::ElevatedWithMock));
        // The development flags pass through where they exist.
        let o = parse_with(&args(&["--elevated", TICKET, "--service-pid", "7", "--instance", "2"]), true).unwrap();
        assert_eq!((o.service_pid, o.instance), (Some(7), Some(2)));
        assert!(o.mock.is_none() && !o.help);
        // Not a development flag: accepted in every build.
        assert!(parse_with(&args(&["--elevated", TICKET]), false).is_ok());
        assert_eq!(
            parse_with(&args(&["--elevated", TICKET, "--instance", "2"]), false),
            Err(CliError::Unknown("--instance".into()))
        );
    }

    #[test]
    fn the_debug_of_options_prints_no_ticket_byte() {
        let o = parse(&args(&["--elevated", TICKET])).unwrap();
        let shown = format!("{o:?}");
        assert!(!shown.contains("00112233") && !shown.contains("ccddeeff"), "{shown}");
    }

    #[test]
    fn the_gate_follows_the_build() {
        assert_eq!(DEV_FLAGS, cfg!(debug_assertions) || cfg!(feature = "dev"));
        assert_eq!(parse(&args(&["--instance", "2"])).is_ok(), DEV_FLAGS);
    }
}
