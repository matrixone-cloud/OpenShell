---
authors:
  - "@lr90"
state: draft
links:
  - (originating GitHub issue TBD — maintainer assigns RFC number)
---

# RFC NNNN - Per-sandbox externally-managed credentials

## Summary

Add a way for a control plane to inject a **per-sandbox, externally-minted
secret** (e.g. a workload's registration token) into a sandbox **without the
secret ever appearing in the pod spec, sandbox spec, or any read API**, and to
**rotate** that secret for a live or suspended sandbox without recreating the
pod. The secret is delivered to the sandbox supervisor over the existing
authenticated `GetSandboxProviderEnvironment` channel and injected into the
sandbox process environment, reusing the provider-environment refresh path that
already exists.

This generalizes the existing provider-credential system (which is
provider-profile scoped and reusable across sandboxes) to cover credentials that
are **unique per sandbox** and **owned by an external system**.

## Motivation

A control plane (MOI/matrixflow) launches a per-user agent sandbox that runs an
edge agent (`astra-edge`). The edge agent authenticates to a remote server with
a token that is:

- **per-sandbox unique** — bound to that sandbox's stable edge-agent id;
- **externally minted** — signed by the control plane, not by OpenShell;
- **long-lived (30d) and rotatable** — a long-running or long-suspended sandbox
  must be able to obtain a fresh token.

Today the only injection path is `SandboxSpec.environment` (a plain
`map<string,string>`). The Kubernetes driver copies it verbatim into the
container `env` array (`crates/openshell-driver-kubernetes/src/driver.rs`,
`build_env_list` / `sandbox_template_to_k8s_*`), so the token lands in:

- the Pod spec (readable via `kubectl get pod -o yaml`, etcd, control-plane logs,
  diagnostic dumps);
- OpenShell's stored `Sandbox` message (returned by `GetSandbox`/`ListSandboxes`).

"Not on the command line" is not secret storage. Any principal that can read a
Pod spec or the sandbox record obtains a 30-day credential that can register the
edge identity.

There is also **no rotation path**: the token is fixed at create in the pod env,
`Suspend`/`Resume` only call the driver (no env update), and the OpenShell API
has no way to update a running/suspended sandbox's environment. After the token
TTL elapses the edge can no longer re-authenticate; `Resume` returns ready but
the edge is permanently offline.

Leaving this unchanged blocks the managed-sandbox feature from production: it is
a credential-exposure finding and a lifecycle dead-end.

## Non-goals

- **LLM/provider API credentials.** The existing provider-profile credential
  system (OAuth2 grants, SPIRE JWT-SVID, static keys) is unchanged; this RFC adds
  a sibling for per-sandbox external secrets.
- **Replacing the token with workload identity (SPIFFE).** Having the edge
  authenticate to the remote server with its SPIFFE JWT-SVID would remove the
  need for an external token entirely, but requires the remote server to validate
  SVIDs and federate trust domains. Tracked separately as the long-term option.
- **A Kubernetes-driver-only Secret reference.** A `valueFrom.secretKeyRef`
  approach only helps the k8s driver and still needs a per-sandbox Secret
  lifecycle; this RFC keeps the solution driver-agnostic.
- **Rotating the credential of an already-running child process in place.** See
  Risks — the supervisor injects env into newly-spawned children; in-place
  rotation of a live process is out of scope for the first version.

## Proposal

### Data model

Per-sandbox credentials are stored **separately from the `Sandbox` message** so
they are never returned by `GetSandbox`/`ListSandboxes`. A new stored message,
keyed by sandbox id:

```proto
// Stored server-side, keyed by sandbox id. Never returned by Get/List sandbox.
message SandboxExternalCredentials {
  string sandbox_id = 1;
  // env var name -> credential
  repeated SandboxExternalCredential credentials = 2;
  // Monotonic version bumped on every update; folds into provider_env_revision.
  uint64 revision = 3;
}

message SandboxExternalCredential {
  // Environment variable name delivered to the sandbox process (e.g. ASTRA_TOKEN).
  string env_var = 1;
  // The secret value. Stored encrypted at rest; never logged; never in a pod spec.
  string value = 2;
  // Optional absolute expiry, surfaced via credential_expires_at_ms so the
  // supervisor and control plane can reason about rotation.
  int64 expires_at_ms = 3;
}
```

### API

Credentials are supplied at create and updated via a dedicated RPC:

```proto
message CreateSandboxRequest {
  SandboxSpec spec = 1;
  string name = 2;
  map<string, string> labels = 3;
  // NEW: per-sandbox external credentials. Stored separately from the spec and
  // delivered only via GetSandboxProviderEnvironment; never placed in the pod env.
  repeated SandboxExternalCredential external_credentials = 4;
}

// NEW RPC: update a sandbox's external credentials (rotation). auth=dual,
// scope=sandbox:write, role=user. Bumps the stored revision, which changes
// provider_env_revision and triggers the supervisor's existing re-fetch.
rpc UpdateSandboxCredentials(UpdateSandboxCredentialsRequest)
    returns (UpdateSandboxCredentialsResponse);

message UpdateSandboxCredentialsRequest {
  string sandbox_id = 1;
  repeated SandboxExternalCredential credentials = 2; // full replace
}
message UpdateSandboxCredentialsResponse {
  uint64 provider_env_revision = 1;
}
```

`GetSandboxProviderEnvironment` (unchanged signature) **merges** the per-sandbox
external credentials into its response:

- `environment[env_var] = value` for each credential;
- `credential_expires_at_ms[env_var] = expires_at_ms` when set;
- the per-sandbox credential revision is folded into `provider_env_revision`.

### Delivery path (already exists — reused, not rebuilt)

1. Supervisor calls `fetch_provider_environment(endpoint, sandbox_id)` at startup
   (`crates/openshell-sandbox/src/lib.rs:149`).
2. Supervisor injects the returned env into the sandbox process via
   `inject_provider_env` (`crates/openshell-supervisor-process/src/process.rs:48`).
   `astra-edge` reads `ASTRA_TOKEN` from its process env.
3. The settings poll loop re-fetches when `provider_env_revision` changes
   (`crates/openshell-sandbox/src/lib.rs:1630`) and calls `install_environment`.

So `UpdateSandboxCredentials` → bumped revision → supervisor re-fetch is fully
wired already. The only new code is: storing the per-sandbox record, merging it
into `resolve_provider_environment` + `compute_provider_env_revision`
(`crates/openshell-server/src/grpc/policy.rs:1238,1392`), and the new RPC.

### Security properties

- The value is stored only in the server's credential store (encrypted at rest,
  same as other refresh material) and returned only over the `auth = "sandbox"`
  `GetSandboxProviderEnvironment` channel, to the sandbox it belongs to
  (`enforce_sandbox_scope`).
