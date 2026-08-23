# 开发约定

## 当前实现边界

仓库已完成 **M1：Task-first 领域与入口基础**、**M2：Task Runtime 与多 Space Retrieval** 和 **M3：Engineering Graph**：

- `WorkingIntentSnapshot` 是当前 Task 的非事实工作理解，不包含 Space 路由；每次不可变版本由 `TaskIntentRevision` 表达。Task 与 Space 的相关性由独立的 `TaskSpaceAssociation` 表达，并允许 `0..N` 个结果。
- 不存在 Workspace-to-Space 绑定类型、全局 Active Space、对应配置或绑定命令。
- 显式 `context_search` 可以用 `space_ids` 做硬过滤；只读 `task_context` 只接受 external Session locator、token budget 和 max spaces，不接受 Intent、Signals 或任何 Space/Workspace 路由。
- `task_intent_update` 是 Working Intent 的唯一公开写入口并返回更新后的 TaskContextPack；`task_signal_supersede` 是 Signal 失效入口。
- `TaskContextPack` 同时报告当前 Context Tree/Projection Generation 与可选的历史 Graph Context Tree/Artifact Generation；二者允许不同，且每个 Context 都链接到 Association、typed RetrievalPath 和明确 safety source。
- 同一 Workspace 下的 external Session 只持有各自的 TaskSession、`TaskIntentRevision` 链与非定位 TaskSignals。Graph 查询通过本次请求的 `ResolvedFocus` 精确匹配显式 Graph build 中的 RepositoryId + 完整 ArtifactLocator；Focus 不进入 Runtime，也不跨请求、重启、Task 切换或压缩恢复。
- Workspace 不形成 Space prior。`config.toml` 中的显式本机 Repository Catalog 是 RepositoryId 的唯一权威，一个 ID 可包含 `0..N` 个 canonical checkout/worktree；路径、basename、remote、Git common-dir 或共同父目录都不创建或合并身份。
- PostTool Hook 不运行 Git、Scanner、Graph rebuild，也不发起 ArtifactFocusQuery。File observation 只形成 Breadcrumb；可识别 Test/Check/Lint 执行形成非事实、非定位的 TestOutcome TaskSignal。TaskSignal 可影响 Working Intent retrieval，但不是工程 Evidence；setup、doctor 与显式 Runtime open 才验证 Catalog。
- Engineering Graph 是围绕已有 Context/EngineeringReference 的稀疏历史知识快照，不是全仓代码搜索引擎或当前 Context Store 镜像。Builder 只固化 Reference roots 与最多两跳的 build-time ContextRelation closure；`association_rebuild` 按 RepositoryId 分组并去重 Reference 的精确 repo-relative path，Scanner 只读取该有界计划。
- Graph Builder 固化 immutable ContextId+RevisionId、关系 target Revision 和 build-time safety；未接受、Evidence 不完整或冲突 Revision 永不自动注入。`context_tree_oid` 只解释构建来源，Tree mismatch、新 Revision、Withdraw 或无关 append 都不关闭或隐式重建旧 Graph。
- 公开只读 `task_artifact_focus` 接受 Session locator、Intent CAS、absolute path 和无 path 的 kind-specific coordinates；服务端钉定 immutable ActiveTask snapshot，经 Catalog 补全本次 `ResolvedFocus` 并直接返回即时 Pack。它不创建 ID、生命周期、Intent Revision 或 Runtime 记录；missing declared tail 可安全映射，exact Graph 不可达则返回 budgeted `artifact_not_reachable_in_graph`，不 scan/rebuild、不猜测。
- 受限多语言 Scanner、持久 Engineering Reference、kind-specific 确定性 ArtifactLocator、可重建解析投影、ContextRelation 1–2 跳和 Graph RetrievalPath 已通过固定 oracle 与显式 MCP/CLI 工作流验收；move/rename 直接变为 missing，不执行关联猜测。新建或其他 untracked 文件不进入 Graph（#150 的已确认边界），Task query 也不触发 Repository scan。
- `runtime.sqlite` 已持久化 server-owned、TaskSession/Task-owned、version-CAS 的 `WorkEpisode`、ordered Intent/Signal refs、normalized `WorkObservation`、Capture ingestion 与 safe diagnostics；显式 Runtime API 提供 open/read/list/advance/append/ingest/close-prepare/source verification，一 TaskSession 最多一个 Open Episode。
- 公开 `task_checkpoint` MCP/CLI 以 Task、`TaskIntentRevision` 和 Episode version CAS 显式写入完整 Claims/Unknowns；Runtime v8 同事务 open/advance Episode、生成 inline Validation Observation 与 Claim/Checkpoint ID，并按 Episode+parent version+完整语义幂等 continue/close。ArtifactRef 与 TaskSignal 都不独立构成 Evidence；被 Claim 显式引用的 owned Diff/TestOutcome 仅作为来源线索，由 Builder 转换为带 supports、interpretation 和 limitations 的自包含 EvidenceSnapshot。`continue` 不构建 Candidate，`close` 在 Checkpoint 成功或语义重试后触发确定性 Candidate Builder。
- `CaptureStore` 使用 typed `CaptureId` 保存带 ExternalSessionLocator 与 optional exact ActiveTask owner 的 redacted TTL Breadcrumb；bounded read/list/claim/cleanup、claim 与 Runtime commit 双重幂等、Catalog File→ArtifactRef 映射已实现。无 ActiveTask 或未配置/unsafe File 只保留 typed diagnostic，绝不猜 owner/Repository。
- Hook 只写 owned/diagnostic Capture 和原有非定位 TestOutcome，不 open/ingest Episode 或伪造 Claim。工作 Agent 在 PreCompact/TurnStop 前显式写入完整 current-Intent Checkpoint；verified Hook 只在该 Checkpoint 已存在时补齐 ordered Intent/Signal refs、关闭 Episode 并调用共享 Candidate Builder。重复/并发事件复用同一 Episode/Build/Candidate；SessionEnd 只做本地 TTL 清理。若 `continue` 已成功而 Hook 缺失/失败，`task_checkpoint boundary=close` 加当前 Episode version 与空 Claims/Unknowns 可在同一 CAS 边界关闭已有 Checkpoint，不要求重填或伪造内容。
- Candidate Builder 读取 exact closed Episode、final/相关 Checkpoint 和一个 Index snapshot，逐 Claim 组装最小充分 Evidence；Runtime 在任何 Git 写入前固化 BuildId/SubmissionId/content hash，#117 返回的 CandidateId/EventId 再原子回填。无 Claim、Unknown-only 或 Evidence 不充分均为零 Candidate；无 kind hint固定降级 Discovery。
- #159 Candidate Analysis 是 Runtime derived review state：Search 以完整草稿等值、显式 related Context、exact Artifact Graph、topic/scope 与 BM25 多路 RRF 生成 typed assessment，明确区分 exact duplicate、supports、revises、potential contradiction、unresolved related 和 novel。Space 推荐融合 assessment target、source Task association 与 Space Intent；冲突 Intent/unsafe Context 不自动成为 Primary，无安全 Primary 时给出完整 system-suggested Space Intent。分析不写 Git、不进入 Search/Hook/自动注入；`candidate analyze` 可重跑并替换当前结果。
- #161 已定义 CandidateConfirmation 与 ContextSpaceAssociation 的严格 Event/Reducer/Index 事实：确认引用 exact Candidate/source、Primary/Related、结果 Revision、initial Association、causal Publish Event 和 final content hash；后续 Withdraw 不反向抹除历史确认。Association 独立成可修订 DAG，多 Head 与重复 Confirmation 都显式 conflict；当前嵌套 Context owner 与 Search ranking 不变。
- 无 Space 的 `ContextCandidate` 领域类型已经存在，Builder Candidate 仍不可自动注入。
- `candidate_create` 是当前 Candidate 写入主入口；CLI 与 MCP 要求调用方提供稳定 `submission_id` 与精确 ActiveTask/Intent/closed WorkEpisode 所有权，服务端生成 Candidate/Event 身份和路径，且未确认 Candidate 不参与自动注入。
- 既有 Git Writer、事件校验、SQLite 投影、Context 生命周期、CLI/MCP、Agent Adapter、安装器和 NPM 分发能力继续作为 M1 的基础设施。

