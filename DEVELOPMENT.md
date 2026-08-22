# 开发约定

## 当前实现边界

仓库已完成 **M1：Task-first 领域与入口基础**、**M2：Task Runtime 与多 Space Retrieval** 和 **M3：Engineering Graph**：

- `TaskIntent` 不包含 Space 路由；Task 与 Space 的相关性由独立的 `TaskSpaceAssociation` 表达，并允许 `0..N` 个结果。
- 不存在 Workspace-to-Space 绑定类型、全局 Active Space、对应配置或绑定命令。
- 显式 `context_search` 可以用 `space_ids` 做硬过滤；只读 `task_context` 只接受 external Session locator、token budget 和 max spaces，不接受 Intent、Signals 或任何 Space/Workspace 路由。
- `task_intent_update` 是 Task Intent 的唯一写入口并返回更新后的 TaskContextPack；`task_signal_supersede` 是 Signal 失效入口。
- `TaskContextPack` 同时报告当前 Context Tree/Projection Generation 与可选的历史 Graph Context Tree/Artifact Generation；二者允许不同，且每个 Context 都链接到 Association、typed RetrievalPath 和明确 safety source。
- 同一 Workspace 下的 external Session 只持有各自的 TaskIntent 与非定位 TaskSignals。Graph 查询通过本次请求的 `ResolvedFocus` 精确匹配显式 Graph build 中的 RepositoryId + 完整 ArtifactLocator；Focus 不进入 Runtime，也不跨请求、重启、Task 切换或压缩恢复。
- Workspace 不形成 Space prior。`config.toml` 中的显式本机 Repository Catalog 是 RepositoryId 的唯一权威，一个 ID 可包含 `0..N` 个 canonical checkout/worktree；路径、basename、remote、Git common-dir 或共同父目录都不创建或合并身份。
- PostTool Hook 不运行 Git、Scanner、Graph rebuild，也不发起 ArtifactFocusQuery。File observation 只形成 Breadcrumb；可识别 Test/Check/Lint 执行形成非定位 TestOutcome。setup、doctor 与显式 Runtime open 才验证 Catalog。
- Engineering Graph 是围绕已有 Context/EngineeringReference 的稀疏历史知识快照，不是全仓代码搜索引擎或当前 Context Store 镜像。Builder 只固化 Reference roots 与最多两跳的 build-time ContextRelation closure；`association_rebuild` 按 RepositoryId 分组并去重 Reference 的精确 repo-relative path，Scanner 只读取该有界计划。
- Graph Builder 固化 immutable ContextId+RevisionId、关系 target Revision 和 build-time safety；未接受、Evidence 不完整或冲突 Revision 永不自动注入。`context_tree_oid` 只解释构建来源，Tree mismatch、新 Revision、Withdraw 或无关 append 都不关闭或隐式重建旧 Graph。
- 公开只读 `task_artifact_focus` 接受 Session locator、Intent CAS、absolute path 和无 path 的 kind-specific coordinates；服务端钉定 immutable ActiveTask snapshot，经 Catalog 补全本次 `ResolvedFocus` 并直接返回即时 Pack。它不创建 ID、生命周期、Intent Revision 或 Runtime 记录；missing declared tail 可安全映射，exact Graph 不可达则返回 budgeted `artifact_not_reachable_in_graph`，不 scan/rebuild、不猜测。
- 受限多语言 Scanner、持久 Engineering Reference、kind-specific 确定性 ArtifactLocator、可重建解析投影、ContextRelation 1–2 跳和 Graph RetrievalPath 已通过固定 oracle 与显式 MCP/CLI 工作流验收；move/rename 直接变为 missing，不执行关联猜测。新建或其他 untracked 文件不进入 Graph（#150 的已确认边界），Task query 也不触发 Repository scan。
- `WorkEpisode` 和无 Space 的 `ContextCandidate` 领域类型已经存在。
- `candidate_create` 是当前 Candidate 写入主入口；CLI 与 MCP 都生成服务端 ID，且未确认 Candidate 不参与自动注入。
- 既有 Git Writer、事件校验、SQLite 投影、Context 生命周期、CLI/MCP、Agent Adapter、安装器和 NPM 分发能力继续作为 M1 的基础设施。

以下能力**尚未实现**，不得在代码、测试报告或评审中宣称已经具备：

- **M4：未实现** — WorkEpisode 自动聚合、AgentCheckpoint、Candidate Builder、去重/冲突/Space 推荐和 Candidate confirm/list/discard。
- **团队同步：未实现** — Repository Catalog 是单机显式配置，不是团队事实或知识 Store。

Cursor 与 Codex 都通过显式 `task_intent_update` 建立权威 Task；Prompt Hook 只提供能力提示。已有 ActiveTask 可通过只读 `task_context` 再取 Pack。

M4 必须通过 Mandatory Gate #114/#117：以写入 Git Event、可由 Git 重建到 SQLite 唯一索引的稳定 `submission_id` 实现 Candidate 创建幂等，避免扫描全量 Event、依赖 commit subject，或被无关坏 Event 阻断。M4 还必须让 `source_episode_id` 可验证；M2 不提前实现这些 Capture 责任。

M1 的手工 `candidate_create` 是领域和安全边界的可执行入口，不等同于 M4 的 Low-tax Capture。

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
