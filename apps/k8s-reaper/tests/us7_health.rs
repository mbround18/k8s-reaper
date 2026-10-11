//! US1 of 002 (health), on the dev cluster: quickstart §1, steps 1-3
//! (T128, FR-001..FR-003, SC-001, SC-002).
//!
//! Needs the reaper running from `deploy/dev/reaper.yaml` (`make dev-run`)
//! with the dev ReaperConfig (`cycleInterval: 15s`). Step 3 also needs a
//! debug image, which has the test-only hang hook:
//!
//! ```sh
//! make image-debug dev-load dev-run
//! REAPER_TEST_DEBUG_IMAGE=1 make test-integration STORY=us7_health
//! ```
//!
//! Without `REAPER_TEST_DEBUG_IMAGE=1` step 3 is skipped with a message.
//! Every change to the ReaperConfig and the reaper's Deployment is put back
//! when the test ends, even when it fails.

mod common;
mod full;

use std::future::Future;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{ListParams, Patch, PatchParams};
use kube::{Api, Client, ResourceExt as _};
use serde_json::{Value, json};

const NS: &str = "k8s-reaper";
const NAME: &str = "k8s-reaper";
/// `cycleInterval` of the dev ReaperConfig.
const INTERVAL: Duration = Duration::from_secs(15);
const HANG_VAR: &str = "REAPER_DEV_HANG_AFTER";
/// Set when the dev cluster runs `make image-debug`.
const DEBUG_IMAGE: &str = "REAPER_TEST_DEBUG_IMAGE";

/// The reaper's newest pod that is not being deleted.
async fn reaper_pod(client: &Client) -> Option<Pod> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), NS);
    let list = pods
        .list(&ListParams::default().labels("app.kubernetes.io/name=k8s-reaper"))
        .await
        .unwrap();
    list.items
        .into_iter()
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .max_by_key(|p| p.metadata.creation_timestamp.clone())
}

fn restarts(pod: &Pod) -> i32 {
    pod.status
        .as_ref()
        .and_then(|s| s.container_statuses.as_ref())
        .map_or(0, |c| c.iter().map(|c| c.restart_count).sum())
}

/// `/readyz` of `pod`, through the API server's pod proxy: status and body.
/// `None` while the pod does not answer (starting, or restarting).
async fn readyz(client: &Client, pod: &str) -> Option<(u16, String)> {
    let request = http::Request::get(format!(
        "/api/v1/namespaces/{NS}/pods/{pod}:8080/proxy/readyz"
    ))
    .body(kube::client::Body::empty())
    .unwrap();
    let response = client.send(request).await.ok()?;
    let status = response.status().as_u16();
    let body = response.into_body().collect_bytes().await.ok()?;
    // The API server answers 502/503 itself when it cannot reach the pod;
    // only the reaper's own answers end in a newline-terminated line.
    let body = String::from_utf8_lossy(&body).into_owned();
    matches!(status, 200 | 503)
        .then_some((status, body))
        .filter(|(_, b)| b.ends_with('\n') && !b.trim_start().starts_with('{'))
}

/// `(status, reason)` of the ReaperConfig condition `type_`.
async fn condition(client: &Client, type_: &str) -> Option<(String, String)> {
    let config = full::reaper_configs(client).get("default").await.unwrap();
    let conditions = config.data["status"]["conditions"].as_array()?.clone();
    let c = conditions.iter().find(|c| c["type"] == type_)?;
    Some((
        c["status"].as_str()?.to_owned(),
        c["reason"].as_str()?.to_owned(),
    ))
}

fn is(c: Option<(String, String)>, status: &str, reason: &str) -> bool {
    c.is_some_and(|(s, r)| s == status && r == reason)
}

/// Polls `done` until it holds, failing after `deadline`.
async fn wait_for<F, Fut>(what: &str, deadline: Duration, mut done: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let started = Instant::now();
    while !done().await {
        assert!(
            started.elapsed() < deadline,
            "not {what} after {deadline:?}"
        );
        tokio::time::sleep(full::POLL).await;
    }
}

fn token() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .to_string()
}

/// Rolls the reaper's Deployment with `patch` merged into its pod template,
/// then waits for a pod other than the current one to run.
async fn roll(client: &Client, mut patch: Value) -> String {
    let old = reaper_pod(client).await.map(|p| p.name_any());
    patch["metadata"]["annotations"]["kubectl.kubernetes.io/restartedAt"] = json!(token());
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), NS);
    deployments
        .patch(
            NAME,
            &PatchParams::default(),
            &Patch::Strategic(json!({ "spec": { "template": patch } })),
        )
        .await
        .unwrap();
    let deadline = Duration::from_mins(2);
    let started = Instant::now();
    loop {
        if let Some(p) = reaper_pod(client).await {
            let running = p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running");
            let name = p.name_any();
            if running && old.as_ref() != Some(&name) {
                return name;
            }
        }
        assert!(
            started.elapsed() < deadline,
            "no new reaper pod running after {deadline:?}"
        );
        tokio::time::sleep(full::POLL).await;
    }
}

/// Runs `f` on a fresh runtime from a drop, where the test's runtime is busy.
fn in_drop(
    what: &'static str,
    f: impl FnOnce(Client) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> + Send + 'static,
) {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async { f(common::dev_client().await).await });
    })
    .join()
    .expect(what);
}

