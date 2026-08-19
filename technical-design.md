# 团队共享 Context 技术方案

## 1. 文档信息

- 状态：首版技术方案基线
- 目标平台：macOS（Apple Silicon / Intel）
- 分发方式：Rust 原生二进制 + NPM 包
- 首版 Agent：Cursor、Codex
- 后续 Agent：Claude Code
- 配套产品目标：[团队共享 Context：预期效果](./readme.md)

## 2. 背景与目标

本项目希望把个人或 Agent 在工程过程中形成的有效理解、判断和验证，沉淀为可以由团队成员和后续 Agent 继承的工程 Context，从而降低跨人员、跨端、跨 Agent、跨 Session 的冷启动成本。

系统要同时完成两件事：

```text
Capture
  将已验证、可复用的工程认知沉淀为 Candidate Context

Retrieve
  根据当前工作目标和工作区，在合适时机返回相关 Context
```

首版重点解决：

1. 使用 Git 保存可审计、可迁移的最终知识文件。
2. 使用 SQLite 在本地建立可删除、可重建的查询投影。
3. 使用 Cursor/Codex Hook 和 MCP 降低 Capture、Retrieve 的额外操作成本。
4. 通过生成阶段的追加保护，尽量避免多人或多 Agent 修改已有知识文件。
5. 不依赖外部需求系统、开发期 Git Commit 或其他容易失效的数据维持领域正确性。
6. 提供无需远端服务、无需管理员权限的友好安装和测试流程。

## 3. 约束与非目标

### 3.1 已确认约束

- 每次安装只维护一个 Shared Context Git 仓库。
- 所有 Requirement/ContextSpace 都是该仓库内的逻辑聚合。
- 产品配置、Runtime、SQLite、日志等文件放在当前用户目录。
- Git 是共享 Context 的最终载体。
- SQLite 仅作为本地查询投影，不能成为第二事实源。
- 面向项目使用者和 Agent 的写入接口只生成新文件，不提供修改、删除或重命名已有事件的能力。
- “项目使用者”是产品用户概念，不代表操作系统权限或 RBAC 角色。
- 首版不依赖外部 Requirement、Issue、PR 或文档系统。
- 首版只支持 macOS，通过 NPM 分发 Rust 二进制。
- 首版接入 Cursor 和 Codex，Adapter 边界需要支持后续接入 Claude Code。

### 3.2 首版非目标

- 不实现本地防恶意篡改、专用系统账号、root Helper 或权限隔离。
- 不实现 Contributor、Reviewer、Maintainer 等访问控制角色。
- 不把完整 Conversation、Transcript 或原始 Tool Output 提交到 Git。
- 不依赖远端 Git、PR、Merge Queue 或服务端检查完成首版验收。
- 不实现独立 GUI 或 IDE Extension。
- 不引入向量数据库或本地 Embedding 模型。
- 不承诺对使用者绕过 CLI、直接执行任意 Git 命令的行为提供安全防护。

## 4. 核心设计原则

### 4.1 一个物理 Store，多个逻辑 Space

一次安装只有一个 `ContextStore`，对应唯一 Git 仓库。仓库内可以包含多个 `ContextSpace`，每个 `ContextSpace` 表示一项内部 Requirement 或长期工作目标。

```text
ContextStore（唯一 Git Repo）
├── ContextSpace A
│   ├── Intent Revisions
│   └── Context Items
├── ContextSpace B
│   ├── Intent Revisions
│   └── Context Items
└── Shared Evidence Objects
```

ContextSpace 是逻辑聚合，不对应独立目录、独立数据库或独立 Git 仓库。

### 4.2 Git 保存不可变事件，SQLite 保存派生状态

Git 中只保存不可变事件和自包含证据。知识的当前状态由 SQLite 根据事件间的显式因果关系计算。

```text
Git Event Set
    │
    ├── Intent Revision
    ├── Context Revision
    ├── Evidence Snapshot
    └── Publication Transition
            │
            ▼
       SQLite Projection
            │
            ├── Current Intent
            ├── Accepted Context
            ├── Conflict
            └── Search Index
```

删除 SQLite 后，系统必须能够只依赖当前 Git Tree 中的事件内容重建同样的领域状态。

### 4.3 领域语义与开发现场解耦

以下信息可以作为辅助定位或展示信息，但不能参与领域身份、因果顺序和状态归约：

- 开发期 Commit SHA、Branch、Tag。
- PR、Issue、外部 Requirement ID 或 URL。
- 文件路径、行号、Symbol 位置。
- Agent Session、Agent 版本。
- 用户名、邮箱。
- 时间戳。
- Git Commit 顺序、事件文件路径。
- Confidence、相似度和排序分数。

这些字段应放入明确标记为非权威的 `annotations` 或 `origin_hint`。它们全部丢失后，Context 仍应可以理解和治理。

### 4.4 追加保护是产品行为，不是权限安全

Append-only 的含义是：

- 产品写 API 只创建新事件。
- 修订、发布、撤回、合并和冲突解决都生成新事件。
- 产品自动提交前检测已有文件是否被修改、删除或重命名。

它不表示：

- 当前用户无法直接修改本地文件。
- Git Hook 不可绕过。
- 本地仓库具备安全防篡改能力。

## 5. 领域模型

### 5.1 术语

| 术语 | 定义 |
|---|---|
| `ContextStore` | 一次安装唯一的 Git 知识仓库 |
| `ContextSpace` | 一项内部 Requirement 或工作目标的持续 Context 容器 |
| `IntentRevision` | ContextSpace 的目标、范围和验收条件的一版完整快照 |
| `ContextItem` | 一条逻辑工程认知，例如 Decision、Contract、Risk、Validation |
| `ContextRevision` | ContextItem 的一版完整内容快照，不是文本 Patch |
| `EvidenceSnapshot` | 可以脱离开发现场独立阅读的最小充分证据 |
| `Publication` | 某个 ContextRevision 当前是否对团队生效的治理节点 |
| `Event` | Git 中实际保存的不可变 JSON 文件 |
| `Projection` | SQLite 根据 Event 计算出的当前状态和查询索引 |
| `WorkspaceBinding` | 本地业务工作区到默认 ContextSpace 的便利映射，不属于知识事实 |

### 5.2 ContextSpace 与 Intent

不再存在外部 `RequirementRef`。ContextSpace 自身就是内部 Requirement 容器，使用内部随机 ID 标识。

初始 Intent 至少包含：

```yaml
title:
problem:
desired_outcome:
in_scope:
out_of_scope:
acceptance_conditions:
domain_terms:
```

Intent 发生变化时新增完整的 `IntentRevision`，旧 Revision 不修改。

`space.created` 同时携带首个 IntentRevision 完整快照；后续 `space.intent_revision_added` 必须引用同一 Space 的一个或多个 Intent Head。这样 ContextSpace 不会依赖另一个可能尚未到达的“创建清单”。

### 5.3 ContextItem 类型

首版支持：

- `decision`
- `contract`
- `issue`
- `risk`
- `validation`
- `discovery`
- `progress`

其中 Decision、Contract 应包含明确的主题与适用范围，以支持冲突识别。

### 5.4 稳定 ID

所有领域实体使用 CSPRNG 生成的随机、不透明 ID，例如 UUIDv4：

```text
spc_<random>   ContextSpace
ctx_<random>   ContextItem
rev_<random>   Revision
evt_<random>   Event
pub_<random>   Publication
evd_<random>   Evidence
```

禁止使用标题、路径、时间、用户名、外部单号或内容 Hash 作为领域 ID。

内容 Hash 只用于：

- 数据完整性校验。
- 内容寻址对象。
- 精确重复提示。

内容 Hash 不承担领域身份，因为规范化算法和 Schema 会演进。用于重复提示的语义 Hash 必须排除 `annotations`、`origin_hint`、记录时间和生产者等非权威字段，避免开发现场变化制造伪差异。

