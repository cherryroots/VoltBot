//! Moving chat to a backup provider while the main one can't answer.
//!
//! `provider` under `[ai]` names the main provider and `fallback` the backup, so it works
//! either way round (Claude falling back to OpenAI, or OpenAI to Claude). When the main
//! provider fails because it's out of credit, chat uses the backup and tries the main one
//! again a day later; when it fails because it's down (server errors that kept happening
//! through the retries, or no connection), it tries again an hour later. The first answer that works on the main
//! provider switches back. Both switches are logged as warnings, so they show in the log
//! channel.
//!
//! [`Watched`] wraps the main provider and reports every answer to the shared [`State`].
//! Callers pick a provider once per answer with [`super::Ai::chat`], so one answer never
//! changes provider halfway.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::{ChatEvent, ChatProvider, ChatRequest, GeneratedFile, display_name};

/// Why the main provider can't answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outage {
    OutOfCredit,
    Down,
}

impl Outage {
    /// How long to stay on the backup before trying the main provider again.
    pub fn retry_after(self) -> TimeDelta {
        match self {
            Outage::OutOfCredit => TimeDelta::days(1),
            Outage::Down => TimeDelta::hours(1),
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Outage::OutOfCredit => "is out of credit",
            Outage::Down => "is down",
        }
    }
}

/// An error answer from a provider's API, kept whole so [`outage`] can read it.
#[derive(Debug)]
pub struct ApiError {
    pub provider: &'static str,
    pub status: reqwest::StatusCode,
    pub message: String,
}

impl ApiError {
    /// Whether this error means the provider can't answer anyone right now. Busy answers
    /// (429, 500s, 529) were already retried a couple of times before they got here.
    pub fn outage(&self) -> Option<Outage> {
        // Claude says "Your credit balance is too low"; OpenAI says "You exceeded your
        // current quota, please check your plan and billing details".
        let message = self.message.to_lowercase();
        if ["credit balance", "billing", "quota"]
            .iter()
            .any(|words| message.contains(words))
        {
            return Some(Outage::OutOfCredit);
        }
        match self.status.as_u16() {
            500 | 502 | 503 | 504 | 529 => Some(Outage::Down),
            _ => None,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = display_name(self.provider);
        write!(f, "{name} answered {}: {}", self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

/// Whether `err` means the provider can't answer anyone right now, rather than that this
/// one request was wrong.
pub fn outage(err: &anyhow::Error) -> Option<Outage> {
    for cause in err.chain() {
        if let Some(api) = cause.downcast_ref::<ApiError>() {
            return api.outage();
        }
        if let Some(http) = cause.downcast_ref::<reqwest::Error>()
            && (http.is_connect() || http.is_timeout())
        {
            return Some(Outage::Down);
        }
    }
    None
}

/// The main provider is out: why, since when, and when to try it again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Switched {
    pub outage: Outage,
    pub since: DateTime<Utc>,
    pub retry_at: DateTime<Utc>,
}

/// Whether chat has moved to the backup, shared by everything that answers.
pub struct State {
    main: &'static str,
    backup: &'static str,
    switched: Mutex<Option<Switched>>,
}

impl State {
    pub fn new(main: &'static str, backup: &'static str) -> State {
        State {
            main,
            backup,
            switched: Mutex::new(None),
        }
    }

    pub fn switched(&self) -> Option<Switched> {
        *self.switched.lock().unwrap()
    }

    /// Whether a new answer should go to the backup: switched, and not yet time to retry.
    pub fn use_backup(&self, now: DateTime<Utc>) -> bool {
        self.switched().is_some_and(|s| now < s.retry_at)
    }

    /// The main provider failed. An outage moves chat to the backup (or keeps it there
    /// after a retry that failed); any other error changes nothing.
    pub fn failed(&self, err: &anyhow::Error, now: DateTime<Utc>) {
        let Some(outage) = outage(err) else {
            return;
        };
        let retry_at = now + outage.retry_after();
        let mut switched = self.switched.lock().unwrap();
        let (main, backup) = (display_name(self.main), display_name(self.backup));
        match *switched {
            None => warn!(
                "{main} {} ({err:#}); chat uses {backup} until {retry_at}",
                outage.describe()
            ),
            Some(_) => info!(
                "{main} still {}; trying again at {retry_at}",
                outage.describe()
            ),
        }
        let since = switched.map_or(now, |s| s.since);
        *switched = Some(Switched {
            outage,
            since,
            retry_at,
        });
    }

    /// The main provider answered, so chat is back on it.
    pub fn worked(&self) {
        if self.switched.lock().unwrap().take().is_some() {
            let (main, backup) = (display_name(self.main), display_name(self.backup));
            warn!("{main} works again; chat is back on it instead of {backup}");
        }
    }
}

/// The main provider, reporting how each answer went to [`State`].
pub struct Watched {
    pub inner: Arc<dyn ChatProvider>,
    pub state: Arc<State>,
}

#[async_trait]
impl ChatProvider for Watched {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    fn sends_full_history(&self) -> bool {
        self.inner.sends_full_history()
    }

    async fn stream(
        &self,
        request: ChatRequest,
    ) -> anyhow::Result<mpsc::Receiver<anyhow::Result<ChatEvent>>> {
        let mut events = match self.inner.stream(request).await {
            Ok(events) => events,
            Err(err) => {
                self.state.failed(&err, Utc::now());
                return Err(err);
            }
        };
        // Pass the events on, noting how the answer ends. When the caller stops listening,
        // the inner receiver is dropped too, which stops the request.
        let (sender, receiver) = mpsc::channel(64);
        let state = self.state.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                match &event {
                    Ok(ChatEvent::Done(_)) => state.worked(),
                    Err(err) => state.failed(err, Utc::now()),
                    Ok(_) => {}
                }
                if sender.send(event).await.is_err() {
                    break;
                }
            }
        });
        Ok(receiver)
    }

