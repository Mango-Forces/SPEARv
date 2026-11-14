//! Configuration readers shared by the binder and the extensions.
//!
//! A malformed value is a configuration error: [`number`] and [`flag`] panic
//! instead of silently falling back, because a typo in a reader count or a
//! science flag is not a working configuration. An unset variable still means
//! the documented default.

use std::fmt::Display;
use std::str::FromStr;

/// `name` as a number, or `default` when it is unset.
///
/// # Panics
/// When `name` is set but does not parse, or is not valid UTF-8.
pub fn number<T>(name: &str, default: T) -> T
where
    T: FromStr,
    T::Err: Display,
{
    match number_opt(name) {
        Ok(Some(value)) => value,
        Ok(None) => default,
        Err(message) => panic!("{message}"),
    }
}

/// `name` as a number, `Ok(None)` when it is unset.
pub fn number_opt<T>(name: &str) -> std::result::Result<Option<T>, String>
where
    T: FromStr,
    T::Err: Display,
{
    match std::env::var(name) {
        Ok(text) => parse(name, &text).map(Some),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid UTF-8")),
    }
}

/// A boolean in the vocabulary the existing flags accept: `on`, `1`, `true`
/// and `yes` are true; `off`, `0` and `false` are false. Unset is false.
///
/// # Panics
/// When `name` is set to any other value, or is not valid UTF-8.
pub fn flag(name: &str) -> bool {
    match std::env::var(name) {
        Ok(text) => match text.trim() {
            "on" | "1" | "true" | "yes" => true,
            "off" | "0" | "false" => false,
            other => panic!("{name}={other} is not a boolean (on/off)"),
        },
        Err(std::env::VarError::NotPresent) => false,
        Err(std::env::VarError::NotUnicode(_)) => panic!("{name} is not valid UTF-8"),
    }
}

fn parse<T>(name: &str, text: &str) -> std::result::Result<T, String>
where
    T: FromStr,
    T::Err: Display,
{
    text.trim()
        .parse()
        .map_err(|_| format!("{name}={text} is not a number"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_number_uses_the_default() {
        let name = "SPEARV_TEST_ENV_NUMBER_UNSET";
        std::env::remove_var(name);
        assert_eq!(number(name, 24_u64), 24);
        assert_eq!(number_opt::<u64>(name).unwrap(), None);
    }

    #[test]
    fn a_malformed_number_names_the_variable() {
        let name = "SPEARV_TEST_ENV_NUMBER_BAD";
        std::env::set_var(name, "twenty-four");
        assert!(number_opt::<u64>(name).unwrap_err().contains(name));
        std::env::remove_var(name);
    }

    #[test]
    fn flags_accept_the_existing_vocabulary() {
        let name = "SPEARV_TEST_ENV_FLAG";
        for value in ["on", "1", "true", "yes", " off "] {
            std::env::set_var(name, value);
            assert_eq!(
                flag(name),
                !matches!(value.trim(), "off" | "0" | "false"),
                "{value}"
            );
        }
        std::env::remove_var(name);
        assert!(!flag(name));
    }
}