## 6. Git 数据设计

### 6.1 用户目录布局

```text
~/.shared-context/
├── config.toml
├── bin/
│   └── current/sctx
├── repository/                 # 唯一 Git 仓库
│   ├── .git/
│   ├── events/
│   ├── objects/
│   └── schemas/
├── state/
│   ├── index.sqlite
│   ├── writer.lock
│   ├── index.lock
│   ├── pending/
│   └── capture/
├── backups/
└── logs/
```

只有 `repository/` 是 Git 仓库。SQLite、配置、日志、锁和临时 Capture 数据都位于仓库外。

Cursor/Codex 自身要求的配置仍写入各自用户目录，但仅保存指向稳定二进制的 Hook/MCP 注册项：

```text
~/.cursor/hooks.json
~/.cursor/mcp.json
~/.codex/hooks.json 或 ~/.codex/config.toml
~/.agents/skills/shared-context/
├── SKILL.md
└── agents/openai.yaml
```

全局 Agent Skill 只在使用者显式执行 `sctx setup` 时安装；NPM `postinstall` 不写入该目录。Cursor 与 Codex 共享这一份用户级 Skill，不按 Agent 重复安装。

### 6.2 仓库结构

```text
repository/
├── events/
│   └── <id-prefix>/
│       └── evt_<random>.json
├── objects/
│   └── sha256/
│       └── <hash-prefix>/
│           └── <digest>
└── schemas/
    └── <schema-id>.json
```

设计约束：

- 每个事件一个文件。
- 事件路径由 Writer 生成，调用者不能传入。
- 文件路径只负责物理分片，不表达 ContextSpace、状态或事件顺序。
- 不维护可修改的全局清单、计数器、当前状态文件或 Requirement 目录索引。
- ContextSpace 改名，以及未来引入的合并操作，都不得移动任何历史文件。
- Schema 使用不可变版本文件；升级只新增新 Schema，Reader 保留对已写版本的解析能力。
- 大型文本证据可以存为内容寻址对象；首版不保存二进制附件。

## 7. 事件模型

### 7.1 事件类型

首版事件：

```text
space.created
space.intent_revision_added
context.revision_added
context.reviewed
context.publication_changed
semantic_conflict.opened
semantic_conflict.resolution_added
```

Review 是领域动作，不代表访问控制角色。项目使用者可以通过 CLI 发起 Review/Publish；Agent 默认只使用 Propose 能力，以降低误发布概率。

`context.reviewed` 是对确定 Revision 的不可变评审记录，包含随机 `review_id`、`revision_id`、`verdict: approve|reject` 和理由，本身不改变 Publication Head。Reducer 聚合该 Revision 的全部合法 Review：无记录为 Unreviewed，仅有 Approve 为 Approved，仅有 Reject 为 Rejected，两类同时存在为 Mixed；不按时间选一个。Publish 时可以由本地治理规则检查所引用的 Review，但规则只控制新事件能否生成；Reducer 不使用当前配置重新解释已经存在的 Publication。

### 7.2 Context Revision 示例

```json
{
  "schema_version": "1",
  "event_id": "evt_7cc4...",
  "event_type": "context.revision_added",
  "space_id": "spc_38b1...",
  "context_id": "ctx_aa91...",
  "revision": {
    "revision_id": "rev_a2f0...",
    "parent_revision_ids": ["rev_older..."],
    "kind": "decision",
    "topic_key": "search-result/general-tab-visibility",
    "statement": "General Tab 的可见性由服务端响应字段决定",
    "rationale": "客户端本地推导会导致多端结果不一致",
    "applicability": {
      "domains": ["search-result"],
      "platforms": ["ios", "android"],
      "conditions": ["响应包含 general_tab_visible 字段"]
    },
    "assumptions": [
      "响应仍包含 general_tab_visible 字段"
    ],
    "recheck_when": [
      "服务端重新定义字段语义",
      "客户端切换到新的导航协议"
    ],
    "evidence": [
      {
        "evidence_id": "evd_1234...",
        "kind": "source_snapshot",
        "supports": "客户端直接消费响应字段",
        "content": {
          "response_fragment": {
            "general_tab_visible": false
          },
          "consumer_logic": "UI directly maps the field to tab visibility"
        },
        "interpretation": "未发现客户端本地计算规则",
        "limitations": [
          "不证明未来协议不会变化"
        ]
      }
    ]
  },
  "annotations": {
    "created_at": "2026-08-18T12:00:00+08:00",
    "producer": "codex",
    "origin_hint": {
      "workspace_alias": "mobile",
      "path": "optional",
      "development_commit": "optional"
    }
  }
}
```

`annotations` 不参与领域归约，索引器可以完全忽略该对象而不影响结果。

`applicability` 是 Revision 内的自包含 Scope Snapshot，不引用用户配置或外部可变分类表。WorkspaceBinding 只能为查询提供 Hint，不能参与 Scope 重叠或冲突判定。

### 7.3 完整 Revision，而不是 Patch

- 新 Revision 保存完整快照。
- `parent_revision_ids` 表达内容演进关系。
- 单父 Revision 表示修订。
- 多父 Revision 表示显式合并。
- 新 Candidate 不会自动替代已发布 Revision。
- Revision 的父关系不等于 Publication 状态。

Intent 与 Context 内容均按 Revision DAG 归约：未被其他 Revision 作为父节点引用的 Revision 是内容 Head。一个 Head 表示当前唯一内容分支；多个 Head 表示并发修订冲突，必须新增一个引用全部 Head 的完整多父 Revision 才能收敛。该计算不读取时间或 Git 顺序。Publication 始终指向一个确定的 `revision_id`，不会用“当前最新 Revision”这类易变查询间接定位内容。

所有 Revision Parent 必须已经存在，并属于同一个聚合：Intent Parent 与子节点属于同一 ContextSpace，ContextRevision Parent 与子节点属于同一 ContextItem 和 ContextSpace。任何跨聚合父引用或关系环都进入 Diagnostic，不参与有效 Projection。

### 7.4 Publication 因果图

Publication 显式引用它认为的前置状态：

```json
{
  "schema_version": "1",
  "event_id": "evt_publish_2",
  "event_type": "context.publication_changed",
  "space_id": "spc_38b1...",
  "context_id": "ctx_aa91...",
  "publication": {
    "publication_id": "pub_2",
    "previous_publication_ids": ["pub_1"],
    "action": "publish",
    "revision_id": "rev_a2f0...",
    "review_event_ids": ["evt_review_2"]
  }
}
```

Projection 通过因果关系计算 Publication Heads：

- 没有 Head：尚未发布。
- 一个 `publish` Head：对应 Revision 为 Accepted。
- 一个 `withdraw` Head：Context 为 Deprecated。
- 多个 Head：Governance Conflict。
- Publication 被后继节点引用：对应 Revision 被后继状态替代。

并发操作形成多个 Head 时必须展示冲突，禁止按时间、Event ID、文件名或 Git Commit 顺序执行 Last-Write-Wins。

解决冲突时生成一个同时引用所有冲突 Head 的新 Publication；如果内容也发生合并，同时生成多父 ContextRevision。

Publication 引用的 Revision、全部 `previous_publication_ids` 以及 Publication 自身必须属于同一 ContextItem 和 ContextSpace，且 Publication 因果图必须无环。违反约束的节点不参与 Head 计算。

生命周期投影按 Revision 与 Publication 两个维度呈现，避免用一个可修改的 `status` 字段覆盖历史：