/// The dev ReaperConfig's spec, put back when dropped.
struct SavedConfig {
    spec: Value,
}

impl SavedConfig {
    async fn take(client: &Client) -> Self {
        let config = full::reaper_configs(client).get("default").await.unwrap();
        let spec = config.data["spec"].clone();
        assert_eq!(
            spec["cycleInterval"], "15s",
            "the dev ReaperConfig should have cycleInterval 15s before this story"
        );
        Self { spec }
    }
}

impl Drop for SavedConfig {
    fn drop(&mut self) {
        let spec = self.spec.clone();
        in_drop("putting the dev ReaperConfig back", move |client| {
            Box::pin(async move { full::patch_config(&client, &spec).await })
        });
    }
}

/// Removes the hang hook's variable from the reaper's Deployment when dropped.
struct NoHang;

impl Drop for NoHang {
    fn drop(&mut self) {
        in_drop("removing the hang hook from the reaper", |client| {
            Box::pin(async move {
                let deployments: Api<Deployment> = Api::namespaced(client, NS);
                let patch = json!({ "spec": { "template": { "spec": { "containers": [{
                    "name": NAME,
                    "env": [{ "name": HANG_VAR, "$patch": "delete" }],
                }]}}}});
                deployments
                    .patch(NAME, &PatchParams::default(), &Patch::Strategic(patch))
                    .await
                    .unwrap();
            })
        });
    }
}

const INVALID: &str = "5s";

#[tokio::test]
#[ignore = "needs the dev cluster: make test-integration STORY=us7_health"]
async fn readiness_follows_config_and_cycles() {
    let client = common::dev_client().await;
    let _config = SavedConfig::take(&client).await;

    // Step 1: an invalid config at start keeps the pod unready.
    full::patch_config(&client, &json!({ "cycleInterval": INVALID })).await;
    let pod = roll(&client, json!({})).await;
    wait_for(
        "503 'no valid config applied yet' from /readyz",
        Duration::from_mins(2),
        || async {
            readyz(&client, &pod)
                .await
                .is_some_and(|(s, b)| s == 503 && b == "no valid config applied yet\n")
        },
    )
    .await;
    // The config's fault shows on Valid; Ready reports only the loop
    // (contracts/config.md), so it never says NotReady.
    let valid = condition(&client, "Valid").await;
    assert!(
        valid.as_ref().is_some_and(|(s, _)| s == "False"),
        "Valid: {valid:?}"
    );
    let (_, reason) = condition(&client, "Ready")
        .await
        .expect("a Ready condition");
    assert_ne!(reason, "NotReady");

    // Fixed: ready within a cycle or two.
    full::patch_config(&client, &json!({ "cycleInterval": "15s" })).await;
    wait_for("ready once the config is fixed", INTERVAL * 3, || async {
        readyz(&client, &pod).await.is_some_and(|(s, _)| s == 200)
            && is(condition(&client, "Ready").await, "True", "CycleCompleted")
    })
    .await;

    // Step 2: an invalid edit while running is reported, and the pod stays
    // ready (owner decision Q1).
    full::patch_config(&client, &json!({ "cycleInterval": INVALID })).await;
    wait_for(
        "Valid=False after the invalid edit",
        INTERVAL * 3,
        || async {
            condition(&client, "Valid")
                .await
                .is_some_and(|(s, _)| s == "False")
        },
    )
    .await;
    let watch_until = Instant::now() + INTERVAL * 4;
    while Instant::now() < watch_until {
        let answer = readyz(&client, &pod).await;
        assert!(
            answer.as_ref().is_none_or(|(s, _)| *s == 200),
            "/readyz after an invalid edit: {answer:?}"
        );
        tokio::time::sleep(full::POLL).await;
    }
}

#[tokio::test]
#[ignore = "needs the dev cluster and a debug image: make image-debug dev-load dev-run"]
async fn a_hung_loop_shows_stalled_and_is_restarted() {
    if std::env::var_os(DEBUG_IMAGE).is_none() {
        eprintln!(
            "skipped: the hang hook exists only in debug builds. Run \
             `make image-debug dev-load dev-run`, then set {DEBUG_IMAGE}=1"
        );
        return;
    }
    let client = common::dev_client().await;
    let _config = SavedConfig::take(&client).await;
    let _no_hang = NoHang;

    // Step 3: cycle 2 hangs forever.
    let pod = roll(
        &client,
        json!({ "spec": { "containers": [{
            "name": NAME,
            "env": [{ "name": HANG_VAR, "value": "1" }],
        }]}}),
    )
    .await;
    let started = Instant::now();

    // Stalled once the last finish is 3 intervals old, written at the status
    // task's next tick; allow start-up and two more ticks.
    wait_for(
        "Ready=False/Stalled",
        Duration::from_mins(1) + INTERVAL * 5,
        || async { is(condition(&client, "Ready").await, "False", "Stalled") },
    )
    .await;

    // SC-001: the liveness window (max(10 cycles, 10m)) plus the probe's
    // failures, then the kubelet restarts the container.
    let pods: Api<Pod> = Api::namespaced(client.clone(), NS);
    wait_for(
        "restartCount up",
        Duration::from_mins(12).saturating_sub(started.elapsed()),
        || async { pods.get(&pod).await.is_ok_and(|p| restarts(&p) > 0) },
    )
    .await;
}
