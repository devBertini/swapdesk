//! The command line of `Swapdesk.app`'s executable. Unknown arguments are ignored (macOS adds its own, such as `-psn_0_1`
//! or `-NSDocumentRevisionsDebugMode`).
//!
//! - `--mock <scenario>`: the scripted `MockBackend`, no service (looking at the screens).
//! - `--scratch`: development. The secrets go to a throwaway keychain file and folder in the temp directory, the
//!   ports are chosen by the system and nothing of the owner's real Swapdesk identity is touched; both are removed at exit.
//! - `--selftest <file>`: development. Implies `--scratch`. Once the main window is up the app writes what it sees (the
//!   windows, the service) to `<file>` and quits by itself.
//! - `--help`, `-h`: the usage text on stdout.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Options {
    pub mock: Option<String>,
    pub scratch: bool,
    pub selftest: Option<PathBuf>,
    pub help: bool,
}

impl Options {
    /// Scratch secrets: asked for, or implied by the self-test.
    pub fn use_scratch(&self) -> bool {
        self.scratch || self.selftest.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    MissingValue(&'static str),
}

impl core::fmt::Display for CliError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CliError::MissingValue(flag) => write!(f, "{flag} needs a value."),
        }
    }
}

pub const USAGE: &str = "Swapdesk [--scratch] [--selftest <file>] | Swapdesk --mock <scenario>";

/// Parses the arguments after the program name.
pub fn parse(args: &[String]) -> Result<Options, CliError> {
    let mut o = Options::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mock" => {
                o.mock = Some(args.get(i + 1).ok_or(CliError::MissingValue("--mock"))?.clone());
                i += 1;
            }
            "--selftest" => {
                o.selftest = Some(PathBuf::from(args.get(i + 1).ok_or(CliError::MissingValue("--selftest"))?));
                i += 1;
            }
            "--scratch" => o.scratch = true,
            "--help" | "-h" => o.help = true,
            _ => {}
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
        let o = parse(&[]).unwrap();
        assert_eq!(o, Options::default());
        assert!(!o.use_scratch());
    }

    #[test]
    fn the_selftest_implies_scratch_and_macos_arguments_are_ignored() {
        let o = parse(&args(&["-psn_0_12345", "--selftest", "/tmp/x.txt", "-NSDocumentRevisionsDebugMode", "YES"]))
            .unwrap();
        assert_eq!(o.selftest, Some(PathBuf::from("/tmp/x.txt")));
        assert!(o.use_scratch() && !o.scratch);
    }

    #[test]
    fn mock_scratch_and_help_parse_in_any_order() {
        let o = parse(&args(&["--scratch", "--help", "--mock", "visualizador"])).unwrap();
        assert_eq!(o, Options { mock: Some("visualizador".into()), scratch: true, selftest: None, help: true });
    }

    #[test]
    fn a_missing_value_is_an_error() {
        assert_eq!(parse(&args(&["--mock"])), Err(CliError::MissingValue("--mock")));
        assert_eq!(parse(&args(&["--selftest"])), Err(CliError::MissingValue("--selftest")));
    }
}