| 状态 | 确定性来源 |
|---|---|
| Candidate | Revision 已存在，但没有唯一有效的 Publish Head 指向它 |
| Rejected | 该 Revision 的 Review Summary 仅包含 Reject；不删除 Revision |
| Review Mixed | 同一 Revision 同时存在 Approve 与 Reject Review；全部展示 |
| Accepted | 唯一 Publication Head 的 action 为 `publish`，并指向该 Revision |
| Deprecated | 唯一 Publication Head 的 action 为 `withdraw` |
| Superseded | 后继 Publication 显式引用旧 Head，并发布另一 Revision |
| Conflicted | 同一前置状态产生多个未被引用的 Publication Head |

Review 结论也必须引用同一 ContextSpace/ContextItem 下确定的 `revision_id`；出现相互矛盾的 Review 时全部保留并展示，不按记录时间选边。

### 7.5 Capture 幂等与重复 Context 治理

`context_propose` 在追加事件前做服务端严格幂等检查，只阻止 Skill 重复触发产生的完全相同内容。两个 Proposal 仅在以下条件同时成立时视为同一 Context：

- 位于同一个 ContextSpace。
- 完整权威 Draft 逐字段完全相等：`kind`、`topic_key`、`statement`、`rationale`、`applicability`、有序 `assumptions`、有序 `recheck_when`，以及有序 Evidence Snapshot 中每一项的 `kind`、`supports`、`content`、`interpretation` 和有序 `limitations`。

比较排除生成的 Context/Revision/Evidence ID，以及明确非权威的 annotations/origin hints。检查同一 Space 的所有 Revision，不受 Candidate、Accepted、Deprecated 或 Superseded 生命周期状态限制。命中时返回 `deduplicated: true`、`status: existing` 和已有 Event/Context/Revision ID，不返回新的 Batch/Commit ID，也不新增 Event；任一权威字段不同都不是重复，必须保留为独立 Candidate。FTS、规则、Embedding、释义相似或 Topic 重叠只能用于召回候选，禁止据此自动去重、合并或抑制 Proposal。

上述幂等边界不等于语义上的“重复知识”治理。首版不合并或重定向 ContextSpace/ContextItem ID，避免提前引入另一套需要处理并发 Head 的身份归并协议。人类确认重复知识后：

- 为保留项新增吸收了必要内容的完整 Revision，并显式发布。
- 为重复项新增 Withdraw Publication，显式引用各自当前 Head。
- 重复项仍可按 ID 查询，不删除、不改名、不自动跳转。

通用 Context/Space Consolidation 留作后续事件协议；在定义其前置 Head、并发分支和合并规则之前，不进入首版状态机。

### 7.6 跨 Context 的语义冲突

Publication Head 只能发现同一 ContextItem 的并发治理，无法自动判断两段自然语言是否矛盾。为 Decision、Contract 强制保存 `topic_key` 与完整 Applicability Snapshot，SQLite 对“相同 Topic 且适用范围重叠”的多个 Accepted Revision 生成冲突候选。

- FTS、规则或后续 Embedding 只能提出冲突候选，不能自动裁决。
- 人类确认后新增 `semantic_conflict.opened`，使用新的稳定 `conflict_id`，显式引用涉及的 Context、Revision 和 Publication Head，并保存冲突理由与范围快照。
- 解决时新增 `semantic_conflict.resolution_added`。Resolution 使用独立随机 ID，并显式引用 `conflict_id`、全部前置 Resolution Head、所有仍有效的相关 Publication Head，以及保留、修订或按范围拆分的结果。
- 未解决的已确认语义冲突会阻止相关 Context 自动注入；查询必须同时展示各方，不得静默选择“更新”的一方。

`topic_key` 是 Revision 内容的一部分，不是实体身份。它的修正同样通过完整新 Revision 表达。

Semantic Conflict 的 Resolution 同样形成 DAG：无 Resolution Head 表示 Open；唯一合法 Head 表示 Resolved；多个并发 Head 表示 Resolution Conflict，仍按 Open 处理。Resolution 只关闭这条语义冲突记录，不能替代 Publication 的因果收敛；如果涉及的 Publication 仍有多个 Head，Validator 拒绝将冲突投影为 Resolved。

### 7.7 确定性校验与隔离

- 同一个 `event_id`、Revision ID、Review ID、Publication ID 或 Conflict/Resolution ID 出现在多个文件中时，所有冲突定义一并失效；禁止按扫描顺序保留第一个。
- 一个 `space_id` 必须且只能对应一个合法 `space.created`；多个创建定义会使该 Space 及依赖它的事件整体失效。
- 一个 `context_id` 只能属于一个 `space_id`；若完整事件集合给出多个 Owner，涉及该 Context 的全部定义和依赖节点失效。
- `evidence_id` 在首版定义为仓库内全局唯一；内容复用依赖 Object Digest，不复用 Evidence ID。Evidence ID 碰撞按其他唯一 ID 同样整体失效。
- Reducer 先基于完整事件集合构建 `entity_id → owner aggregate` 映射，只有映射单值后才处理因果边，不能用数据库首条插入结果决定归属。
- 对每类因果图检查同聚合引用、引用存在性和无环性；非法节点及依赖它的节点进入 Diagnostic。
- Quarantine 集合由完整事件集合确定，不能依赖文件遍历顺序、SQLite RowID 或 Git Commit 顺序。
- 未知 Schema 事件保留在 Git 中并报告 Diagnostic，在 Reader 支持该版本前不参与有效 Projection。

## 8. Evidence 设计

### 8.1 自包含原则

Evidence 必须在开发分支、开发 Commit、原始代码仓库或 Agent Session 消失后仍能表达：

- 观察到了什么。
- 如何得到这个结果。
- 它支持哪条结论。
- 证据有哪些限制。

首版支持：

- `source_snapshot`：最小充分的代码、配置、协议或文档片段。
- `experiment_record`：实验前提、输入、步骤、期望和实际结果。
- `artifact_snapshot`：接口响应、测试结果或其他结构化材料。

### 8.2 外部定位只能作为 Hint

下面的形式不能单独构成 Evidence：

```yaml
commit: abc123
path: src/foo.kt
line: 123
```

它们可以放在 `origin_hint`，用于帮助当前使用者跳转，但不能成为必填引用或状态依赖。

### 8.3 大型证据对象

较大的文本证据以内容寻址对象保存：

```text
objects/sha256/ab/<digest>
```

事件引用对象的：

- SHA-256。
- Media Type。
- Size。
- 对证据的解释与限制。

Writer 必须在提交前校验对象内容与摘要一致。对象已存在且内容一致时可以直接复用。

“可以复用”严格限定为：该 Object Path 已经存在于当前 HEAD Tree，且 Git Blob 内容与 Digest 一致。若 Path 只存在于 Working Tree/Index 或另一个 Batch Journal 中，它仍是 Pending，新的 Batch 不得只引用而漏提该对象；首版返回可重试的 `OBJECT_PENDING`，先恢复或显式处理原 Batch。新对象与引用它的 Event 必须记录在同一个 Batch Journal，并在同一个 Git Commit 中显式暂存。两个并发 Batch 产生相同 Digest 时，后取得 Writer Lock 的一方重新检查 HEAD：前一方已提交则复用，仍 Pending 则等待/报错，绝不覆盖或盲目接管。

## 9. 生成期追加保护

### 9.1 唯一写入口

CLI、MCP、Hook 最终统一调用 Rust：

```rust
append_event(payload)
```

调用者不能指定：

- 文件路径。
- Event ID。
- Git Parent、Tree、Commit 或 Ref。
- Update、Delete、Rename 操作。

Writer 负责：

1. 生成随机 Batch/Event ID 和目标路径。
2. 在 `state/pending/<batch_id>/files/` 生成并 fsync 完整内容，执行 Schema、引用和领域不变量校验。
3. 原子写入 `state/pending/<batch_id>/journal.json`，记录每个 Pending File、目标 Path、内容 Hash 和处理阶段。
4. 获取全局 Writer Lock，检查已有受管文件和 Git Index。
5. 使用 `create_new(true)` 创建目标文件。
6. 只暂存本批次明确生成的文件。
7. 提交前复核 Staged Diff 与 Batch Journal 完全一致，且状态全部为 A。
8. Commit 成功后记录 Commit OID，更新 SQLite，最后完成并清理 Journal。