- It is never written to `SandboxSpec.environment`, so it never reaches the pod
  spec, etcd, or `GetSandbox`/`ListSandboxes`.
- Deleting the sandbox deletes its credential record.

### Lifecycle

- **Create:** `external_credentials` stored under the sandbox id.
- **Resume after long suspend:** control plane calls `UpdateSandboxCredentials`
  with a fresh token before/at resume; the resumed supervisor fetches the new
  value on startup.
- **Rotation of a live sandbox:** `UpdateSandboxCredentials` bumps the revision;
  supervisor re-fetches and updates the injected env for the next child spawn
  (see Risks for in-place limitation).
- **Delete:** credential record removed.

## Implementation plan

1. **proto** (`proto/openshell.proto`): add `SandboxExternalCredential`,
   `SandboxExternalCredentials`, `CreateSandboxRequest.external_credentials`,
   `UpdateSandboxCredentials` RPC + messages. Regenerate Rust + the vendored
   moi-backend Go stubs.
2. **store**: persist `SandboxExternalCredentials` keyed by sandbox id in the
   existing encrypted credential store; delete on sandbox delete.
3. **server** (`grpc/mod.rs`, `grpc/policy.rs`, `grpc/provider.rs`):
   - `CreateSandbox`: persist `external_credentials`.
   - `resolve_provider_environment` / `handle_get_sandbox_provider_environment`:
     merge per-sandbox credentials into `environment` + `credential_expires_at_ms`.
   - `compute_provider_env_revision`: fold in the per-sandbox credential revision.
   - `UpdateSandboxCredentials`: replace record, persist, return new revision.
4. **supervisor**: no change required for suspend/resume + next-spawn rotation.
   Optional follow-up: restart the sandbox command on `provider_env_changed` so a
   continuously-running process picks up a rotated token.
5. **moi-backend** (matrixflow): stop putting `ASTRA_TOKEN` in
   `SandboxSpec.Environment`; pass it in `CreateSandboxRequest.external_credentials`;
   on `Resume` when the token is near expiry, sign a new one and call
   `UpdateSandboxCredentials`. Regenerate `pkg/openshellpb`.
6. **tests**: server unit tests (merge, revision bump, scope enforcement, not in
   Get/List); e2e that `ASTRA_TOKEN` is absent from the pod spec but present in
   the sandbox process env; rotation via `UpdateSandboxCredentials` reflected in a
   subsequent `GetSandboxProviderEnvironment`.

## Risks

- **In-place rotation of a running process.** `install_environment` updates the
  in-memory snapshot used for the *next* child spawn; a long-running `astra-edge`
  keeps its original token until it respawns (crash/reconnect or resume). For the
  primary suspend→resume rotation case this is sufficient. Continuous-rotation
  needs the optional supervisor restart-on-change (step 4) — deferred.
- **Encryption at rest.** The value must use the same at-rest encryption as
  existing refresh material; a plaintext store would just move the exposure.
- **Read-API leakage regressions.** Tests must assert the value never appears in
  `GetSandbox`/`ListSandboxes` or any diagnostic dump.

## Alternatives

- **Kubernetes Secret reference** (`valueFrom.secretKeyRef`): driver-specific
  (k8s only), needs per-sandbox Secret lifecycle, rotation still needs pod
  restart or a mounted-file re-read. Rejected as narrower and non-portable.
- **SPIFFE workload identity**: edge authenticates with its JWT-SVID; no external
  token, no rotation problem, no plaintext. Strategically cleanest but requires
  remote-server SVID validation + trust-domain federation. Tracked as the
  long-term direction; out of scope here.
- **Status quo (plaintext env)**: rejected — credential exposure + no rotation.

## Prior art

- Provider-profile credentials with `GetSandboxProviderEnvironment`, per-var
  `credential_expires_at_ms`, `provider_env_revision`, and refresh strategies
  incl. `External` ("managed by an external system") — this RFC is the
  per-sandbox analogue.
- SPIFFE/SPIRE JWT-SVID rotation into `/var/run/secrets/openshell/token`.

## Open questions

- Should `external_credentials` be full-replace only (simpler) or support
  per-key patch? (Proposal: full replace.)
- Auth scope for `UpdateSandboxCredentials`: reuse `sandbox:write`/user, or a
  dedicated control-plane scope?
- Do we want the optional supervisor restart-on-change now, or defer until a
  continuous-rotation use case appears?
