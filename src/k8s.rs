//! Kubernetes (kube-rs) helpers: client, pod list, and follow-log reader.

use std::path::Path;

use futures::{AsyncBufReadExt, TryStreamExt};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, LogParams};
use kube::config::{Config, KubeConfigOptions, Kubeconfig};
use kube::Client;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub async fn connect(
    kubeconfig: Option<&Path>,
    kube_context: Option<&str>,
) -> Result<Client, String> {
    let mut opts = KubeConfigOptions::default();
    if let Some(ctx) = kube_context {
        opts.context = Some(ctx.to_string());
    }
    let config = if let Some(path) = kubeconfig {
        let kc = Kubeconfig::read_from(path).map_err(|e| format!("read kubeconfig: {e:?}"))?;
        Config::from_custom_kubeconfig(kc, &opts)
            .await
            .map_err(|e| format!("kube config: {e:?}"))?
    } else {
        Config::from_kubeconfig(&opts)
            .await
            .map_err(|e| format!("kube config: {e:?}"))?
    };
    Client::try_from(config).map_err(|e| format!("kube client: {e:?}"))
}

pub async fn list_pod_names(client: &Client, namespace: &str) -> Result<Vec<String>, String> {
    let api: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let list = api
        .list(&ListParams::default().limit(500))
        .await
        .map_err(|e| format!("list pods: {e:?}"))?;
    let mut names: Vec<String> = list
        .items
        .into_iter()
        .filter_map(|p| p.metadata.name)
        .collect();
    names.sort();
    Ok(names)
}

/// Follow pod logs; sends each line (without trailing newline) on `line_tx`.
pub fn spawn_pod_log_reader(
    client: Client,
    namespace: String,
    pod: String,
    container: Option<String>,
    tail_lines: i64,
    line_tx: mpsc::UnboundedSender<String>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let pods: Api<Pod> = Api::namespaced(client, &namespace);
        let lp = LogParams {
            follow: true,
            tail_lines: Some(tail_lines),
            container,
            ..LogParams::default()
        };
        let stream = match pods.log_stream(&pod, &lp).await {
            Ok(s) => s,
            Err(e) => {
                let _ = line_tx.send(format!("[k8s] log_stream: {e:?}"));
                return;
            }
        };
        let mut lines = stream.lines();
        while let Ok(Some(line)) = lines.try_next().await {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    })
}
