//! Channel files and URLs: which provider, where to, and nothing that leaks.

use std::fmt;
use std::path::Path;

use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, Tokio1Executor};
use percent_encoding::percent_decode_str;
use url::Url;

use crate::secret::Secret;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Provider {
    Discord,
    Slack,
    Generic,
    Email,
}

impl Provider {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Discord => "discord",
            Provider::Slack => "slack",
            Provider::Generic => "generic",
            Provider::Email => "email",
        }
    }
}

/// Why a channel file was not usable. Carries no part of the file's contents.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChannelError {
    #[error("the channel file could not be read")]
    Unreadable,
    #[error("the channel file is empty")]
    Empty,
    #[error("not a valid URL")]
    NotAUrl,
    #[error("unsupported scheme; use https, smtp or smtps")]
    Scheme,
    #[error("{0} is unencrypted; add insecure=true to allow it")]
    Insecure(&'static str),
    #[error("unknown format; use discord, slack or generic")]
    Format,
    #[error("format does not apply to email channels")]
    FormatOnEmail,
    #[error("email channel has no host")]
    NoHost,
    #[error("email channel needs a from address")]
    NoFrom,
    #[error("email channel needs at least one to address")]
    NoTo,
    #[error("invalid {0} address")]
    Address(&'static str),
    #[error("credentials are not valid percent-encoded UTF-8")]
    Credentials,
    #[error("TLS setup failed")]
    Tls,
}

/// One usable channel. `Debug` shows only the name and provider.
pub struct Channel {
    name: String,
    provider: Provider,
    pub(crate) target: Target,
}

pub(crate) enum Target {
    Webhook(Secret<Url>),
    Email(Box<Email>),
}

pub(crate) struct Email {
    pub transport: Secret<AsyncSmtpTransport<Tokio1Executor>>,
    pub from: Mailbox,
    pub to: Vec<Mailbox>,
}

impl Channel {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn provider(&self) -> Provider {
        self.provider
    }

    /// The sender, for email channels.
    #[must_use]
    pub fn email_from(&self) -> Option<String> {
        match &self.target {
            Target::Email(email) => Some(email.from.to_string()),
            Target::Webhook(_) => None,
        }
    }

    /// The recipients, for email channels; empty otherwise.
    #[must_use]
    pub fn email_to(&self) -> Vec<String> {
        match &self.target {
            Target::Email(email) => email.to.iter().map(ToString::to_string).collect(),
            Target::Webhook(_) => Vec::new(),
        }
    }
}

impl fmt::Debug for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Channel")
            .field("name", &self.name)
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

/// What a channel directory held, both lists sorted by channel name.
#[derive(Debug, Default)]
pub struct ChannelSet {
    pub valid: Vec<Channel>,
    pub invalid: Vec<(String, ChannelError)>,
}

/// Reads every channel file in `dir`. A missing or unreadable directory is an
/// empty set: notifications are off. Names starting with `.` (the kubelet's
/// `..data` links) and anything that is not a file are skipped.
#[must_use]
pub fn load_dir(dir: &Path) -> ChannelSet {
    let mut set = ChannelSet::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return set;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if !std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let parsed = std::fs::read_to_string(&path)
            .map_err(|_| ChannelError::Unreadable)
            .and_then(|raw| parse_channel(&name, &raw));
        match parsed {
            Ok(channel) => set.valid.push(channel),
            Err(err) => set.invalid.push((name, err)),
        }
    }
    set.valid.sort_by(|a, b| a.name.cmp(&b.name));
    set.invalid.sort_by(|a, b| a.0.cmp(&b.0));
    set
}

/// Parses one channel URL (contracts/notifications.md "Provider, inferred
/// from the URL"). `format` and `insecure` are stripped before sending.
pub fn parse_channel(name: &str, raw: &str) -> Result<Channel, ChannelError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ChannelError::Empty);
    }
    let mut url = Url::parse(raw).map_err(|_| ChannelError::NotAUrl)?;
    let mut format = None;
    let mut insecure = false;
    let mut kept = Vec::new();
    for (key, value) in url.query_pairs() {
        match &*key {
            "format" => format = Some(value.into_owned()),
            "insecure" => insecure = value == "true",
            _ => kept.push((key.into_owned(), value.into_owned())),
        }
    }
    let (provider, target) = match url.scheme() {
        scheme @ ("https" | "http") => {
            if scheme == "http" && !insecure {
                return Err(ChannelError::Insecure("http"));
            }
            if kept.is_empty() {
                url.set_query(None);
            } else {
                url.query_pairs_mut().clear().extend_pairs(&kept);
            }
            let provider = match format.as_deref() {
                None => infer(&url),
                Some("discord") => Provider::Discord,
                Some("slack") => Provider::Slack,
                Some("generic") => Provider::Generic,
                Some(_) => return Err(ChannelError::Format),
            };
            (provider, Target::Webhook(Secret::new(url)))
        }
        "smtp" | "smtps" => {
            if format.is_some() {
                return Err(ChannelError::FormatOnEmail);
            }
            let email = email(&url, &kept, insecure)?;
            (Provider::Email, Target::Email(Box::new(email)))
        }
        _ => return Err(ChannelError::Scheme),
    };
    Ok(Channel {
        name: name.to_owned(),
        provider,
        target,
    })
}

fn infer(url: &Url) -> Provider {
    let host = url.host_str().unwrap_or_default();
    let discord_host =
        host == "discord.com" || host == "discordapp.com" || host.ends_with(".discord.com");
    if discord_host && url.path().starts_with("/api/webhooks/") {
        Provider::Discord
    } else if host == "hooks.slack.com" {
        Provider::Slack
    } else {
        Provider::Generic
    }
}

fn email(url: &Url, query: &[(String, String)], insecure: bool) -> Result<Email, ChannelError> {
    let param = |key: &str| {
        query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.trim().is_empty())
    };
    let from = param("from")
        .ok_or(ChannelError::NoFrom)?
        .trim()
        .parse::<Mailbox>()
        .map_err(|_| ChannelError::Address("from"))?;
    let to = param("to")
        .ok_or(ChannelError::NoTo)?
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<Mailbox>()
                .map_err(|_| ChannelError::Address("to"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if to.is_empty() {
        return Err(ChannelError::NoTo);
    }

    let host = url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or(ChannelError::NoHost)?;
    let implicit = url.scheme() == "smtps";
    let params = TlsParameters::new(host.to_owned()).map_err(|_| ChannelError::Tls)?;
    let tls = if implicit {
        Tls::Wrapper(params)
    } else if insecure {
        Tls::Opportunistic(params)
    } else {
        Tls::Required(params)
    };
    let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
        .port(url.port().unwrap_or(if implicit { 465 } else { 587 }))
        .tls(tls);
    if !url.username().is_empty() {
        let decode = |s: &str| {
            percent_decode_str(s)
                .decode_utf8()
                .map(std::borrow::Cow::into_owned)
                .map_err(|_| ChannelError::Credentials)
        };
        let user = decode(url.username())?;
        let password = decode(url.password().unwrap_or_default())?;
        builder = builder.credentials(Credentials::new(user, password));
    }
    Ok(Email {
        transport: Secret::new(builder.build()),
        from,
        to,
    })
}
