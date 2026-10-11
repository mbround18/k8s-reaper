//! The bounded queue and its worker (contracts/notifications.md "Delivery").
//! Enqueueing never blocks a cycle; a full queue drops the oldest entry.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use lettre::AsyncTransport as _;
use lettre::Message;
use lettre::message::header::ContentType;
use reaper_core::config::Notifications;
use reaper_core::notify::{Notification, Notifier, channel_wants};
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, RETRY_AFTER};
use tokio::sync::{Notify, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tracing::warn;

use crate::channels::{Channel, Email, Provider, Target};
use crate::payload;

/// Delivery tuning. The defaults are the contract; tests shorten them.
#[derive(Debug, Clone)]
pub struct Settings {
    pub queue_capacity: usize,
    /// Per attempt.
    pub timeout: Duration,
    /// Waits between attempts; attempts are `backoff.len() + 1`.
    pub backoff: Vec<Duration>,
    /// Upper bound for a `429` `Retry-After`.
    pub max_retry_after: Duration,
    /// Messages in flight at once, so one slow channel does not hold the rest.
    pub concurrency: usize,
    pub user_agent: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            queue_capacity: 256,
            timeout: Duration::from_secs(10),
            backoff: vec![Duration::from_secs(1), Duration::from_secs(4)],
            max_retry_after: Duration::from_mins(1),
            concurrency: 4,
            user_agent: "k8s-reaper".to_owned(),
        }
    }
}

/// The `result` label of `reaper_notifications_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Delivery {
    Sent,
    Failed,
    Dropped,
    Filtered,
}

impl Delivery {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Delivery::Sent => "sent",
            Delivery::Failed => "failed",
            Delivery::Dropped => "dropped",
            Delivery::Filtered => "filtered",
        }
    }
}

/// Called once per channel and notification with the result: the metrics hook.
pub type Report = Arc<dyn Fn(&str, Provider, Delivery) + Send + Sync>;

struct Job {
    channel: Arc<Channel>,
    notification: Arc<Notification>,
    cluster: Arc<str>,
}

struct Shared {
    queue: Mutex<VecDeque<Job>>,
    wake: Notify,
    closed: AtomicBool,
    channels: RwLock<Vec<Arc<Channel>>>,
    settings: Settings,
    report: Report,
    client: reqwest::Client,
}

impl Shared {
    fn report(&self, channel: &Channel, delivery: Delivery) {
        (self.report)(channel.name(), channel.provider(), delivery);
    }
}

/// Owns the queue and its worker task. Start it inside a Tokio runtime.
pub struct NotifyHandle {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Debug, thiserror::Error)]
#[error("building the HTTP client failed")]
pub struct BuildError;

impl NotifyHandle {
    pub fn start(settings: Settings, report: Report) -> Result<Self, BuildError> {
        // kube uses ring too; whichever installs first wins, which is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .timeout(settings.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(settings.user_agent.clone())
            .build()
            .map_err(|_| BuildError)?;
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            wake: Notify::new(),
            closed: AtomicBool::new(false),
            channels: RwLock::new(Vec::new()),
            settings,
            report,
            client,
        });
        let worker = tokio::spawn(run(Arc::clone(&shared)));
        Ok(Self {
            shared,
            worker: Mutex::new(Some(worker)),
        })
    }

    /// Replaces the channels (the directory is re-read every cycle). Messages
    /// already queued keep the channel they were queued for.
    pub fn set_channels(&self, channels: Vec<Channel>) {
        *self
            .shared
            .channels
            .write()
            .unwrap_or_else(PoisonError::into_inner) = channels.into_iter().map(Arc::new).collect();
    }

    /// Stops taking messages and delivers what is queued, for at most
    /// `deadline`. Returns how many were left undelivered.
    pub async fn shutdown(&self, deadline: Duration) -> usize {
        self.shared.closed.store(true, Ordering::SeqCst);
        self.shared.wake.notify_one();
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(mut worker) = worker
            && tokio::time::timeout(deadline, &mut worker).await.is_err()
        {
            worker.abort();
        }
        let left: Vec<Job> = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
            .collect();
        for job in &left {
            self.shared.report(&job.channel, Delivery::Dropped);
        }
        left.len()
    }
}

impl Notifier for NotifyHandle {
    fn enqueue(&self, notification: Notification, config: &Notifications) {
        let channels = self
            .shared
            .channels
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if channels.is_empty() {
            return;
        }
        let notification = Arc::new(notification);
        let cluster: Arc<str> = Arc::from(config.cluster_name.as_str());
        let closed = self.shared.closed.load(Ordering::SeqCst);
        let mut dropped = Vec::new();
        {
            let mut queue = self
                .shared
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for channel in channels {
                if !channel_wants(config, channel.name(), notification.reason) {
                    self.shared.report(&channel, Delivery::Filtered);
                    continue;
                }
                if closed {
                    dropped.push(channel);
                    continue;
                }
                queue.push_back(Job {
                    channel,
                    notification: Arc::clone(&notification),
                    cluster: Arc::clone(&cluster),
                });
                while queue.len() > self.shared.settings.queue_capacity {
                    if let Some(oldest) = queue.pop_front() {
                        dropped.push(oldest.channel);
                    }
                }
            }
        }
        for channel in dropped {
            warn!(
                channel = channel.name(),
                provider = channel.provider().as_str(),
                "notification queue full; dropped the oldest message"
            );
            self.shared.report(&channel, Delivery::Dropped);
        }
        self.shared.wake.notify_one();
    }
}

