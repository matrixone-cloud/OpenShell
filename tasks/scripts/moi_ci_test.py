# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Prevent mirror-gate regressions and false success in the internal CI result."""

import os
import subprocess
from pathlib import Path

import pytest
import yaml

ROOT = Path(__file__).resolve().parents[2]


def workflow(name: str) -> dict:
    # GitHub uses YAML 1.2; BaseLoader keeps the event key "on" as a string.
    return yaml.load(
        (ROOT / ".github/workflows" / name).read_text(), Loader=yaml.BaseLoader
    )


def test_prs_and_updates_run_without_mirrors_or_labels() -> None:
    ci = workflow("moi-ci.yml")
    assert ci["on"]["pull_request"]["branches"] == ["moi-dev"]
    assert {"opened", "synchronize", "reopened", "ready_for_review"} <= set(
        ci["on"]["pull_request"]["types"]
    )
    assert "paths" not in ci["on"]["pull_request"]
    assert "pull_request_target" not in ci["on"]
    assert ci["permissions"] == {"contents": "read"}
    checks = ci["jobs"]["checks"]
    assert checks["runs-on"] == "ubuntu-24.04"
    assert checks["if"] == "github.repository == 'matrixone-cloud/OpenShell'"
    assert "container" not in checks
    assert {row["lane"] for row in checks["strategy"]["matrix"]["include"]} == {
        "rust",
        "sdk",
        "repository",
        "helm",
    }
    assert checks["steps"][0]["with"]["persist-credentials"] == "false"
    assert all("pr-gate" not in step.get("uses", "") for step in checks["steps"])


def test_mirror_status_publisher_is_disabled_only_for_internal_repo() -> None:
    publish = workflow("required-ci-gates.yml")["jobs"]["publish"]
    assert publish["if"] == "github.repository != 'matrixone-cloud/OpenShell'"


def test_internal_labels_do_not_post_upstream_mirror_instructions() -> None:
    hint = workflow("e2e-label-help.yml")["jobs"]["hint"]
    assert hint["if"].startswith("github.repository != 'matrixone-cloud/OpenShell' &&")
    for label in ("test:e2e", "test:e2e-gpu", "test:e2e-kubernetes"):
        assert f"github.event.label.name == '{label}'" in hint["if"]


@pytest.mark.parametrize("result", ["success", "failure", "cancelled", "skipped"])
def test_result_job_rejects_failed_cancelled_or_skipped_lanes(result: str) -> None:
    job = workflow("moi-ci.yml")["jobs"]["result"]
    assert job["name"] == "MOI / CI"
    assert job["needs"] == "checks"
    assert "always()" in job["if"]
    assert job["env"]["CHECKS_RESULT"] == "${{ needs.checks.result }}"
    completed = subprocess.run(
        ["bash", "-e", "-c", job["steps"][0]["run"]],
        env={**os.environ, "CHECKS_RESULT": result},
        capture_output=True,
        text=True,
        check=False,
    )
    assert (completed.returncode == 0) == (result == "success")
