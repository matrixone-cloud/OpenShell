# OpenShell Sandbox Suspend/Resume — kind 手动测试指南

在本地 [kind](https://kind.sigs.k8s.io) 集群上手动端到端验证 Kubernetes sandbox 的 suspend（挂起）与 resume（恢复）功能。适用于在 `feat/kubernetes-suspend-resume` 分支上自测，或开 PR 前确认行为。

本目录相关文件：

| 文件 | 用途 |
|------|------|
| [`kind-config.yaml`](./kind-config.yaml) | kind 集群定义 |
| [`values-kind-local.yaml`](./values-kind-local.yaml) | 本地测试 Helm values 覆盖 |

自动化测试见 [`e2e/rust/tests/sandbox_suspend_resume.rs`](../rust/tests/sandbox_suspend_resume.rs)。

## suspend/resume 做什么

| 操作 | 行为 |
|------|------|
| **suspend** | 删除 sandbox **Pod**；保留 `Sandbox` CR 与 workspace **PVC** |
| **resume** | 挂载同一 PVC 创建**新 Pod**；`/sandbox` 下文件保留 |
| **不保留** | 运行中的进程、内存、打开的连接、`/sandbox` 以外的文件 |

suspend/resume 需要 **Kubernetes compute driver** 和 **Agent Sandbox `v1beta1`**（`spec.operatingMode`）。其他 compute driver 会返回 unsupported 错误。

## 是否需要安装 Agent Sandbox？

**需要。** OpenShell Kubernetes driver 会创建 `agents.x-k8s.io/v1beta1` 的 `Sandbox` 自定义资源。[Agent Sandbox](https://agent-sandbox.sigs.k8s.io) controller 负责把这些 CR 协调成 Pod 和 PVC。

- 未安装 Agent Sandbox → 创建 sandbox 失败（Kubernetes API 404）
- suspend/resume 需要 **`v1beta1`** 及 `spec.operatingMode`
- 请安装 **v0.5.0 及以上**（仓库默认 `AGENT_SANDBOX_VERSION=v0.5.0`）
- **v0.4.6（`v1alpha1`）** 支持 create/delete，**不支持** suspend/resume

原地升级 Agent Sandbox 后，需重启 OpenShell gateway，以便重新检测可用的 API 版本。

## 前置条件

kubectl / helm 使用 **`~/.kube/config`**。创建 kind 集群**之前**务必：

```shell
unset KUBECONFIG
```

若 `env | grep KUBE` 有输出，或 `mise activate` 已加载 shell，mise 会把 `KUBECONFIG` 指到仓库里的 `kubeconfig`，kind 就不会写入 `~/.kube/config`。

创建后确认 context 已生效：

```shell
kubectl config current-context
# 应为 kind-openshell-kind

kubectl get nodes
```

**若 `kubectl get nodes` 仍报 `localhost:8080 was refused`**：说明 kind 配置在仓库 `kubeconfig` 里，而 `~/.kube/config` 是空的。合并一次即可：

```shell
unset KUBECONFIG
KUBECONFIG=~/.kube/config:/Users/lr90/rust/OpenShell/kubeconfig \
  kubectl config view --flatten > /tmp/kubeconfig-merged
mv /tmp/kubeconfig-merged ~/.kube/config
kubectl config use-context kind-openshell-kind
kubectl get nodes
```

（路径 `/Users/lr90/rust/OpenShell` 改成你的 clone 路径。）

构建镜像、跑 Helm 等需要在**仓库根目录**执行：

```shell
cd /Users/lr90/rust/OpenShell   # 改成你的 clone 路径
```

常用变量（可选，方便复制）：

```shell
export KIND_CLUSTER_NAME=openshell-kind
export KUBE_CONTEXT="kind-${KIND_CLUSTER_NAME}"
```

安装工具：

```shell
mise install
```

单独安装 kind（mise 未提供）：

```shell
brew install kind
# 或: go install sigs.k8s.io/kind@v0.27.0
```

确认 Docker 已运行：

```shell
docker info
```

| 工具 | 用途 |
|------|------|
| Docker | kind 节点、镜像构建 |
| kind | 本地 Kubernetes 集群 |
| kubectl、helm | 部署与查看资源 |
| mise | 项目工具链 |
| 当前分支编译的 `openshell` CLI | `sandbox suspend` / `sandbox resume` |

## 1. 创建 kind 集群

`kind create` 在 **`KUBECONFIG` 未设置** 时会把集群写入 `~/.kube/config`。

```shell
cd /Users/lr90/rust/OpenShell   # 改成你的 clone 路径
export KIND_CLUSTER_NAME=openshell-kind

unset KUBECONFIG
kind create cluster --config e2e/kubernetes/kind-config.yaml
kubectl config use-context "kind-${KIND_CLUSTER_NAME}"
kubectl get nodes
```

## 2. 安装 Agent Sandbox（v0.5.0）

```shell
export AGENT_SANDBOX_VERSION=v0.5.0

kubectl apply -f \
  "https://github.com/kubernetes-sigs/agent-sandbox/releases/download/${AGENT_SANDBOX_VERSION}/manifest.yaml"

kubectl -n agent-sandbox-system rollout status deployment/agent-sandbox-controller --timeout=300s
kubectl get crd sandboxes.agents.x-k8s.io

# 确认 v1beta1 已 serve
kubectl api-resources --api-group=agents.x-k8s.io
```

上游 manifest 大致会创建以下资源（精确字段以 release manifest 为准）：

```yaml
---
apiVersion: v1
kind: Namespace
metadata:
  name: agent-sandbox-system
---
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: sandboxes.agents.x-k8s.io
spec:
  group: agents.x-k8s.io
  names:
    kind: Sandbox
    plural: sandboxes
  scope: Namespaced
  versions:
    - name: v1beta1
      served: true
      storage: true
      schema:
        openAPIV3Schema:
          type: object
          properties:
            spec:
              type: object
              properties:
                operatingMode:
                  type: string
                  enum: ["Running", "Suspended"]
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: agent-sandbox-controller
  namespace: agent-sandbox-system
spec:
  replicas: 1
  selector:
    matchLabels:
      app: agent-sandbox-controller
  template:
    metadata:
      labels:
        app: agent-sandbox-controller
    spec:
      containers:
        - name: controller
          image: registry.k8s.io/agent-sandbox/agent-sandbox-controller:v0.5.0
```

## 3. 构建镜像并导入 kind

从当前分支构建 gateway 和 supervisor，并预加载默认 sandbox 镜像，避免首次 create 时在集群内拉镜像耗时过长。

**新终端**只需确认 context（并 `unset KUBECONFIG` 若 mise 注入了别的路径）：

```shell
unset KUBECONFIG
export KIND_CLUSTER_NAME=openshell-kind

kind get clusters
# 应列出 openshell-kind

cd /Users/lr90/rust/OpenShell
export IMAGE_TAG=local-suspend-resume
export REGISTRY=openshell

CONTAINER_ENGINE=docker IMAGE_REGISTRY="$REGISTRY" IMAGE_TAG="$IMAGE_TAG" \
  bash tasks/scripts/docker-build-image.sh gateway
CONTAINER_ENGINE=docker IMAGE_REGISTRY="$REGISTRY" IMAGE_TAG="$IMAGE_TAG" \
  bash tasks/scripts/docker-build-image.sh supervisor

kind load docker-image "${REGISTRY}/gateway:${IMAGE_TAG}" --name "$KIND_CLUSTER_NAME"
kind load docker-image "${REGISTRY}/supervisor:${IMAGE_TAG}" --name "$KIND_CLUSTER_NAME"

SANDBOX_IMAGE=ghcr.io/nvidia/openshell-community/sandboxes/base:latest
docker pull "$SANDBOX_IMAGE"
kind load docker-image "$SANDBOX_IMAGE" --name "$KIND_CLUSTER_NAME"
```

kind 节点**不会**共享宿主机 Docker 镜像缓存。chart 或 sandbox Pod 引用的每个镜像都必须通过 `kind load docker-image` 导入，或从集群内可访问的 registry 拉取。

## 4. 用 Helm 部署 OpenShell

```shell
cd /Users/lr90/rust/OpenShell
kubectl config use-context "kind-${KIND_CLUSTER_NAME}"

kubectl create namespace openshell

helm upgrade --install openshell deploy/helm/openshell \
  --kube-context "kind-${KIND_CLUSTER_NAME}" \
  --namespace openshell \
  --values deploy/helm/openshell/ci/values-skaffold.yaml \
  --values e2e/kubernetes/values-kind-local.yaml \
  --set fullnameOverride=openshell \
  --set "image.repository=${REGISTRY}/gateway" \
  --set "image.tag=${IMAGE_TAG}" \
  --set "image.pullPolicy=IfNotPresent" \
  --set "supervisor.image.repository=${REGISTRY}/supervisor" \
  --set "supervisor.image.tag=${IMAGE_TAG}" \
  --wait --timeout 5m

kubectl -n openshell rollout status statefulset/openshell --timeout=300s
kubectl -n openshell get pods,svc
```

Helm chart 会自动选择 Kubernetes compute driver。`server.grpcEndpoint` 为空时，chart 会从 release Service 推导集群内回调 URL（`server.disableTls: true` 时为明文 HTTP）。

## 5. 编译 CLI 并连接 gateway

```shell
cargo build -p openshell-cli
export PATH="$PWD/target/debug:$PATH"
```

另开终端 port-forward（使用 `~/.kube/config`，无需 `export KUBECONFIG`）：

```shell
unset KUBECONFIG
kubectl config use-context kind-openshell-kind
kubectl cluster-info
kubectl -n openshell get svc openshell

kubectl -n openshell port-forward svc/openshell 8090:8080
```

保持该终端运行；在**另一个终端**注册 gateway：

```shell
openshell gateway add http://127.0.0.1:8090 --local --name kind-local
openshell status
```

## 6. 手动测试 suspend/resume

### 6.1 创建 sandbox 并写入 workspace 标记

```shell
openshell sandbox create --name suspend-test -- \
  sh -lc 'printf suspend-resume-marker > /sandbox/suspend-resume-marker; echo marker-written; sleep infinity'

openshell sandbox get suspend-test
# Phase 应为 Ready
```

### 6.2 suspend 前记录 Kubernetes 状态

OpenShell / Agent Sandbox 的 Pod **没有** `agents.x-k8s.io/sandbox-name` 这个 label（文档旧写法有误）。与 [`sandbox_suspend_resume.rs`](../rust/tests/sandbox_suspend_resume.rs) 一致，用 **Sandbox UID 的 ownerReference** 找 Pod：

```shell
export SANDBOX_NAME=suspend-test

kubectl -n openshell get sandbox "$SANDBOX_NAME" -o yaml > /tmp/sandbox-before.yaml

export SANDBOX_UID=$(kubectl -n openshell get sandbox "$SANDBOX_NAME" \
  -o jsonpath='{.metadata.uid}')

# 按 ownerReference 找 Pod（需要 jq：brew install jq）
export OLD_POD=$(kubectl -n openshell get pods -o json | jq -r --arg uid "$SANDBOX_UID" \
  '.items[] | select([.metadata.ownerReferences[]?.uid] | index($uid)) | .metadata.name')

# 若无 jq，可直接看 Sandbox status 里的 agentPod 字段：
# export OLD_POD=$(kubectl -n openshell get sandbox "$SANDBOX_NAME" -o jsonpath='{.status.agentPod}')

export OLD_POD_UID=$(kubectl -n openshell get pod "$OLD_POD" \
  -o jsonpath='{.metadata.uid}')

echo "Sandbox UID: $SANDBOX_UID"
echo "Pod: $OLD_POD (uid=$OLD_POD_UID)"

kubectl -n openshell get pvc -o wide
```

### 6.3 Suspend

```shell
openshell sandbox suspend suspend-test

openshell sandbox get suspend-test
# Phase: Suspended

kubectl -n openshell get sandbox "$SANDBOX_NAME" \
  -o jsonpath='{.spec.operatingMode}{"\n"}'
# 期望: Suspended

kubectl -n openshell get sandbox "$SANDBOX_NAME" \
  -o jsonpath='{range .status.conditions[*]}{.type}={.status}{"\n"}{end}'
# 期望: Suspended=True

kubectl -n openshell get pod "$OLD_POD"
# 期望: NotFound

kubectl -n openshell get pvc
# PVC 仍在；ownerReferences 仍指向同一 Sandbox UID
```

suspend 后的 `Sandbox` CR 大致形态：

```yaml
apiVersion: agents.x-k8s.io/v1beta1
kind: Sandbox
metadata:
  name: suspend-test
  namespace: openshell
  uid: "<unchanged-sandbox-uid>"
spec:
  operatingMode: Suspended
status:
  conditions:
    - type: Suspended
      status: "True"
```

### 6.4 Resume

```shell
openshell sandbox resume suspend-test
# CLI 会等待 Phase 回到 Ready

openshell sandbox get suspend-test

kubectl -n openshell get sandbox "$SANDBOX_NAME" \
  -o jsonpath='{.spec.operatingMode}{"\n"}'
# 期望: Running

export NEW_POD=$(kubectl -n openshell get pods -o json | jq -r --arg uid "$SANDBOX_UID" \
  '.items[] | select([.metadata.ownerReferences[]?.uid] | index($uid)) | .metadata.name')
# 或: export NEW_POD=$(kubectl -n openshell get sandbox "$SANDBOX_NAME" -o jsonpath='{.status.agentPod}')

export NEW_POD_UID=$(kubectl -n openshell get pod "$NEW_POD" \
  -o jsonpath='{.metadata.uid}')

echo "New Pod: $NEW_POD (uid=$NEW_POD_UID)"

# 如何判断（须在 suspend 前保存 OLD_POD_UID 才能对比 Pod UID）：
# - Sandbox CR 的 metadata.uid（$SANDBOX_UID）→ 应不变
# - Pod 的 metadata.uid → resume 后应是新值（与 OLD_POD_UID 不同）
# - suspend 后旧 Pod 已删除，若没提前保存 OLD_POD_UID，只能看：mode=Running、有新 Pod、marker 仍在

[ "$(kubectl -n openshell get sandbox "$SANDBOX_NAME" -o jsonpath='{.metadata.uid}')" = "$SANDBOX_UID" ] \
  && echo "Sandbox UID 未变 ✓" || echo "Sandbox UID 变了 ✗"

if [ -n "${OLD_POD_UID:-}" ]; then
  [ "$NEW_POD_UID" != "$OLD_POD_UID" ] \
    && echo "Pod UID 已换新 ✓" || echo "Pod UID 相同 ✗"
fi
```

### 6.5 验证 workspace 持久化

```shell
openshell sandbox exec -n suspend-test -- cat /sandbox/suspend-resume-marker
# 或（已设为 last-used sandbox 时）：openshell sandbox exec -- cat /sandbox/suspend-resume-marker
# 期望输出: suspend-resume-marker
```

### 6.6 清理

```shell
openshell sandbox delete suspend-test
kubectl -n openshell get sandbox,pvc,pods
```

## 7. Kubernetes 资源形态参考

OpenShell 通过 Kubernetes driver 创建 `Sandbox` CR。运行中的 sandbox 大致如下（精确字段取决于 chart values 与集群版本）：

```yaml
apiVersion: agents.x-k8s.io/v1beta1
kind: Sandbox
metadata:
  name: suspend-test
  namespace: openshell
  labels:
    app.kubernetes.io/managed-by: openshell
spec:
  operatingMode: Running
  podTemplate:
    spec:
      serviceAccountName: openshell-sandbox
      automountServiceAccountToken: false
      initContainers:
        - name: workspace-seed
          # 首次使用 PVC 时从镜像 seed /sandbox
      containers:
        - name: agent
          image: ghcr.io/nvidia/openshell-community/sandboxes/base:latest
          volumeMounts:
            - name: workspace
              mountPath: /sandbox
      volumes:
        - name: workspace
          persistentVolumeClaim:
            claimName: <pvc-owned-by-sandbox>
  volumeClaimTemplates:
    - metadata:
        name: workspace
      spec:
        accessModes: ["ReadWriteOnce"]
        resources:
          requests:
            storage: 10Gi
```

Workspace PVC（由 Sandbox CR 拥有）：

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: workspace-suspend-test-xxxxx
  namespace: openshell
  ownerReferences:
    - apiVersion: agents.x-k8s.io/v1beta1
      kind: Sandbox
      name: suspend-test
      uid: "<sandbox-uid>"
      controller: true
      blockOwnerDeletion: true
spec:
  accessModes:
    - ReadWriteOnce
  resources:
    requests:
      storage: 10Gi
status:
  phase: Bound
```

## 8. 可选：运行自动化 e2e 测试

复用同一 kind 集群与镜像构建，只跑 suspend/resume 测试：

```shell
export OPENSHELL_E2E_KUBE_CONTEXT="kind-${KIND_CLUSTER_NAME}"
export AGENT_SANDBOX_VERSION=v0.5.0
export OPENSHELL_E2E_KUBE_BUILD_IMAGES=1
export OPENSHELL_REGISTRY="$REGISTRY"
export IMAGE_TAG="$IMAGE_TAG"

OPENSHELL_E2E_KUBE_TEST=kubernetes_sandbox_suspend_resume_preserves_workspace \
  mise run e2e:kubernetes
```

设置 `OPENSHELL_E2E_KUBE_CONTEXT` 后，`e2e/with-kube-gateway.sh` 会安装 Agent Sandbox、部署 chart、port-forward 并运行测试。若使用本地构建的镜像，运行前需先完成步骤 3 的 `kind load`。

## 9. 故障排查

| 现象 | 可能原因 | 处理 |
|------|----------|------|
| `sandbox suspend` 报 unsupported | Agent Sandbox 仅提供 `v1alpha1` | 升级到 v0.5.0；重启 gateway |
| 没有 `Sandbox` CR | 未安装 Agent Sandbox | 重做步骤 2 |
| Pod 一直 Pending | 镜像未导入 kind | `kind load docker-image ...` |
| `no nodes found for cluster ""` | `$KIND_CLUSTER_NAME` 未设置 | `export KIND_CLUSTER_NAME=openshell-kind` |
| `connection to the server localhost:8080 was refused` | `~/.kube/config` 为空或没有 current-context；kind 配置在仓库 `kubeconfig` | 见前置条件里的「合并 kubeconfig」；或 `kubectl config use-context kind-openshell-kind` |
| suspend 后 Pod 仍在 | controller 未就绪或版本不对 | 查 `kubectl -n agent-sandbox-system logs deploy/agent-sandbox-controller` |
| resume 后 marker 丢失 | 数据写在 `/sandbox` 外 | 测试文件必须写在 PVC 挂载路径下 |
| jsonpath `array index out of bounds` | 用不存在的 label 查 Pod | 用 `jq` + `ownerReferences` 或 `{.status.agentPod}`，见 6.2 / 6.4 |
| `ImagePullBackOff` | `Always` 拉取但集群无外网 | 使用 `IfNotPresent` 并预加载镜像 |

常用命令：

```shell
kubectl -n agent-sandbox-system get pods
kubectl -n openshell get sandbox,pvc,pods -o wide
kubectl -n openshell describe sandbox suspend-test
kubectl -n openshell logs -l app.kubernetes.io/instance=openshell --tail=100
openshell logs suspend-test
```

## 10. 拆除环境

```shell
unset KUBECONFIG
kubectl config use-context kind-openshell-kind

helm uninstall openshell -n openshell
kubectl delete namespace openshell

kubectl delete -f \
  "https://github.com/kubernetes-sigs/agent-sandbox/releases/download/v0.5.0/manifest.yaml"

kind delete cluster --name openshell-kind
```

## 与 k3d 开发路径对比

仓库默认本地 Kubernetes 流程使用 **k3d**（`mise run helm:k3s:create`），会自动安装 Agent Sandbox v0.5.0。本指南使用 **kind**，因为 CI 与部分开发者更习惯 kind。suspend/resume 行为相同，差异仅在集群创建与镜像导入方式。

k3d 上基于 Skaffold 的迭代开发，见 [`helm-dev-environment`](../../.agents/skills/helm-dev-environment/SKILL.md) skill。