async fn run(shared: Arc<Shared>) {
    let permits = Arc::new(Semaphore::new(shared.settings.concurrency.max(1)));
    let mut tasks = JoinSet::new();
    loop {
        // Wait for a free slot first, so the backlog stays in the queue where
        // overflow can drop its oldest entry.
        let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
            break;
        };
        let job = shared
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        match job {
            Some(job) => {
                let shared = Arc::clone(&shared);
                tasks.spawn(async move {
                    deliver(&shared, &job).await;
                    drop(permit);
                });
                while tasks.try_join_next().is_some() {}
            }
            None if shared.closed.load(Ordering::SeqCst) => break,
            None => {
                drop(permit);
                shared.wake.notified().await;
            }
        }
    }
    while tasks.join_next().await.is_some() {}
}

/// Why one attempt failed, in words that never include the URL.
struct Failure {
    kind: String,
    retry: bool,
    retry_after: Option<Duration>,
}

impl Failure {
    fn retry(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            retry: true,
            retry_after: None,
        }
    }

    fn fatal(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            retry: false,
            retry_after: None,
        }
    }
}

async fn deliver(shared: &Shared, job: &Job) {
    let settings = &shared.settings;
    let mut attempt = 0;
    loop {
        let result = tokio::time::timeout(settings.timeout, send(shared, job))
            .await
            .unwrap_or_else(|_| Err(Failure::retry("timeout")));
        let failure = match result {
            Ok(()) => {
                shared.report(&job.channel, Delivery::Sent);
                return;
            }
            Err(failure) => failure,
        };
        let Some(backoff) = settings.backoff.get(attempt).filter(|_| failure.retry) else {
            warn!(
                channel = job.channel.name(),
                provider = job.channel.provider().as_str(),
                reason = job.notification.reason,
                attempts = attempt + 1,
                error = %failure.kind,
                "notification not delivered"
            );
            shared.report(&job.channel, Delivery::Failed);
            return;
        };
        let wait = failure
            .retry_after
            .map_or(*backoff, |after| after.min(settings.max_retry_after));
        attempt += 1;
        tokio::time::sleep(wait).await;
    }
}

async fn send(shared: &Shared, job: &Job) -> Result<(), Failure> {
    let n = &job.notification;
    let cluster = &job.cluster;
    let (url, body) = match &job.channel.target {
        Target::Email(email) => return send_email(email, n, cluster).await,
        Target::Webhook(url) => {
            let body = match job.channel.provider() {
                Provider::Discord => payload::discord(n, cluster),
                Provider::Slack => payload::slack(n, cluster),
                Provider::Generic | Provider::Email => payload::generic(n, cluster),
            };
            (url, body)
        }
    };
    let response = shared
        .client
        .post(url.expose().clone())
        .header(CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|err| {
            if err.is_builder() {
                Failure::fatal("invalid request")
            } else if err.is_timeout() {
                Failure::retry("timeout")
            } else if err.is_connect() {
                Failure::retry("connect")
            } else {
                Failure::retry("request")
            }
        })?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let kind = format!("HTTP {}", status.as_u16());
    if status == StatusCode::TOO_MANY_REQUESTS {
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        return Err(Failure {
            kind,
            retry: true,
            retry_after,
        });
    }
    if status == StatusCode::REQUEST_TIMEOUT || status.is_server_error() {
        return Err(Failure::retry(kind));
    }
    Err(Failure::fatal(kind))
}

async fn send_email(email: &Email, n: &Notification, cluster: &str) -> Result<(), Failure> {
    let mut builder = Message::builder()
        .from(email.from.clone())
        .subject(payload::email_subject(n))
        .header(ContentType::TEXT_PLAIN);
    for to in &email.to {
        builder = builder.to(to.clone());
    }
    let message = builder
        .body(payload::email_body(n, cluster))
        .map_err(|_| Failure::fatal("building the message"))?;
    email
        .transport
        .expose()
        .send(message)
        .await
        .map(|_| ())
        .map_err(|err| {
            if err.is_permanent() {
                Failure::fatal("SMTP permanent error")
            } else if err.is_timeout() {
                Failure::retry("SMTP timeout")
            } else if err.is_tls() {
                Failure::retry("SMTP TLS error")
            } else {
                Failure::retry("SMTP error")
            }
        })
}
