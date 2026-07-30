// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#![cfg(feature = "e2e-kubernetes")]

use std::process::Stdio;
use std::time::{Duration, Instant};

use openshell_e2e::harness::binary::openshell_cmd;
use openshell_e2e::harness::output::strip_ansi;
use openshell_e2e::harness::sandbox::SandboxGuard;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::sleep;

#[tokio::test]
async fn kubernetes_sandbox_suspend_resume_preserves_workspace() {
    let mut sandbox = SandboxGuard::create_keep(
        &[
            "sh",
            "-lc",
            "printf suspend-resume-marker > /sandbox/suspend-resume-marker; echo marker-written; sleep infinity",
        ],
        "marker-written",
    )
    .await
    .expect("sandbox create should succeed");

    wait_for_phase(&sandbox.name, "Ready").await;

    let before = kubectl_json("sandbox", &sandbox.name).await;
    let sandbox_uid = json_string(&before, "/metadata/uid");
    let old_pod = owned_pod(&sandbox_uid).await;
    let old_pod_name = json_string(&old_pod, "/metadata/name");
    let old_pod_uid = json_string(&old_pod, "/metadata/uid");
    let pvc_uids = owned_pvc_uids(&sandbox_uid).await;
    assert!(!pvc_uids.is_empty(), "sandbox should own at least one PVC");

    run_lifecycle_command("suspend", &sandbox.name).await;

    let suspended = kubectl_json("sandbox", &sandbox.name).await;
    assert_eq!(
        suspended.pointer("/spec/operatingMode").and_then(Value::as_str),
        Some("Suspended")
    );
    assert!(
        suspended
            .pointer("/status/conditions")
            .and_then(Value::as_array)
            .is_some_and(|conditions| conditions.iter().any(|condition| {
                condition.get("type").and_then(Value::as_str) == Some("Suspended")
                    && condition.get("status").and_then(Value::as_str) == Some("True")
            })),
        "agent-sandbox should report Suspended=True"
    );
    wait_for_resource_absent("pod", &old_pod_name).await;
    assert_eq!(owned_pvc_uids(&sandbox_uid).await, pvc_uids);

    run_lifecycle_command("resume", &sandbox.name).await;

    let resumed = kubectl_json("sandbox", &sandbox.name).await;
    assert_eq!(json_string(&resumed, "/metadata/uid"), sandbox_uid);
    assert_eq!(
        resumed.pointer("/spec/operatingMode").and_then(Value::as_str),
        Some("Running")
    );
    let new_pod = owned_pod(&sandbox_uid).await;
    assert_ne!(json_string(&new_pod, "/metadata/uid"), old_pod_uid);
    assert_eq!(owned_pvc_uids(&sandbox_uid).await, pvc_uids);

    let marker = sandbox
        .exec(&["cat", "/sandbox/suspend-resume-marker"])
        .await
        .expect("workspace marker should remain readable after resume");
    assert!(marker.contains("suspend-resume-marker"));

    sandbox.cleanup().await;
}

async fn run_lifecycle_command(operation: &str, sandbox_name: &str) {
    let output = openshell_cmd()
        .args(["sandbox", operation, sandbox_name])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .unwrap_or_else(|error| panic!("failed to run sandbox {operation}: {error}"));
    let combined = strip_ansi(&format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ));
    assert!(
        output.status.success(),
        "sandbox {operation} failed (status {:?}):\n{combined}",
        output.status.code()
    );
}

async fn wait_for_phase(sandbox_name: &str, expected_phase: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let output = openshell_cmd()
            .args(["sandbox", "get", sandbox_name])
            .output()
            .await
            .expect("failed to run sandbox get");
        let combined = strip_ansi(&format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
        assert!(
            output.status.success(),
            "sandbox get failed while waiting for {expected_phase}:\n{combined}"
        );
        if combined
            .lines()
            .any(|line| line.trim() == format!("Phase: {expected_phase}"))
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "sandbox did not reach {expected_phase} within 30 seconds:\n{combined}"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn kubectl_json(resource: &str, name: &str) -> Value {
    let mut command = Command::new("kubectl");
    let context = kube_context();
    command.args(["--context", &context]);
    command.args(["-n", "openshell", "get", resource]);
    if !name.is_empty() {
        command.arg(name);
    }
    let output = command
        .args(["-o", "json"])
        .output()
        .await
        .unwrap_or_else(|error| panic!("failed to run kubectl get {resource}/{name}: {error}"));
    assert!(
        output.status.success(),
        "kubectl get {resource}/{name} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("kubectl output should be JSON")
}

async fn kubectl_resource_exists(resource: &str, name: &str) -> bool {
    let context = kube_context();
    Command::new("kubectl")
        .args(["--context", &context])
        .args(["-n", "openshell", "get", resource, name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .expect("failed to run kubectl")
        .success()
}

async fn wait_for_resource_absent(resource: &str, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(90);
    while kubectl_resource_exists(resource, name).await {
        assert!(
            Instant::now() < deadline,
            "{resource}/{name} was not deleted within 90 seconds"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn owned_pvc_uids(sandbox_uid: &str) -> Vec<String> {
    let list = kubectl_json("pvc", "").await;
    let mut uids = list
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|pvc| {
            pvc.pointer("/metadata/ownerReferences")
                .and_then(Value::as_array)
                .is_some_and(|owners| {
                    owners.iter().any(|owner| {
                        owner.get("uid").and_then(Value::as_str) == Some(sandbox_uid)
                    })
                })
        })
        .map(|pvc| json_string(pvc, "/metadata/uid"))
        .collect::<Vec<_>>();
    uids.sort();
    uids
}

async fn owned_pod(sandbox_uid: &str) -> Value {
    let list = kubectl_json("pod", "").await;
    let pods = list
        .get("items")
        .and_then(Value::as_array)
        .expect("pod list should contain items")
        .iter()
        .filter(|pod| resource_is_owned_by(pod, sandbox_uid))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(pods.len(), 1, "sandbox should own exactly one pod");
    pods.into_iter().next().unwrap()
}

fn resource_is_owned_by(resource: &Value, owner_uid: &str) -> bool {
    resource
        .pointer("/metadata/ownerReferences")
        .and_then(Value::as_array)
        .is_some_and(|owners| {
            owners
                .iter()
                .any(|owner| owner.get("uid").and_then(Value::as_str) == Some(owner_uid))
        })
}

fn json_string(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing string at JSON pointer {pointer}"))
        .to_string()
}

fn kube_context() -> String {
    std::env::var("OPENSHELL_E2E_KUBE_CONTEXT")
        .unwrap_or_else(|_| panic!("OPENSHELL_E2E_KUBE_CONTEXT must be set by e2e/with-kube-gateway.sh"))
}