禁止使用：

```bash
git add .
git add -A
```

### 9.2 提交前检查

自动提交前检查知识目录相对 HEAD 的状态：

```text
A  允许：新增事件或对象
M  拒绝：已有文件被修改
D  拒绝：已有文件被删除
R  拒绝：已有文件被重命名
```

检测到 M/D/R 时：

- 停止本次产品自动提交。
- 不自动覆盖或恢复用户文件。
- 输出受影响路径。
- 提示使用 `sctx context revise`、`sctx context withdraw` 等追加式命令。

检测到不属于本批次的 Staged Path 时同样拒绝提交，防止一次自动 Commit 顺带吸收使用者的手动暂存内容。

Batch Journal 是仓库外的恢复凭证，不是领域事实。恢复时不信任 Journal 的阶段字段或 Commit OID，而是先用当前 HEAD、Index、Working Tree 与 Journal 中的 Path/Blob Hash 对账：

1. 所有目标 Path 已在当前 HEAD 且 Blob Hash 一致：即使 Journal 还写着 `prepared/staged`，也把 Batch 认定为已 Commit，只补 SQLite 并完成 Journal，绝不重复 Commit。
2. HEAD 尚无目标 Path，Working Tree/Index 已与 Journal 完全一致：继续提交同一批 ID 和内容，不生成替代 Event。
3. HEAD 和 Working Tree 尚无目标 Path，但 Journal 管理的 `files/` 内容完整且 Hash 一致：用 `create_new(true)` 恢复相同目标 Path，再按第 2 类继续。
4. 部分已提交、Hash 不一致、Journal Payload 缺失或混入其他 Staged Path：停止自动恢复并要求显式人工处理。

其他未跟踪文件只由 Doctor 报告，不能猜测其来源或自动提交。人类可以显式执行 `sctx pending commit <batch_id>` 或 `sctx pending move-aside <batch_id>`。因此即使进程在 `git commit` 已成功但返回前、或 Commit OID 写回 Journal 前崩溃，同一 Batch 也最多产生一次语义提交。

### 9.3 并发与失败恢复

- UUID 降低并发路径冲突概率。
- `create_new(true)` 保证生成阶段不覆盖已有路径。
- Writer Lock 只串行化 Git Index/Commit，不阻塞 SQLite 读取。
- 文件创建成功、Git Commit 失败时，新文件保持 Pending；`sctx doctor` 校验并报告对应 Batch Journal，由下次 Writer 恢复或由使用者显式处理。
- Git Commit 成功、SQLite 更新失败时，下次查询根据 Git HEAD 自动补建索引。
- 自动提交只包含 Writer 本次生成的显式路径，不吸收仓库中的其他变化。

### 9.4 本地 Git Hook

Setup 可以安装可选的 Context Repo `pre-commit`：

```text
pre-commit → sctx validate --staged
```

Hook 复用相同 Validator，用于给手动 Git 操作提供更早反馈。Hook 是 UX 保护，可以被绕过，不属于正确性的信任根。

## 10. SQLite 本地投影

### 10.1 数据库位置与职责

```text
~/.shared-context/state/index.sqlite
```

SQLite 负责：

- 解析和校验诊断。
- 当前 Intent、Publication 和 Conflict 投影。
- ContextSpace Intent、Scope、Kind、Status 等结构化索引。
- FTS5 全文检索。
- Workspace、Capture 等本地便利信息的查询缓存；其持久配置仍在 `config.toml` 或对应的用户目录文件中。

SQLite 不负责：

- 保存唯一知识副本。
- 决定事件身份。
- 通过写数据库改变知识治理状态。

### 10.2 核心表

| 表 | 用途 |
|---|---|
| `meta` | Indexed Tree、Projection Generation 及全部实现版本 |
| `source_file` | Git Path、Blob OID、解析状态 |
| `space_projection` | ContextSpace 当前 Intent 投影 |
| `intent_revision` | Intent 完整 Revision |
| `intent_head` | 当前 Intent Head；多行表示 Intent Conflict |
| `context_item` | Context 逻辑实体 |
| `context_revision` | Context 完整 Revision |
| `review` | 对确定 Revision 的不可变 Review 与聚合摘要 |
| `publication` | Publication 因果节点 |
| `publication_head` | 当前有效 Head |
| `evidence` | Evidence Snapshot 和对象引用 |
| `scope` | 结构化适用范围 |
| `semantic_conflict` | 已确认的跨 Context 冲突 |
| `conflict_resolution` | Semantic Conflict Resolution DAG 与 Head |
| `conflict` | Intent、Publication、Resolution 等派生冲突总览 |
| `diagnostic` | 仅由当前 Tree 推导的非法事件、悬空引用、重复 ID、关系环；不存历史 Operational Warning |
| `context_fts` | 标题、结论、理由、证据的 FTS5 索引 |

建议参数：

```text
journal_mode=WAL
synchronous=NORMAL
foreign_keys=ON
busy_timeout=3000
```

使用 Rust bundled SQLite + FTS5，避免依赖不同 macOS 版本自带的 SQLite 特性。

### 10.3 增量索引

`meta` 保存 `indexed_tree_oid`、`projection_generation`、`db_schema_version`、`event_parser_version`、`reducer_version`、`conflict_detector_version`、`normalizer/tokenizer_version` 和 `search/ranking_version`。这些值仅用于投影同步与升级，不进入事件语义；任一实现版本不匹配时都必须重建对应投影。

所有 SQLite 写入使用独立的 `state/index.lock` 串行化，不能只依赖 WAL。查询或写入后的同步流程为：

1. 获取当前 HEAD Tree OID；若与 `indexed_tree_oid` 相同且实现版本一致，直接查询。
2. 获取 `index.lock`，再次读取 HEAD、`indexed_tree_oid` 和实现版本，防止基于过期游标更新。
3. 比较旧、新 Tree，并只通过 Git Blob 读取已提交内容，避免读取 Dirty Working Tree。
4. 解析变化事件，利用反向引用索引计算受影响闭包；新加入的目标可能让旧悬空引用恢复，因此也要纳入依赖它的节点。
5. 对受影响的整个 ContextSpace/ContextItem/Conflict 和同 Topic 候选集重算重复 ID、引用、环、Head、Scope Conflict 与 FTS。无法证明闭包完整时直接全量重建。
6. 在一个 SQLite 事务中用临时表替换受影响闭包，最后更新全部版本、`projection_generation` 和 `indexed_tree_oid`。
7. 提交后再次观察 HEAD；若写入期间 HEAD 已前进，则循环同步到新的 Tree 后再为本次请求返回结果。

未提交的 Pending 文件不进入 Projection。若 Tree Diff 出现已有受管路径的 M/D/R，安全的首版实现直接对新 Tree 全量重建，并在本次同步输出“追加协议被手工绕过”的 Operational Warning。该 Warning 可以进入日志/Doctor 输出，但不写入可重建 Projection 的 `diagnostic` 表，因为仅看当前 Tree 无法证明历史 M/D/R。Indexer 仍以新的当前 Tree 为事实，重新解析被修改的 Blob、移除已删除 Path 的事件，并传播由此产生的悬空引用或冲突。产品 Writer 随后拒绝自动提交，但这不被表述为本机安全防护。对同一个当前 Tree，增量索引与全量重建必须得到相同领域 Projection；Operational Warning 不属于这一等价性断言。

以下情况执行全量重建：

