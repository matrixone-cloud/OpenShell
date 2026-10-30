// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#![cfg(feature = "e2e-kubernetes-unrestricted")]

//! Exercise direct and allowlisted egress through the real Kubernetes driver,
//! workload supervisor, and SSH session while both sandboxes remain alive.

use std::io::Write as _;

use openshell_e2e::harness::container::SupportContainer;
use openshell_e2e::harness::sandbox::SandboxGuard;
use tempfile::NamedTempFile;

const FIXTURE: &str = r#"
import http.server, socket, threading
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'network-fixture'
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args): pass
udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
udp.bind(('0.0.0.0', 8000))
def echo():
    while True:
        data, address = udp.recvfrom(1024)
        udp.sendto(data, address)
threading.Thread(target=echo, daemon=True).start()
http.server.HTTPServer(('0.0.0.0', 8000), Handler).serve_forever()
"#;

const POLICY: &str = r"version: 1
filesystem_policy:
  include_workdir: true
  read_only: [/usr, /lib, /lib64, /bin, /sbin, /proc, /dev/urandom, /etc]
  read_write: [/sandbox, /tmp, /dev/null]
landlock:
  compatibility: best_effort
process:
  run_as_user: sandbox
  run_as_group: sandbox
network_policies: {}
";

fn probe(ip: &str, unrestricted: bool) -> String {
    let enabled = if unrestricted { "True" } else { "False" };
    format!(
        r#"
import os, socket, urllib.request
unrestricted = {enabled}
assert os.getuid() != 0, 'workload must retain non-root identity'
try:
    raw = socket.socket(socket.AF_INET, socket.SOCK_RAW, socket.IPPROTO_ICMP)
except PermissionError:
    pass
else:
    raw.close()
    raise AssertionError('ordinary networking must not grant raw sockets')
if unrestricted:
    for key in ('HTTP_PROXY','HTTPS_PROXY','ALL_PROXY','all_proxy','grpc_proxy','GRPC_PROXY'):
        assert key not in os.environ, 'forced proxy routing must be removed'
    socket.gethostbyname('kubernetes.default.svc.cluster.local')
def http():
    with urllib.request.urlopen('http://' + {ip:?} + ':8000/', timeout=3) as response:
        assert response.read() == b'network-fixture'
def udp():
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as client:
        client.settimeout(3)
        client.sendto(b'udp-fixture', ({ip:?},8000))
        assert client.recvfrom(1024)[0] == b'udp-fixture'
for operation in (http, udp):
    if unrestricted:
        operation()
    else:
        try:
            operation()
        except OSError:
            pass
        else:
            raise AssertionError('unlisted destination must remain blocked')
print('NETWORK_PROBE_OK', flush=True)
"#
    )
}

#[tokio::test]
async fn kubernetes_network_modes_coexist_for_workload_and_ssh() {
    let fixture = SupportContainer::start_python("unrestricted-egress-fixture", FIXTURE, 8000)
        .await
        .expect("start private TCP/UDP echo fixture on the test cluster network");
    let ip = fixture.ip().expect("get fixture network address");
    let mut policy = NamedTempFile::new().expect("create policy");
    policy.write_all(POLICY.as_bytes()).expect("write policy");
    policy.flush().expect("flush policy");
    let policy_path = policy.path().to_str().expect("UTF-8 policy path");
    let mut sandboxes = Vec::new();
    for unrestricted in [false, true] {
        let driver = if unrestricted {
            r#"{"kubernetes":{"unrestricted_network":true}}"#
        } else {
            r#"{"kubernetes":{"unrestricted_network":false}}"#
        };
        let script = probe(&ip, unrestricted);
        let entrypoint =
            format!("{script}\nimport time\nprint('MODE_READY', flush=True)\ntime.sleep(600)");
        let sandbox = SandboxGuard::create_keep_with_args(
            &[
                "--policy",
                policy_path,
                "--driver-config-json",
                driver,
                "--env",
                "HTTP_PROXY=http://192.0.2.1:9",
                "--env",
                "ALL_PROXY=socks5://192.0.2.1:9",
                "--env",
                "grpc_proxy=http://192.0.2.1:9",
            ],
            &["python3", "-c", &entrypoint],
            "MODE_READY",
        )
        .await
        .expect("create sandbox and verify entrypoint network mode");
        sandboxes.push((sandbox, unrestricted));
    }
    for (sandbox, unrestricted) in &sandboxes {
        let script = probe(&ip, *unrestricted);
        let output = sandbox
            .exec(&["python3", "-c", &script])
            .await
            .expect("verify independent SSH network modes while both sandboxes exist");
        assert!(output.contains("NETWORK_PROBE_OK"));
    }
}
