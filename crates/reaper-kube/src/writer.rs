//! `ClusterWriter` over the Kubernetes API: merge patches, including on the
//! `scale` subresource, each with a `resourceVersion` precondition.

use std::collections::BTreeMap;
use std::future::Future;

use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, StatefulSet};
use k8s_openapi::api::batch::v1::CronJob;
use k8s_openapi::api::core::v1::{Namespace, Pod};
use kube::Client;
use kube::api::{Api, ApiResource, DynamicObject, Patch, PatchParams};
use reaper_core::model::{NsName, ObjectKind, ObjectRef, WorkloadKind, WorkloadRef};
use reaper_core::ports::{ClusterWriter, HelmReleasePatch, WriteError};
use reaper_core::record::MetadataPatch;
use serde_json::{Map, Value, json};

use crate::crd::ReaperConfig;

pub const FIELD_MANAGER: &str = "k8s-reaper";

/// Group, version and plural for each kind the reaper writes or reports on.
#[must_use]
pub fn api_resource(kind: ObjectKind) -> ApiResource {
    match kind {
        ObjectKind::Deployment => ApiResource::erase::<Deployment>(&()),
        ObjectKind::StatefulSet => ApiResource::erase::<StatefulSet>(&()),
        ObjectKind::DaemonSet => ApiResource::erase::<DaemonSet>(&()),
        ObjectKind::CronJob => ApiResource::erase::<CronJob>(&()),
        ObjectKind::Pod => ApiResource::erase::<Pod>(&()),
        ObjectKind::Namespace => ApiResource::erase::<Namespace>(&()),
        ObjectKind::ReaperConfig => ApiResource::erase::<ReaperConfig>(&()),
        ObjectKind::HelmRelease => reaper_flux::helmrelease::api_resource(),
    }
}

/// Map a kube error onto the port's error.
#[must_use]
pub fn write_error(err: kube::Error) -> WriteError {
    match err {
        kube::Error::Api(s) if s.is_conflict() => WriteError::Conflict,
        kube::Error::Api(s) if s.is_not_found() => WriteError::NotFound,
        other => WriteError::Other(other.to_string()),
    }
}

#[derive(Clone)]
pub struct KubeWriter {
    client: Client,
}

impl KubeWriter {
    #[must_use]
    pub fn new(client: Client) -> Self {
        KubeWriter { client }
    }

    fn api(&self, kind: ObjectKind, namespace: Option<&str>) -> Api<DynamicObject> {
        let ar = api_resource(kind);
        match namespace {
            Some(ns) => Api::namespaced_with(self.client.clone(), ns, &ar),
            None => Api::all_with(self.client.clone(), &ar),
        }
    }

    async fn merge(
        &self,
        kind: ObjectKind,
        namespace: Option<&str>,
        name: &str,
        body: Value,
    ) -> Result<String, WriteError> {
        let obj = self
            .api(kind, namespace)
            .patch(name, &params(), &Patch::Merge(body))
            .await
            .map_err(write_error)?;
        new_version(obj.metadata.resource_version)
    }
}

fn params() -> PatchParams {
    PatchParams {
        field_manager: Some(FIELD_MANAGER.into()),
        ..PatchParams::default()
    }
}

fn new_version(rv: Option<String>) -> Result<String, WriteError> {
    rv.ok_or_else(|| WriteError::Other("response carried no resourceVersion".into()))
}

/// `{key: value}` with `null` for deletions.
fn merge_map(m: &BTreeMap<String, Option<String>>) -> Value {
    Value::Object(
        m.iter()
            .map(|(k, v)| (k.clone(), v.clone().map_or(Value::Null, Value::String)))
            .collect(),
    )
}

/// `metadata` with the precondition and any non-empty annotation/label maps.
fn metadata(
    resource_version: &str,
    annotations: &BTreeMap<String, Option<String>>,
    labels: &BTreeMap<String, Option<String>>,
) -> Value {
    let mut m = Map::new();
    m.insert("resourceVersion".into(), resource_version.into());
    if !annotations.is_empty() {
        m.insert("annotations".into(), merge_map(annotations));
    }
    if !labels.is_empty() {
        m.insert("labels".into(), merge_map(labels));
    }
    Value::Object(m)
}

impl ClusterWriter for KubeWriter {
    fn patch_metadata(
        &self,
        target: &ObjectRef,
        resource_version: &str,
        patch: &MetadataPatch,
    ) -> impl Future<Output = Result<String, WriteError>> + Send {
        let body = json!({
            "metadata": metadata(resource_version, &patch.annotations, &patch.labels),
        });
        self.merge(target.kind, target.namespace.as_deref(), &target.name, body)
    }

    fn scale(
        &self,
        workload: &WorkloadRef,
        resource_version: &str,
        replicas: u32,
    ) -> impl Future<Output = Result<String, WriteError>> + Send {
        let target = workload.object_ref();
        async move {
            if matches!(
                workload.kind,
                WorkloadKind::DaemonSet | WorkloadKind::CronJob
            ) {
                return Err(WriteError::Other(format!(
                    "{} has no scale subresource",
                    workload.kind.as_str()
                )));
            }
            let replicas = i32::try_from(replicas)
                .map_err(|_| WriteError::Other(format!("replicas {replicas} out of range")))?;
            let scale = self
                .api(target.kind, Some(&workload.namespace))
                .patch_scale(
                    &workload.name,
                    &params(),
                    &Patch::Merge(json!({
                        "metadata": {"resourceVersion": resource_version},
                        "spec": {"replicas": replicas},
                    })),
                )
                .await
                .map_err(write_error)?;
            new_version(scale.metadata.resource_version)
        }
    }

    fn set_cronjob_suspend(
        &self,
        workload: &WorkloadRef,
        resource_version: &str,
        suspend: bool,
    ) -> impl Future<Output = Result<String, WriteError>> + Send {
        let body = json!({
            "metadata": {"resourceVersion": resource_version},
            "spec": {"suspend": suspend},
        });
        self.merge(
            ObjectKind::CronJob,
            Some(&workload.namespace),
            &workload.name,
            body,
        )
    }

    fn patch_helm_release(
        &self,
        helm_release: &NsName,
        resource_version: &str,
        patch: &HelmReleasePatch,
    ) -> impl Future<Output = Result<String, WriteError>> + Send {
        let body = reaper_flux::holds::merge_body(resource_version, patch);
        self.merge(
            ObjectKind::HelmRelease,
            Some(&helm_release.namespace),
            &helm_release.name,
            body,
        )
    }
}
