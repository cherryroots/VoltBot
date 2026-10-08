//! Errors that are safe to show to users.
//!
//! Handlers return `anyhow::Error`. Most errors are internal ("database is locked") and the
//! user only sees "Something went wrong". When a handler wants the user to read the message
//! ("I couldn't find a time in that"), it returns a [`UserError`] instead, made with
//! [`user_error`]. The dispatcher checks for it with `downcast_ref`.

use std::fmt;

#[derive(Debug)]
pub struct UserError(pub String);

impl fmt::Display for UserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UserError {}

/// An error whose message is shown to the user as is.
pub fn user_error(message: impl Into<String>) -> anyhow::Error {
    UserError(message.into()).into()
}

/// What the user sees for a given error: the message of a [`UserError`], or a generic line.
pub fn user_message(err: &anyhow::Error) -> String {
    match err.downcast_ref::<UserError>() {
        Some(user_err) => user_err.0.clone(),
        None => "Something went wrong. The error was logged.".to_string(),
    }
}

pub fn is_user_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<UserError>().is_some()
}