以下能力**尚未实现**，不得在代码、测试报告或评审中宣称已经具备：

- **M4 最终验收：未完成** — #117、#136、#156–#163 与 #169 已完成轻量 Working Intent、语义幂等、Hint 文本召回、可验证 WorkEpisode/Capture、Hook lifecycle、Candidate Builder/Review/Confirm 全链路；#164 最终端到端验收尚未执行。
- **团队同步：未实现** — Repository Catalog 是单机显式配置，不是团队事实或知识 Store。

Cursor 与 Codex 都通过显式 `task_intent_update` 建立 TaskSession 的首个 `TaskIntentRevision`；Prompt Hook 只提供能力提示。已有 ActiveTask 可通过只读 `task_context` 再取 Pack。

Mandatory Gate #114/#117 已完成：稳定 `submission_id` 写入 Git Event 并可重建到 SQLite submission/conflict 索引；Candidate lock 覆盖索引同步、lookup、pending recovery 和 append，主写路径不扫描全量 Event，也不依赖 commit subject。known-v1 malformed Candidate 只有在可安全提取合法 SubmissionId 时形成 submission-local conflict；unknown schema、无 hint 或其他 submission 的坏 Event 只保留 diagnostic。`candidate_create` 同时使用 #156 的 Runtime existence/owner/status 与 Task/Intent CAS 验证 source Episode。