- 首次运行。
- SQLite `quick_check` 失败。
- 任一 DB Schema、Parser、Reducer、Conflict Detector、Normalizer/Tokenizer 或 Search/Ranking 版本变化。
- Indexed Tree 不可访问。
- 用户执行 `sctx index rebuild`。

重建只读取当前 Git Tree 和事件因果关系，不依赖 Git Commit 历史顺序。健康数据库优先在同一个 SQLite 文件中构建 Shadow Tables，并在持有 `index.lock` 时用单一事务切换，让已有 WAL Reader 完成旧快照后自然看到新 Generation，避免替换文件导致长驻 MCP 继续读取旧 Inode。

只有 SQLite 文件已经损坏、无法开启事务时才隔离旧文件并创建新 DB。CLI/MCP 每次请求前必须比较已打开连接与 `index.sqlite` 当前文件标识/Generation；发生替换时关闭并重开连接。首版也可以直接采用“每次请求打开连接”的简单实现。

一次 Query/Context Pack 请求必须在一个 SQLite 只读事务中完成：在开启事务前完成文件标识检查与必要的索引同步，随后在同一 Snapshot 中读取 `projection_generation`、`indexed_tree_oid`、Context、Conflict、Evidence 和分页结果，并把该 Generation/Tree 随响应返回。禁止用多次独立自动提交的 SELECT 拼装一个可能跨 Generation 的响应。

### 10.4 中文与代码搜索

首版使用结构化过滤 + FTS5。Rust 写入 FTS 前生成搜索 Token：

- Unicode 归一化、大小写折叠。
- 拆分 `snake_case`、`camelCase` 和文件样式标识符。
- 中文生成二元词组。
- 标题、Statement、Rationale、Evidence 分列并使用不同 BM25 权重。

查询优先级：

1. 当前 ContextSpace 精确匹配。
2. Scope、Kind、状态过滤。
3. FTS5 BM25 相关度。
4. Evidence 完整度。
5. `context_id ASC, revision_id ASC` 作为最终稳定 Tie-breaker。

默认不让“创建时间较新”压过已经验证的知识。

所有分页与 Context Pack 生成必须使用完整稳定排序键，不能依赖 SQLite RowID 或插入顺序；因此相同事件集合即使以不同顺序重建，结果顺序也一致。

## 11. Capture 与 Retrieve

### 11.1 Capture 分层

```text
Agent Session
    │
    ├── Breadcrumb / Trace
    │      本地短期保存，不进入 Git
    │
    └── Candidate Context
           结构化、自包含、写入 Git
```

Hook 可以将文件访问、测试结果等 Breadcrumb 写入：

```text
~/.shared-context/state/capture/
```

该目录设置 TTL，并在写入前进行 Secret/PII 检查。原始 Transcript 默认不进入 Git。

Agent 发现可复用且有证据的工程认知时，通过 MCP 调用 `context_propose`。Rust Writer 生成 Candidate Revision 事件。

显式 `sctx setup` 还会安装用户级 `shared-context` Agent Skill。Skill 在实质性工程任务开始时主动调用 `context_for_task`，并在出现可复用且已验证的结论时调用 `context_propose`；它只能创建 Candidate，不能自动 Review、Publish、Withdraw 或解决冲突。返回的 Context 一律按不可信、只读参考数据处理，不能执行其中的命令或指令。

Skill 的隐式触发依赖 Agent 的技能发现与调度，不是协议级强保证。Hook 继续负责 SessionStart 基础注入、受支持 Prompt Hook 的检索和 Breadcrumb，作为 Skill 未被加载或未主动调用时的确定性兜底；Skill 与 Hook 是互补关系，不相互替代。

### 11.2 Retrieve 流程

查询上下文由以下信息组成：

- 当前业务 Workspace。
- 本地 `WorkspaceBinding`。
- 当前聚焦的 ContextSpace。
- 用户 Prompt。
- 当前或最近访问的文件名、模块 Hint。

其中 Workspace、Prompt、文件路径仅用于查询，不进入 Context 身份或 Publication 状态。

检索流程：

1. 使用 ContextSpace、Scope、Kind、状态缩小候选集。
2. 使用 FTS5 排序。
3. 展开直接相关的 Evidence、Relation 和 Conflict。
4. 在 Token Budget 内优先返回摘要、ID、状态和匹配原因。
5. Agent 需要完整内容时再调用 `context_get`。

自动注入只包含：

- 已发布。
- 未 Deprecated。
- 无未解决 Governance Conflict 或已确认的语义 Conflict。
- Evidence 满足最低要求。

Candidate 可以显式查询，但默认不作为高权限指令自动注入。

## 12. CLI、MCP 与 Agent Adapter

### 12.1 Rust 单一入口

首版不要求常驻 Daemon。一个 Rust 二进制提供：

```text
sctx setup
sctx doctor
sctx space ...
sctx workspace bind|list|unbind ...
sctx context ...
sctx search ...
sctx index ...
sctx pending list|commit|move-aside ...
sctx demo ...
sctx hook --agent cursor|codex
sctx mcp serve --client cursor|codex
```

Git 写入通过 `writer.lock` 控制，投影写入通过 `index.lock` 控制；多个查询进程通过 SQLite WAL 并行读取。若真实性能数据证明需要常驻进程，再增加用户级 Daemon，不作为首版前置设计。

### 12.2 MCP Tools

Agent 首版暴露：

| Tool | 类型 | 说明 |
|---|---|---|
| `context_for_task` | 读 | 根据当前任务生成 Context Pack |
| `context_search` | 读 | 结构化条件 + 全文查询 |
| `context_get` | 读 | 获取指定 Context 的完整内容和 Evidence |
| `context_propose` | 写 | 生成新的 Candidate Revision |
| `space_list` | 读 | 查找可用 ContextSpace |

Space 创建、Review、Publish、Withdraw、Semantic Conflict 确认/解决首版优先由人类 CLI 完成。这个差异是产品交互选择，不是访问控制边界。

`context_for_task` 和 `context_propose` 接受 `space_id` 或当前 `workspace`。路由优先级为显式 `space_id` 高于本地精确 `WorkspaceBinding`；响应返回 `routing.resolved_space_id` 和 `routing.source`。未绑定、相对路径、目标 Space 不存在或两种路由信息都缺失时，Retrieve 与 Proposal 都必须结构化失败，不能猜测或退化为跨 Space 查询。Workspace 路径只作为非权威查询 Hint，不回显到 Tool Result，也不写入权威 Context。

`context_propose` 的重复保护必须在服务端完成，使用 7.5 节定义的“同 Space + 完整权威 Draft 完全相等”规则。Agent 侧的 `context_search` 只能缩小可能匹配集合，不能用 FTS 或语义相似度作最终重复判定。

### 12.3 Adapter 统一事件

核心定义：

```rust
enum CanonicalAgentEvent {
    SessionStart,
    PromptSubmit,
    PostToolUse,
    PreCompact,
    TurnStop,
    SessionEnd,
}
```

Adapter 负责：

- 检测 Agent 是否安装。
- 合并/卸载 Agent 配置。
- 将 Agent Hook 输入转为统一事件。
- 将统一 Action 转为 Agent 对应的 Hook 输出。
- 声明当前 Agent 版本支持的能力。

未来 Claude Code 只新增 Adapter 和配置生成器，不修改 Git、事件、SQLite 或 MCP 领域逻辑。

### 12.4 Cursor

- MCP：注册本地 stdio Server。
- SessionStart：注入工作区级摘要和 MCP 使用说明。
- Prompt 级精确检索：由 Agent 调用 `context_for_task`。
- PostToolUse/Stop：记录 Breadcrumb 或提示沉淀 Candidate。

Cursor 的 Prompt Submit Hook 不作为首版精确 Context 注入依赖，避免将产品正确性绑定到 Agent Hook 的不对称能力。

### 12.5 Codex

