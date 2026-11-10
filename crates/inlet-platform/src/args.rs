//! Command-line splitting shared by every argv helper in this crate.
//!
//! The registered link handlers start Inlet as `inlet --url -- <link>`. Everything
//! after the first `--` is data, never an option: a link that breaks out of its
//! quotes (`inlet://x" --autostart "`) must not be able to set flags. The app's own
//! command-line parsing should use [`split_args`] too.

/// Splits `args` (without the program name) at the first `--`: options before it,
/// positional values after it (the `--` itself is in neither). Without a `--`,
/// everything is in the first part.
pub fn split_args<S: AsRef<str>>(args: &[S]) -> (&[S], &[S]) {
    match args.iter().position(|a| a.as_ref() == "--") {
        Some(sep) => (&args[..sep], &args[sep + 1..]),
        None => (args, &[]),
    }
}

/// Whether `flag` appears among the options, before any `--`.
pub fn has_flag<S: AsRef<str>>(args: &[S], flag: &str) -> bool {
    split_args(args).0.iter().any(|a| a.as_ref() == flag)
}

/// The value after `flag` among the options (`--exit-after 10`), if any.
pub fn flag_value<'a, S: AsRef<str>>(args: &'a [S], flag: &str) -> Option<&'a str> {
    let (options, _) = split_args(args);
    let at = options.iter().position(|a| a.as_ref() == flag)?;
    options.get(at + 1).map(AsRef::as_ref)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_the_first_separator() {
        let args = ["--url", "--", "inlet://x", "--", "--autostart"];
        let (options, positional) = split_args(&args);
        assert_eq!(options, ["--url"]);
        assert_eq!(positional, ["inlet://x", "--", "--autostart"]);
        let none: [&str; 0] = [];
        assert_eq!(split_args(&none), (&none[..], &none[..]));
        assert_eq!(split_args(&["--a"]).1.len(), 0);
    }

    #[test]
    fn flags_after_the_separator_do_not_count() {
        let args = [
            "--url",
            "--",
            "inlet://x\"",
            "--autostart",
            "--exit-after",
            "1",
        ];
        assert!(has_flag(&args, "--url"));
        assert!(!has_flag(&args, "--autostart"));
        assert_eq!(flag_value(&args, "--exit-after"), None);
        assert_eq!(
            flag_value(&["--exit-after", "5"], "--exit-after"),
            Some("5")
        );
        assert_eq!(flag_value(&["--exit-after"], "--exit-after"), None);
    }
}