M1 的手工 `candidate_create` 仍是独立领域和安全入口；M4 Builder 复用其 #117 admission，但不替代后续确认和知识聚合。

## 环境与检查

需要 Rust stable（最低 Rust 1.85）、`rustfmt`、`clippy`、Git，以及 Node.js 18 或更新版本。

提交门禁：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
(cd npm && npm test)
```

定向执行里程碑验收：

```bash
cargo test --locked -p sctx-cli --test milestone_one_contract
cargo test --locked -p sctx-mcp --test mcp_contract
cargo test --locked -p sctx-cli --test milestone_two_contract
cargo test --locked -p sctx-cli --test milestone_three_contract
```

提交 `Cargo.lock`，确保 CLI workspace 的本地与 CI 构建使用相同依赖解析结果。

## Crate 依赖方向

依赖只能从表格下方向上方流动；`cli` 是组合入口，`domain` 不依赖其他 workspace crate。

| 层级 | Crate | 允许的 workspace 依赖 |
|---:|---|---|
| 0 | `domain` | 无 |
| 1 | `local-state`, `event-schema`, `task-runtime` | `domain` |
| 2 | `git-store` | `domain`, `event-schema`, `local-state` |
| 3 | `index` | `domain`, `event-schema`, `git-store` |
| 4 | `search` | `domain`, `engineering-graph`, `index` |
| 5 | `mcp`, `agent-adapter` | M1 已声明的更低层 crate |
| 6 | `adapter-cursor`, `adapter-codex` | `domain`, `agent-adapter` |
| 7 | `installer` | Adapter、Store、Index、Search 等更低层 crate |
| 8 | `cli` | 所有库 crate |

跨 crate 的可恢复错误统一使用 `sctx_domain::Error` 和 `sctx_domain::Result`。新增依赖时必须保持以上有向无环结构，并同步更新 workspace 契约测试。

## Fixtures 与 Schema

- `fixtures/` 与 `tests/fixtures/` 保存跨 crate 的只读测试输入，不在测试中原地改写；`tests/oracles/` 保存手工编写的固定 expected，不得从生产输出反算。
- `schemas/` 保存随源码版本控制的 Schema。
- `npm/` 保存 NPM launcher、平台包和离线 Bundle 源码。

这些目录不得被 `.gitignore` 整体排除；构建产物应使用精确到产物目录的忽略规则。

## NPM 打包检查

NPM 层没有第三方 JavaScript 运行时依赖：

```bash
cd npm
npm test
```

macOS 测试会为 `aarch64-apple-darwin` 和 `x86_64-apple-darwin` 构建真实 CLI，因此两种 Rust target 都必须安装。arm64 主机执行当前架构二进制的 launcher、offline install 和 setup smoke；没有 Intel 主机时，x64 只证明 Mach-O、签名、包内容、`os/cpu`、离线 lock 和 checksum 契约，不构成原生执行证据。

本地构建与离线安装说明见 [`npm/README.md`](./npm/README.md)。构建脚本只写本地产物，不执行 publish 或 upload。

## 设计不变量

实现必须遵守 [`technical-design.md`](./technical-design.md) 的核心不变量。M1–M3 契约尤其禁止重新引入 Workspace 路由、Task 请求中的 Space 路由、Space 偏好排序、裸 query 自动注入、文本 TaskSignal 冒充 Graph edge，或让未确认 Candidate 进入自动注入。