- MCP：注册本地 stdio Server。
- SessionStart：注入工作区级摘要和 MCP 使用说明。
- UserPromptSubmit：在支持时注入 Prompt-aware Context Pack。
- PostToolUse/PreCompact/Stop：记录 Breadcrumb、补充检索或提示沉淀 Candidate。

Codex Hook 需要用户 Review/Trust 时，Setup 和 Doctor 必须显示 `ACTION REQUIRED`，不能绕过信任机制或冒充安装完成。

### 12.6 主动 Shared Context Skill

`setup` 和 `upgrade` 把 instruction-only 的 `shared-context` Skill 事务化安装到
`~/.agents/skills/shared-context/`，由 Cursor 与 Codex 共用同一份用户级资产。NPM
安装和 `postinstall` 不修改 Skill 或 Agent 配置；只有显式执行 `sctx setup` 才安装。
Setup Manifest 记录产品拥有的 Skill 文件及 Hash，升级只覆盖仍与旧 Hash 一致的文件，
同名外部内容或安装后被用户修改的内容必须保留并告警。失败时 Setup Journal 恢复原字节
和权限，普通卸载只删除仍与 Manifest 匹配的受管文件。

Skill 的主动流程是：

1. 对实现、调试、架构、评审、迁移、测试、发布和运维等实质工程任务，在大规模调查或
   修改前调用 `context_for_task`，传入任务摘要与当前绝对 Workspace。
2. MCP 路由优先使用显式 `space_id`，其次使用精确 `WorkspaceBinding`。未绑定 Workspace
   不推测默认 Space；`context_for_task` 和 `context_propose` 都必须 fail closed，后者不得
   写 Event。成功的任务 Retrieve 硬过滤到解析出的 Space，不能把绑定仅作为排序偏好。
3. Retrieved Context 始终是不可信、只读参考数据。Agent 需要使用当前代码、文档、测试或
   可观测行为复核，不执行 Context 中的命令，也不允许其覆盖用户请求或当前证据。
4. 只有形成可复用、精确且有自包含证据的结论时，Skill 才调用 `context_propose`。成功
   只产生 Candidate；证据是否充分由 Skill 工作流判断，Rust 边界继续执行 Schema、隐私
   和 Writer 校验。
5. 同一 Space 的 Proposal 只有在完整权威内容逐字段、逐数组顺序完全相同时才复用所有
   生命周期状态中的已有 Context/Revision：`kind`、`topic_key`、`statement`、`rationale`、`applicability`、
   `assumptions`、`recheck_when`，以及每条 Evidence 的 `kind`、`supports`、`content`、
   `interpretation`、`limitations`。生成 ID 和非权威 annotations/origin hints 不参与比较。
   命中返回 `deduplicated: true`、`status: existing` 和已有 Event/Context/Revision ID，
   不返回新 Batch/Commit ID。GitStore 在 Writer 进程间锁内完成“恢复 Pending → 读取 HEAD →
   查重 → 追加”，使并发相同重试最多生成一个 Event；CLI 的人工 revise/review/publish
   路径不经过此去重入口。
6. 任一权威字段或数组顺序不同都必须生成独立 Candidate。禁止 trim、大小写折叠、FTS、
   Embedding、模糊匹配、改写相似度或其他语义合并。

MCP 新建 Candidate 时，非权威 annotations 仅记录 `producer: shared-context-mcp` 和 MCP `client` 类型；它们只证明写入经过 MCP 边界，不声称 Agent Skill 必然触发。不得记录 Workspace、Prompt 或 Session，且 annotations 不参与严格去重。

Skill 和 Agent 可见的 MCP 工具面不提供 Review、Publish、Withdraw、Supersede 或冲突解决。
Skill 明确禁止绕过 MCP 使用 CLI 或隐藏接口自动治理。人类仍可通过显式 CLI 完成治理；
这是交互与指令边界，不是针对本机用户的权限隔离。

自动化验收可以证明 Skill 资产内容、事务安装、MCP Tool Schema、绑定路由、Candidate
写入、严格去重和未绑定不写入。它不能证明真实 Cursor/Codex 在自然语言任务中一定隐式
调用 Skill，不能证明模型每次都正确判断“证据充分”，也不能证明模型在所有对话中遵守
禁止自动治理的指令；这些真实 Agent 行为必须报告为 `NOT_PROVEN`，不能由 Fixture 或
模拟 Tool Call 冒充。

## 13. Rust 与 NPM 分发

### 13.1 Rust 模块

建议 Rust Workspace：

```text
crates/
├── domain
├── event-schema
├── git-store
├── index
├── search
├── mcp
├── adapter-cursor
├── adapter-codex
├── installer
└── cli
```

其中：

- `domain`：领域对象、因果图和不变量。
- `event-schema`：JSON 解析、版本和校验。
- `git-store`：追加写、显式暂存、Commit、状态检查。
- `index`：SQLite Schema、增量同步、重建。
- `search`：结构化过滤、FTS、Context Pack。
- `mcp`：stdio MCP Server。
- `adapter-*`：Agent Hook/config 适配。
- `installer`：Setup、Backup、Doctor、Uninstall。
- `cli`：统一二进制入口。

调用系统 Git 时必须通过 argv 传参，不拼接 Shell 字符串；这样既可以复用 macOS Git 凭据链，也能避免命令注入。

### 13.2 NPM 包

```text
@company/shared-context
@company/shared-context-darwin-arm64
@company/shared-context-darwin-x64
```

- 主包提供极薄的 JavaScript Launcher。
- 两个平台包通过 `optionalDependencies`、`os`、`cpu` 选择 Mach-O。
- 不使用 N-API；运行时协议是 CLI、Hook、MCP stdio。
- 运行期不依赖 Node、Python、Homebrew 或远端下载。
- 平台二进制需要签名并在包内提供校验摘要。

发布流水线同时为每个架构生成自包含离线 Bundle，不能假设主包 tgz 能在断网时解析 Registry 中的 `optionalDependencies`：

```text
shared-context-<version>-darwin-<arch>-offline/
├── package.json                 # 依赖均指向下列本地 file: tgz
├── package-lock.json
├── packages/
│   ├── shared-context.tgz
│   └── shared-context-darwin-<arch>.tgz
├── install                      # 执行 npm --offline 并启动 setup
└── SHA256SUMS
```

Bundle 中不包含两个架构的冗余二进制；发布 CI 分别在 arm64/x64 Mac 断网环境验证。`install` 只准备本地 Runtime 并显式调用 Setup，不能绕过 `postinstall` 不修改 Agent 配置的原则。

NPM `postinstall` 不修改 Cursor/Codex 配置。显式执行 `setup` 后，二进制复制到：

```text
~/.shared-context/bin/<version>/<arch>/sctx
~/.shared-context/bin/current/sctx
~/.agents/skills/shared-context/SKILL.md
~/.agents/skills/shared-context/agents/openai.yaml
```

Agent 配置始终引用 `bin/current/sctx`，升级只原子切换 Runtime 版本。Skill 资产编译进 `sctx`，不依赖安装时联网或 NPM 运行时文件。

## 14. 安装、测试与卸载

### 14.1 快速安装

```bash
npx -y @company/shared-context@<version> \
  setup --agents cursor,codex --demo
```

Setup：

1. 检查 macOS、CPU、Git、签名、磁盘空间。
2. 创建或复用 `~/.shared-context/`。
3. 初始化或验证唯一 `repository/`。
4. 不允许自动创建第二个 Context Git 仓库。
5. 创建/重建 SQLite。
6. 检测 Cursor/Codex。
7. 显示配置 Diff。
8. 备份并原子合并 Hook/MCP 配置。
9. 原子安装或升级 `~/.agents/skills/shared-context/`，并把受管文件及 SHA-256 写入安装 Manifest；同名用户文件或已修改文件保留并告警。
10. 交互式 Onboarding 创建首个 ContextSpace，并把选择的业务 Workspace 绑定到它；`--demo` 则在同一仓库内创建自包含样例 Space。
11. 运行 Git、SQLite、MCP、Adapter、Global Skill Smoke Test；`--demo` 额外运行 Proposal、Review/Publish 和 Search 闭环。
12. 显示需要重启或人工 Trust 的 Agent，以及可复制的 `space create`、`workspace bind`、`context propose` 和 `search` 命令。

