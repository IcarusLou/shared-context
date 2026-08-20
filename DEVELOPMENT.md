# 开发约定

## 当前实现边界

仓库当前完成的是 **M1：Task-first 领域与入口基础**：

- `TaskIntent` 不包含 Space 路由；Task 与 Space 的相关性由独立的 `TaskSpaceAssociation` 表达，并允许 `0..N` 个结果。
- 不存在 Workspace-to-Space 绑定类型、全局 Active Space、对应配置或绑定命令。
- 显式 `context_search` 可以用 `space_ids` 做硬过滤；`context_for_task` 不接受 Space 路由，Search 排序也没有 Space 偏好分支。
- `WorkEpisode` 和无 Space 的 `ContextCandidate` 领域类型已经存在。
- `candidate_create` 是当前 Candidate 写入主入口；CLI 与 MCP 都生成服务端 ID，且未确认 Candidate 不参与自动注入。
- 既有 Git Writer、事件校验、SQLite 投影、Context 生命周期、CLI/MCP、Agent Adapter、安装器和 NPM 分发能力继续作为 M1 的基础设施。

以下能力**尚未实现**，不得在代码、测试报告或评审中宣称已经具备：

- **M2：未实现** — TaskSession/runtime.sqlite、TaskIntent revision、动态 Task Signal、多 Space 推断、Space Intent 检索和可解释 TaskContextPack。
- **M3：未实现** — Engineering Graph、Repository/File/Symbol/API/Schema/Test 关联、重新解析与关系扩展。
- **M4：未实现** — WorkEpisode 自动聚合、AgentCheckpoint、Candidate Builder、去重/冲突/Space 推荐和 Candidate confirm/list/discard。

M1 的手工 `candidate_create` 是领域和安全边界的可执行入口，不等同于 M4 的 Low-tax Capture。

## 环境与检查

需要 Rust stable（最低 Rust 1.85）、`rustfmt`、`clippy`、Git，以及 Node.js 18 或更新版本。

M1 提交门禁：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
(cd npm && npm test)
```

定向执行 M1 跨 crate 验收：

```bash
cargo test --locked -p sctx-cli --test milestone_one_contract
cargo test --locked -p sctx-mcp --test mcp_contract
```

提交 `Cargo.lock`，确保 CLI workspace 的本地与 CI 构建使用相同依赖解析结果。

## Crate 依赖方向

依赖只能从表格下方向上方流动；`cli` 是组合入口，`domain` 不依赖其他 workspace crate。

| 层级 | Crate | 允许的 workspace 依赖 |
|---:|---|---|
| 0 | `domain` | 无 |
| 1 | `local-state`, `event-schema` | `domain` |
| 2 | `git-store` | `domain`, `event-schema`, `local-state` |
| 3 | `index` | `domain`, `event-schema`, `git-store` |
| 4 | `search` | `domain`, `index` |
| 5 | `mcp`, `agent-adapter` | M1 已声明的更低层 crate |
| 6 | `adapter-cursor`, `adapter-codex` | `domain`, `agent-adapter` |
| 7 | `installer` | Adapter、Store、Index、Search 等更低层 crate |
| 8 | `cli` | 所有库 crate |

跨 crate 的可恢复错误统一使用 `sctx_domain::Error` 和 `sctx_domain::Result`。新增依赖时必须保持以上有向无环结构，并同步更新 workspace 契约测试。

## Fixtures 与 Schema

- `fixtures/` 保存跨 crate 的只读测试输入，不在测试中原地改写。
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

实现必须遵守 [`technical-design.md`](./technical-design.md) 的核心不变量。M1 契约尤其禁止重新引入 Workspace 路由、Task 请求中的 Space 路由、Space 偏好排序，或让未确认 Candidate 进入自动注入。