    async fn download_file(&self, file: &GeneratedFile) -> anyhow::Result<Vec<u8>> {
        self.inner.download_file(file).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_error(status: u16, message: &str) -> anyhow::Error {
        ApiError {
            provider: "claude",
            status: reqwest::StatusCode::from_u16(status).unwrap(),
            message: message.into(),
        }
        .into()
    }

    #[test]
    fn tells_outages_from_bad_requests() {
        let credit = api_error(
            400,
            "Your credit balance is too low to access the Anthropic API.",
        );
        assert_eq!(outage(&credit), Some(Outage::OutOfCredit));
        assert_eq!(outage(&api_error(529, "Overloaded")), Some(Outage::Down));
        assert_eq!(
            outage(&api_error(500, "Internal error")),
            Some(Outage::Down)
        );
        assert_eq!(outage(&api_error(400, "messages: bad")), None);
        assert_eq!(outage(&api_error(401, "invalid x-api-key")), None);
        assert_eq!(outage(&api_error(429, "rate limited")), None);
        // Wrapped in context, it's still found.
        assert_eq!(
            outage(&api_error(503, "down").context("answering")),
            Some(Outage::Down)
        );
        assert_eq!(outage(&anyhow::anyhow!("something else")), None);
        // OpenAI out of credit.
        let quota = ApiError {
            provider: "openai",
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            message: "You exceeded your current quota, please check your plan and billing \
                      details."
                .into(),
        };
        assert_eq!(quota.outage(), Some(Outage::OutOfCredit));
    }

    #[test]
    fn switches_and_comes_back() {
        let state = State::new("claude", "openai");
        let now = Utc::now();
        assert!(!state.use_backup(now));

        // A bad request doesn't switch.
        state.failed(&api_error(400, "messages: bad"), now);
        assert!(!state.use_backup(now));

        // Out of credit: the backup for a day.
        state.failed(&api_error(400, "credit balance is too low"), now);
        assert!(state.use_backup(now + TimeDelta::hours(23)));
        assert!(!state.use_backup(now + TimeDelta::hours(24)));

        // The retry a day later fails because Claude is down now: another hour, and the
        // switch keeps its start.
        let later = now + TimeDelta::hours(24);
        state.failed(&api_error(529, "Overloaded"), later);
        let switched = state.switched().unwrap();
        assert_eq!(switched.outage, Outage::Down);
        assert_eq!(switched.since, now);
        assert_eq!(switched.retry_at, later + TimeDelta::hours(1));

        state.worked();
        assert_eq!(state.switched(), None);
    }
}