Setup 必须幂等。每次执行生成 Setup Journal；失败时恢复 Runtime 和 Agent 配置，但不删除、回滚或覆盖 Context Git 数据。

Agent 配置采用“文件锁 → 解析 → 最小结构合并 → 展示 Diff → 原字节备份 → 临时文件 fsync + rename → 重新解析”的流程。Cursor JSON 保留全部未知字段；Codex TOML 使用保留注释/格式的编辑器。安装 Manifest 记录本产品新增条目的稳定标识和 Hash；卸载只移除仍与记录一致的条目，安装后被使用者修改或新增的配置保留并告警。

非交互测试机可以使用：

```bash
sctx setup --config ./setup.toml --yes --demo
```

`--demo` 的成功标准不是空库查询，而是完成 `space create → workspace bind → propose → publish → search` 的本地闭环。

### 14.2 离线安装

```bash
tar -xzf shared-context-<version>-darwin-<arch>-offline.tar.gz
./shared-context-<version>-darwin-<arch>-offline/install \
  setup --agents cursor,codex --demo
```

离线 Bundle 同时携带主包和当前架构平台包，并使用本地 `file:` 依赖与 npm Offline Mode 安装。Bundle 取得后，Setup、Append、Index、Search、MCP、重建和卸载均不产生网络请求。

### 14.3 Demo

```bash
sctx demo seed
sctx demo run
```

Demo 在唯一 Git 仓库中创建一个逻辑 `demo` ContextSpace，不创建第二个 Git 仓库。它用于验证：

- Space/Intent 创建。
- Candidate 提交。
- Review/Publish。
- CLI/FTS 查询。
- MCP Initialize/List Tools/Search/Propose。
- Cursor/Codex Hook Adapter Fixture。
- SQLite 删除和重建。

Demo Space 不再需要时可以通过本地 `config.toml` 的显示偏好隐藏；这只是界面偏好，不改变 Git 中的领域状态，也不删除历史事件。

### 14.4 Doctor

```bash
sctx doctor
sctx doctor --json
```

检查：

- `~/.shared-context/` 目录和权限。
- 唯一 Git 仓库及 `git fsck`。
- 已有文件 M/D/R 和 Pending A。
- Event Schema 和因果图诊断。
- SQLite `quick_check`、Indexed Tree、FTS。
- Cursor/Codex 配置语法和目标路径。
- 全局 Agent Skill 文件、内容 Hash 和安装所有权状态。
- MCP Initialize/List Tools。
- Hook Trust 或需要人工处理的步骤。

`doctor --fix` 只进行安全且可逆的修复，例如重建 SQLite、恢复缺失注册。它不能 Commit Pending Batch，也不能自动恢复或覆盖使用者手动修改的 Git 文件；Pending 只能由下一次 Writer 按 Journal 恢复，或由使用者显式执行 `sctx pending ...`。

### 14.5 卸载

```bash
sctx uninstall
```

默认行为：

- 移除自己仍能精确识别的 Cursor/Codex 配置条目。
- 只移除 Manifest 中由本产品拥有且内容 Hash 未变化的全局 Skill 文件；用户修改或同名用户文件保留并告警。
- 恢复安装前的配置备份或保留用户后续修改并告警。
- 移除 Runtime、日志和可重建缓存。
- 保留唯一 Context Git 仓库。

删除知识数据必须使用单独的显式命令，并展示绝对路径和二次确认，不作为普通卸载的一部分。

## 15. 一致性、异常与恢复

| 场景 | 处理 |
|---|---|
| Event 已创建、Commit 失败 | 保留 Pending A 与 Batch Journal；下次 Writer 可按 Hash 恢复，Doctor 只报告，手工操作需显式命令 |
| Commit 成功、SQLite 更新失败 | Git 仍为事实；下次查询增量补建或全量重建 |
| SQLite 损坏 | 持有 `index.lock` 隔离旧 DB，从当前 Git Tree 重建；各进程检测文件标识/Generation 后重开连接 |
| 手动修改/删除/重命名已有事件 | 自动提交拒绝；Doctor 报告，不自动覆盖 |
| 重复 Event/Revision 等唯一 ID | 所有碰撞定义进入 Diagnostic，不按扫描顺序保留任何一个 |
| 悬空引用或因果环 | 相关事件 Quarantine，不猜测状态 |
| 多个 Publication Head | 显式 Governance Conflict，不采用 Last-Write-Wins |
| Agent 配置写入失败 | 根据 Setup Journal 恢复原字节与权限 |
| 新 Agent 版本能力未知 | 降级为 MCP + CLI，禁用未验证 Hook 能力 |

## 16. 安全与隐私边界

- 原始 Conversation/Transcript 默认不进入 Git。
- Capture 数据设置 TTL，并执行 Secret/PII 检查。
- Event 和 Evidence 写入 Git 前再次执行敏感信息扫描。
- Context 内容按不可信数据处理，不执行其中的命令或脚本。
- 只有已发布、未冲突的 Context 可以自动注入。
- Candidate、Annotation、外部 Hint 不得提升为系统指令。
- Git 仓库位于当前用户目录，不宣称抵御当前用户主动篡改。
- 首版只有一个知识仓库，因此不提供文件级或 ContextSpace 级读取隔离。

## 17. 实施阶段

### 阶段一：领域与 Git 基础

- Event Schema 与兼容策略。
- ContextSpace、Intent、ContextRevision、Publication 因果图。
- 单仓库初始化。
- Append Writer、显式暂存和生成期保护。
- Validator 与 Fixture。

### 阶段二：SQLite 与查询

- SQLite Schema。
- Tree-based 增量索引与全量重建。
- 中文/代码分词。
- 结构化过滤、FTS 和 Context Pack。
- Conflict/Diagnostic 投影。

### 阶段三：CLI 与 MCP

- Space、Context、Review/Publish CLI。
- MCP Search/Get/Propose。
- WorkspaceBinding。
- Capture TTL 和敏感信息检查。

### 阶段四：Agent 与分发

- Cursor Adapter。
- Codex Adapter。
- NPM arm64/x64 平台包。
- Setup、Doctor、Upgrade、Uninstall。
- Demo 和端到端测试。

## 18. 首版验收标准

### 18.1 安装与环境

1. 干净 arm64/x64 Mac 从 NPM 或对应架构离线 Bundle 执行 `setup --demo`，到完成首个 Space/Workspace Binding/Publish/Search 闭环不超过 3 分钟。
2. 全流程不需要 sudo、远端 Git、外部账号或 API Key。
3. arm64/x64 的离线 Bundle 分别在断网环境完成 Setup、Append、Index、CLI/MCP 查询、重建和卸载；安装过程不访问 Registry。
4. Setup 连续执行三次不产生重复配置。
5. 路径包含空格和中文时可以正常工作。
6. 一次安装内只能存在一个 Context Git 仓库；Setup 和 Demo 不创建第二个仓库。
7. 对带未知字段、TOML 注释和既有 Cursor/Codex 条目的配置，在每个写入阶段注入失败后均恢复原字节；正常安装/卸载不丢失安装后新增或修改的用户配置。
8. 显式 Setup 在 `~/.agents/skills/shared-context/` 安装合法的 `SKILL.md` 与 `agents/openai.yaml`；Cursor、Codex 同时启用时仍只有一份，连续 Setup 不重复写入，升级/卸载不覆盖或删除用户修改。

### 18.2 Git 与事件

1. CLI/MCP 所有写接口只生成新事件或新证据对象。
2. 调用者不能通过产品 API 指定路径或覆盖已有 Event ID。
3. 100 个权威 Draft 不同的并发 Proposal 生成 100 个不同的新事件文件；完全相同 Draft 的并发重试按 18.5 的严格幂等规则收敛。
4. 自动提交只暂存本批次生成的显式路径。
5. 已有事件出现 M/D/R 时自动提交拒绝，并输出可执行的修订提示。
6. 禁用 Git Hook 后，产品生成和自动提交路径仍不覆盖已有文件。
7. Revision、Withdraw、Semantic Conflict 确认与解决全部通过新事件完成。
8. Crash 后只有带有效 Batch Journal 且 Path/Hash 匹配的 Pending 文件能自动恢复；来源不明的新增文件不会被 Doctor 自动提交。
9. 在 `create_new`、`git add`、`git commit` 返回前/后、Commit OID 写回 Journal、SQLite 更新和 Journal Cleanup 各边界注入崩溃；恢复后同一 Batch 的 Event ID/内容不变，最多一次语义提交，HEAD 与索引最终一致。

### 18.3 领域稳定性

1. 删除原业务 Workspace、开发 Branch 和开发 Commit 后，Context/Evidence 仍可独立阅读。
2. 对“保留 annotations/origin_hint”与“构造时剥离这些字段”的等价 Event Fixture 分别归约，Projection 结果保持一致；测试不修改已经写入的历史文件。
3. 事件发现顺序不同，Publication Head 和 Conflict 归约结果保持一致。
4. 并发 Publication 形成多个 Head 时必须显式报 Conflict。
5. 不允许通过时间、Git Commit 顺序或文件名隐式解决冲突。
6. 两条适用范围重叠的 Accepted Decision 被确认矛盾后，默认注入同时阻断，查询同时返回冲突双方。

### 18.4 SQLite 与查询

1. 删除或损坏 SQLite 后，可以从当前 Git Tree 确定性重建。
2. 重建后的 Space、Accepted Context、Conflict 和排序 Fixture 一致。
3. 10 万条测试事件下，Warm Search P95 小于 100ms；验收报告必须固定查询集并记录 Mac 型号、CPU、内存、macOS 与文件系统信息。
4. 查询响应包含 Indexed Tree、Projection 状态、匹配原因和 Conflict 标记。
5. 对任意追加序列，以及测试中手工提交的 M/D/R，增量数据库与针对同一 Tree 的 Scratch Rebuild 在有效状态、仅由当前 Tree 推导的校验 Diagnostic、FTS 文档和稳定排序上完全一致；依赖旧新 Tree Diff 的 Operational Warning 明确排除。
6. 并发运行 Query、Index、Rebuild 和 Git Append，最终 `indexed_tree_oid` 与 HEAD Tree 一致，游标不倒退，长驻 MCP 不继续返回已替换旧 DB 的 Projection。
7. 每个多页 Search/Context Pack 响应中的 Tree、Generation、Context、Conflict 与 Evidence 来自同一 SQLite Read Transaction。

### 18.5 Agent 与配置

1. Cursor、Codex 均完成 MCP Initialize、List Tools、Search、Get、Propose。
2. Cursor/Codex Adapter 对真实 Hook Payload Fixture 通过契约测试。
3. Agent Hook 不可用时，CLI 和 MCP 仍能完成核心流程。
4. Codex Hook 未 Trust 时明确显示 `ACTION REQUIRED`，不能冒充成功。
5. `context_for_task` 和 `context_propose` 使用绑定 Workspace 路由到相同 Space，显式 `space_id` 优先；未绑定或缺少路由时两者均 fail closed，Workspace 路径不进入权威 Context 或 Tool Result。
6. 对同一 Space 和完全相同权威 Draft 连续或并发调用 `context_propose`，只产生一个 Candidate Event；重复响应为 `deduplicated: true`、`status: existing` 并返回同一 Event/Context/Revision，不返回新的 Batch/Commit ID。任一权威字段发生变化都产生独立 Candidate，语义近似不得自动合并。
7. Hook 关闭或 Skill 未触发时，已声明的 Hook 兜底行为仍通过真实 Payload Fixture；Skill 被真实 Agent 隐式发现并调用必须由端到端 Agent Trace/Tool Call 证明，只验证文件安装、YAML 或 Skill 静态内容时一律标记 `NOT_PROVEN`，不能声称隐式触发已验收。
8. 卸载后恢复原 Agent 配置，保留用户修改的 Skill，并保留唯一 Context Git 仓库。

## 19. 后续演进

首版完成后可在不改变领域模型的前提下增加：

- 为唯一 Git 仓库配置一个远端，增加团队同步和远端追加校验。
- Claude Code Adapter。
- External System Alias，但外部 ID 仍不能成为领域主键。
- Embedding 表和混合检索；向量仍然只是派生索引。
- Review/Conflict 可视化界面。
- 更细粒度的团队治理策略。
- 基于真实数据评估是否需要常驻用户级 Daemon。

## 20. 核心不变量

1. 一次安装只有一个 ContextStore 和一个 Git 仓库。
2. ContextSpace 是内部 Requirement 容器，不依赖外部需求系统。
3. Git 当前 Tree 中的事件与证据是事实源。
4. SQLite、Capture、WorkspaceBinding 和 Agent 配置都不是事实源。
5. 所有领域实体使用内部随机稳定 ID。
6. Revision 保存完整快照，不保存文本 Patch。
7. Evidence 必须自包含，开发现场引用只能作为 Hint。
8. Git Commit、时间、文件路径和 Agent Session 不参与状态归约。
9. Publication 只按显式因果关系归约，并发 Head 必须暴露为冲突。
10. 产品写入路径只创建新文件，修订与治理都通过新事件完成。
11. 产品自动提交不得吸收或覆盖仓库中已有文件的变化。
12. 删除 SQLite 后必须能够从 Git 确定性重建。
13. 相同 Git Tree 和相同实现版本必须得到相同 Projection 与稳定查询顺序。
14. Append-only 是生成期协作约束，不是操作系统权限或本机防篡改边界。
15. Capture 幂等只认同一 ContextSpace 内完整权威 Draft 完全相等；语义近似、释义或搜索分数不构成重复。
16. Agent Skill 隐式触发不是强保证；Hook 提供确定性基础兜底，未观察到真实 Agent Tool Call 时验收状态为 `NOT_PROVEN`。

## 21. Agent 接入参考与版本策略

以下能力已按 2026-08-18 的官方文档核对：

- [Codex MCP](https://learn.chatgpt.com/docs/extend/mcp?surface=cli)：支持本地 stdio Server，配置位于 `~/.codex/config.toml`，并提供 `codex mcp add/list`。
- [Codex Hooks](https://learn.chatgpt.com/docs/hooks)：支持 `SessionStart`、`UserPromptSubmit`、`PreCompact`、`PostToolUse`、`Stop` 等事件；`UserPromptSubmit` 可返回 `additionalContext`；非 Managed Command Hook 需要使用者 Review/Trust。
- [Cursor MCP](https://docs.cursor.com/context/model-context-protocol)：支持 stdio MCP，并支持用户级 `~/.cursor/mcp.json`。
- [Cursor Hooks](https://cursor.com/docs/hooks)：支持 `sessionStart`、`beforeSubmitPrompt`、`preCompact`、`postToolUse`、`stop` 等事件；`sessionStart` 可返回 `additional_context`。

这些是易变的厂商适配能力，不属于领域核心不变量。每个 Adapter 必须声明已验证的 Agent 版本范围，使用真实 Payload Fixture 做契约测试；遇到未知版本或能力不匹配时降级为 MCP + CLI，并由 `doctor` 给出明确诊断。
