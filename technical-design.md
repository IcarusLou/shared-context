# Shared Context 技术方案

## 1. 文档信息

- 状态：目标技术方案
- 目标平台：macOS（Apple Silicon / Intel）
- 分发方式：Rust 原生二进制 + NPM 包
- 首版 Agent：Cursor、Codex
- 配套产品目标：[Shared Context：预期效果](./readme.md)

本项目尚未上线，本文直接定义目标模型、接口和存储结构。

### 1.1 当前实现状态（Mew #170）

本文的大部分章节描述目标架构，不代表代码已经全部实现。当前里程碑边界如下：

| 里程碑 | 状态 | 当前代码事实 |
|---|---|---|
| **M1：Task-first 领域与入口基础** | **已实现** | `WorkingIntentSnapshot` 无 Space；`TaskSpaceAssociation` 支持 `0..N`；不存在 Workspace-to-Space 绑定；检索没有 preferred-Space 排序；无 Space Candidate 不可自动注入 |
| **M2：Task Runtime 与多 Space Retrieval** | **已实现** | TaskSession/runtime.sqlite、`TaskIntentRevision`、Space Intent 召回、`0..N` 多 Space 关联、typed RetrievalPath、严格 `task_intent_update` 与只读 `task_context` 已通过跨 crate/E2E 验收 |
| **M3：Engineering Graph** | **已实现** | 稳定本机 Repository Catalog、可重建 Registry、Reference-derived 有界扫描、build-time immutable Context/safety snapshot、历史 Graph Retrieval、ContextRelation 1–2 跳及 MCP/CLI 工作流已通过固定跨 crate/E2E oracle |
| **M4：Low-tax Capture** | **已实现** | #117、#136、#156–#164 与 #169 已实现旁路 Working Intent、Hint Text retrieval、WorkEpisode、Hook lifecycle、Candidate Builder/analysis/Review/Confirm，并通过固定跨层 E2E、privacy、performance 与恢复验收 |
| **Repository 范围推理前准入与 Session 授权** | **实现中** | #181–#191/#194 已实现 Direct/显式 Group/Disabled、短期 Session lease、SessionStart 固定 marker、MCP Session guard、registered cross-Repo / safe non-locating / unsafe drop 与 SessionEnd 清理；#192 已把完整 Skill workflow 改为可信 marker 后的 installer-owned 渐进加载，最终 token proxy/NPM 验收仍待 #193 |

当前 `task_intent_update` 通过外部 Session Locator 和 Revision CAS 创建或修订承载 `WorkingIntentSnapshot` 的 `TaskIntentRevision`，并返回可解释的多 Space TaskContextPack；`task_context` 只按 Locator 读取已有 ActiveTask，不能提交 Intent、Signals 或身份。SessionStart 先在本地同步完成 Repository 范围准入，只有 Enabled lease 才向 Agent 返回固定短 marker；PromptSubmit 始终不重复 marker，也不读取 Runtime/Search。Enabled lease 是 Session-level 准入：PostToolUse 的安全已登记路径按 Catalog 保留真实 Repository 归属，安全未登记或 mixed/unrepresentable multi-Repo 事件只形成无 workspace/file hint 的 non-locating Breadcrumb，可识别测试工具仍形成非事实、非定位的 TestOutcome；unsafe 输入整条丢弃。TaskSignal 可影响 Working Intent retrieval，但不是工程 Evidence。Hook 不运行 Git discovery、Scanner、Registry sync、Graph rebuild、Focus 提交、Episode open/ingest，也不伪造 Claim。PreCompact/TurnStop 只能关闭已有 current-Intent Checkpoint 的 Episode并调用共享 Builder。显式 Graph 工具完成 bounded scan、Reference record、rebuild/diagnose 和 explain；Graph 不可用时 Task Retrieval 降级为 Context-only。Candidate 只由 closed WorkEpisode 的 Builder 调用内部 submission service 创建，公开面仅提供 list/get/discard/confirm。

### 1.2 Repository 范围推理前准入（Mew #180/#185）

当前已把本地范围组件接入 Agent Hook；整个准入发生在模型推理前，不读取 Prompt，也不经过模型：

- `RepositoryGroup` 是产品私有 `config.toml` 中的显式本机配置。Group root、成员 Repository 与 `RepositoryGroupId` 由 typed CLI 进行 add/update/remove/list/doctor；配置与修复不会在业务仓库中创建项目级 Agent 文件。
- `ScopeResolver` 对 canonical Session 启动目录返回 `Direct`、`Group` 或 `Disabled`。`Direct` 使用已注册 checkout 的 longest-prefix 并优先于 Group；`Group` 只匹配显式 root 的精确相等；任意祖先目录和未注册 sibling 都是 `Disabled`。
- `AuthorizedSessionScope` 是按 `ExternalSessionLocator` 隔离、Catalog revision 约束、最长 24 小时的产品私有 lease。文件名只含 locator digest，记录只含 typed decision、允许的 RepositoryId、Catalog revision 与 TTL；不保存 checkout/Group root、Prompt、transcript、tool output、report 或业务正文。
- SessionStart 同步读取 Catalog，并对 Catalog/lease 使用 non-blocking try-lock。只有 Missing locator 可以按本次 canonical cwd 解析并先持久化 lease；Current 直接复用，Stale/Expired/锁忙/解析异常立即返回 `Disabled`。同一 locator 的首次成功决定是 sticky，后续 startup/resume/compact 即使 cwd 改变也不重新判归属。
- `Direct` 与 `Group` 使用同一个不超过 128 bytes、无 Repository/路径/Prompt/身份的 activation marker；marker 只出现在显式 SessionStart（包括 Codex resume/compact）边界，PromptSubmit 返回 neutral wire output。
- Enabled PostToolUse 在 Runtime/Capture 之前验证结构化 `file_path`/`filepath`/`path`/`workdir`/`working_directory`。启动时的 Direct/Group 与 `allowed_repository_ids` 不限制 Session 后续调查目标：任意已登记 Repository 使用 Catalog longest-prefix 与真实 File mapping；显式 Group root 可安全表示覆盖其下多个 registered checkout 的单一 Capture workspace。安全未登记路径、registered/unregistered mixed 或无显式 root 可表示的多 checkout 事件整条降级为 path-free non-locating meaning；ambiguous、missing、relative、symlink 或非 file/directory 输入整条丢弃。
- Disabled 的 Prompt/Tool/PreCompact/Stop/End 全部保持 Agent-neutral 且不打开 Runtime/Capture；SessionEnd 只按 exact locator 尝试移除 lease，不跨 Session 清理。Catalog/lease 锁忙或异常均 fail-open 让 Agent 继续，同时 fail-closed 为 Disabled。
- scope 解析与 lease 热路径不运行 Git 或 Repository scan；安装仍是用户级配置，不需要 launcher，不在业务仓库写项目级 MCP/Hook 文件。

当前 MCP Server 已按 current Enabled `AuthorizedSessionScope` 实施 Session-level authorization guard；Disabled/Missing/Expired/Stale/busy/corrupt Session 调用被拒绝，Enabled Session 可调查任意已登记 Repository或提交不伪造 Artifact identity 的非定位 Evidence。全局 Skill 主入口是最小 activation gate：没有可信 SessionStart marker 时自动路径不读取完整 workflow reference、不产生 Shared Context MCP 调用提示；有 marker 时才完整读取一次 installer-owned reference。Server guard 只负责安全和不落越权数据，Skill gate 负责调用前的指令准入。MCP 进程和工具 Schema 仍由用户级配置全局提供，可能物理启动或可见；#192 的 reference-read/MCP-call 字节合约不等于真实计费 token 测量，最终 proxy 与 NPM/分发闭环仍由 #193 验收。

## 2. 背景与目标

本项目希望让 Agent 在工程过程中形成的有效理解、判断和验证，低成本地沉淀为后续任务可以直接继承的工程 Context，从而降低跨端、跨仓库、跨 Agent 和跨 Session 的冷启动成本。

系统需要完成一条连续链路：

```text
理解当前 Task
    ↓
连接历史 Intent、Context 与工程对象
    ↓
注入当前真正相关的 Context
    ↓
持续观察开发过程
    ↓
自动形成新的 Context Candidate
```

核心目标：

1. 以当前 `TaskIntentRevision` 中的 `WorkingIntentSnapshot` 作为默认检索入口，不要求调用者提前选择 Space。
2. 自动推断一个 Task 与多个 `ContextSpace` 的关联，并返回匹配原因。
3. 建立 Context 与 Repository、Module、File、Symbol、API、Schema、Test 的可重建关联。
4. 从 Prompt、代码访问、Diff、测试和 Agent 结论中自动生成 `ContextCandidate`。
5. 使用 Git 保存稳定、可审计的 Context 事实，使用 SQLite 保存可删除、可重建的投影和工程关联。
6. 保持自动注入安全：只有有效、已激活、证据充分且无阻断冲突的 Context 可以自动进入 Agent 上下文。

## 3. 范围与非目标

### 3.1 已确认约束

- 一次安装维护一个本地 `ContextStore`，对应唯一 Git 仓库。
- `ContextSpace` 表示一项内部 Requirement 或长期工作目标，不依赖外部需求系统维持身份。
- Workspace 只说明代码位置和当前工程现场，不能决定当前 Task 属于哪个 Space。
- 一个 Task 可以同时关联零个、一个或多个 Space。
- `ContextCandidate` 在形成时可以没有 Space；Space 归属是 Candidate 的后续确认结果。
- 文件路径、Symbol 位置、Commit、Branch 等不能成为领域身份，但必须能参与检索和关联重建。
- Git 中只保存稳定 Context 事实和自包含 Evidence；Task、Capture、置信度和当前代码解析结果保存在本地状态中。
- SQLite 不是事实源；删除后必须能够从 Context Git Tree 和当前可访问的业务代码仓库重建相应投影。
- 产品写接口只创建新事件和对象，不修改、删除或重命名已有知识文件。
- 首版支持 Cursor 和 Codex，Adapter 边界支持后续新增其他 Agent。

### 3.2 非目标

- 不依赖外部 Requirement、Issue、PR 或文档系统完成核心流程。
- 不把完整 Conversation、Transcript 或原始 Tool Output 写入 Git。
- 不实现独立 GUI 或 IDE Extension。
- 首版不依赖向量数据库或本地 Embedding 模型；先以结构化关联、FTS 和可解释规则完成召回。
- 不实现本地防恶意篡改、专用系统账号、root Helper 或权限隔离。
- 不承诺对使用者绕过产品、直接执行任意 Git 命令的行为提供安全防护。

## 4. 总体架构

### 4.1 四层结构

```text
┌─────────────────────────────────────────────────────────────┐
│ Agent Integration                                           │
│ Prompt / Tool / Diff / File / Symbol / API / Test Signals   │
└──────────────────────────────┬──────────────────────────────┘
                               ↓
┌─────────────────────────────────────────────────────────────┐
│ Task Runtime                                                │
│ TaskSession / TaskIntentRevision / WorkEpisode / Candidate  │
└───────────────────────┬──────────────────────┬──────────────┘
                        ↓                      ↓
┌───────────────────────────────┐  ┌──────────────────────────┐
│ Association & Retrieval       │  │ Candidate Builder        │
│ Space Inference               │  │ Claim / Evidence / Scope │
│ Engineering Graph             │  │ Dedup / Conflict         │
│ Multi-source Ranking          │  │ Space Recommendation     │
└───────────────────┬───────────┘  └─────────────┬────────────┘
                    ↓                            ↓ confirm
┌─────────────────────────────────────────────────────────────┐
│ Context Repository                                          │
│ Space Intent / Context / Evidence / Lifecycle / Conflict    │
│ Git Events + Objects                                        │
└─────────────────────────────────────────────────────────────┘
```

### 4.2 两类状态

稳定事实：

- ContextSpace 与 Intent Revision。
- ContextItem 与 Context Revision。
- Context 与 Space 的显式组织关系。
- Evidence Snapshot。
- Context 生命周期和语义冲突。

派生或短期状态：

- WorkingIntentSnapshot 与 TaskIntentRevision。
- Task 与 Space 的相关性分数。
- 当前文件、Symbol、API 和 Schema 的解析位置。
- Context 与工程对象的当前匹配结果。
- WorkEpisode、ContextCandidate 和候选置信度。
- FTS、排序分数、分页游标和 Token Budget 结果。

稳定事实进入 Git；派生或短期状态进入 SQLite 或有 TTL 的本地 Capture 存储。

## 5. 领域模型

### 5.1 术语

| 术语 | 定义 |
|---|---|
| `ContextStore` | 一次安装唯一的本地 Git 知识仓库 |
| `ContextSpace` | 一项内部 Requirement 或长期工作目标的 Intent 与 Context 组织容器 |
| `IntentRevision` | ContextSpace 目标、范围和验收条件的一版完整快照 |
| `TaskSession` | 一次 Agent 工程任务的本地运行实例，与 Agent Session 隔离 |
| `WorkingIntentSnapshot` | Agent 对当前 Task 目标、方向、范围、约束、Hint 与自然形成问题的轻量、非事实工作理解 |
| `TaskIntentRevision` | TaskSession 中一个不可变 WorkingIntentSnapshot 版本及其 parent 关系 |
| `TaskSignal` | Prompt、Workspace、Diff、TestOutcome 等非事实、非定位工作线索；可影响 Working Intent retrieval，但不是工程 Evidence |
| `ArtifactFocusQuery` | 一次围绕 File、Module、Symbol、API、Schema 或 Test 检索历史 Context 的请求；只包含 Session/Intent CAS、absolute path、kind coordinates 和输出边界，不是持久 Task 状态 |
| `ResolvedFocus` | 服务端在单次 ArtifactFocusQuery 内通过 Catalog 得到的 RepositoryId + 完整 ArtifactLocator；只作为本次 Graph seed，不是领域身份或工程事实证明 |
| `TaskSpaceAssociation` | Task 与多个 Space 的派生相关性、置信度和匹配依据 |
| `WorkEpisode` | 一次 Task 内被聚合的探索、修改、验证和结论 |
| `ContextCandidate` | 从 WorkEpisode 自动生成、尚未进入知识事实层的 Context 草稿 |
| `ContextItem` | 一条稳定工程认知，例如 Decision、Contract、Risk 或 Validation |
| `ContextRevision` | ContextItem 的一版完整内容快照，不是文本 Patch |
| `SpaceAssociation` | Context 与一个 Primary Space 及若干 Related Space 的可修订显式关系 |
| `ContextRelation` | Context 之间的依赖、约束、实现、验证或冲突关系 |
| `EvidenceSnapshot` | 可以脱离开发现场独立阅读的最小充分证据 |
| `EngineeringReference` | Context 中保存的非权威工程定位观察，包含 RepositoryId 与完整 kind-specific ArtifactLocator |
| `EngineeringArtifact` | 当前代码树中可解析的 Module、File、Symbol、API、Schema 或 Test |
| `ContextArtifactAssociation` | Context 与 EngineeringArtifact 的派生、可失效、可重建关联 |
| `LifecycleTransition` | Context 激活、废弃或替代的不可变因果节点 |
| `Event` | Git 中实际保存的不可变 JSON 文件 |
| `Projection` | SQLite 根据 Event 和当前工程快照计算出的查询状态 |

### 5.2 ContextSpace 与 Intent

`ContextSpace` 是稳定 Requirement Intent 与 Context 组织容器，不是检索分区键，也不是 Workspace 的属性。

Intent 至少包含：

```yaml
title:
problem:
desired_outcome:
in_scope:
out_of_scope:
acceptance_conditions:
domain_terms:
```

Intent 变化时新增完整 `IntentRevision`。Revision 使用显式 Parent DAG；多个 Head 表示 Intent Conflict，必须通过引用全部 Head 的新完整 Revision 收敛。

完整 Intent 字段都进入 `space_intent_fts`，作为 Task 多路召回的一种信号，并与 Context、Scope 和工程关联共同推断候选 Space。

### 5.3 WorkingIntentSnapshot

`WorkingIntentSnapshot` 是本地、会话级、非事实的旁路快照，不进入 Git。仅 `goal` 必填，其余字段省略即为空：

```yaml
goal:
current_direction:
in_scope:
out_of_scope:
domains:
platforms:
constraints:
acceptance_conditions:
artifact_hints:
interface_hints:
open_questions:
```

真实目标、方向、范围或 Hint 变化时可旁路生成新 Revision；canonical 语义相同的 continue 返回 `already_current`。Hint 只参与文本检索，open_questions 只保存自然形成的问题。

同一个业务仓库中的不同 Agent Session 使用不同 `task_id`，互不覆盖当前 Intent 或候选 Space。

### 5.4 TaskSpaceAssociation

`TaskSpaceAssociation` 是检索结果，不是知识事实。一个关联至少包含：

```yaml
task_id:
space_id:
score:
matched_intent_fields:
matched_artifacts:
matched_contexts:
relation_paths:
reasons:
```

系统允许：

- 没有任何高置信度 Space；此时仍可按 Context 和工程关系检索。
- 同时关联多个 Space；不得强制选出唯一 Active Space。
- 同一个 Task 在开发过程中改变候选 Space 排序。
- 用户为当前 Task 提供显式修正；修正只写入 TaskSession，不形成 Workspace 全局配置。

### 5.5 ContextCandidate 与 ContextItem

`ContextCandidate` 保存在本地 Runtime 中，可以没有 Space。它至少包含：

```yaml
candidate_id:
task_id:
kind:
topic_key:
statement:
rationale:
applicability:
assumptions:
recheck_when:
evidence:
engineering_references:
related_contexts:
space_candidates:
novelty:
conflicts:
```

`space_candidates` 可以同时包含已有 Space 和一个系统生成的 `proposed_new_space_intent`。不存在合理的已有 Space 时，系统不得要求用户先退出当前流程手工创建 Space。

确认 Candidate 时：

1. 选择已有 Primary Space，或确认系统生成的新 Space Intent；可附加 Related Spaces。
2. 应用用户对内容的可选修正。
3. 如果选择新 Space，在同一个 Writer Batch 中先生成 `space.created`。
4. 在该 Batch 中生成 `context.created`、初始 `context.space_association_changed` 和 `context.lifecycle_changed(action=activate)`。
5. Candidate 本地状态标记为 Confirmed，随后按 TTL 清理。

Candidate 未确认前不进入 Git，也不能自动注入。

`ContextItem` 的身份独立于 Space。Space 组织关系由 `SpaceAssociation` 表达，因此错误归属可以通过新关联事件修正，而不需要改写 Context 历史或创建重复知识。

### 5.6 Context 类型与关系

Context 类型：

- `decision`
- `contract`
- `issue`
- `risk`
- `validation`
- `discovery`
- `progress`

`ContextRevision` 可以包含稳定的 `ContextRelation`：

```yaml
relations:
  - kind: depends_on | constrains | implements | validated_by | contradicts | related_to
    target_context_id:
    rationale:
```

ContextRelation 属于工程认知的一部分，参与 Revision 语义和检索图扩展。文件路径、Symbol 和 Commit 不属于 ContextRelation，而属于非权威 EngineeringReference。

### 5.7 Applicability

Applicability 是 Revision 内自包含的适用范围：

```yaml
domains:
platforms:
conditions:
```

它不引用用户配置或外部可变分类表。Scope 用于：

- Task Context 过滤和排序。
- 跨端 Context 发现。
- 相同 Topic 的潜在语义冲突检测。
- Candidate Space 推荐。

### 5.8 稳定 ID

领域实体使用随机、不透明 ID：

```text
spc_<random>   ContextSpace
ctx_<random>   ContextItem
rev_<random>   Intent 或 Context Revision
evt_<random>   Event
evd_<random>   Evidence
asc_<random>   SpaceAssociation
trn_<random>   LifecycleTransition
cnf_<random>   SemanticConflict
rsl_<random>   ConflictResolution
```

本地对象使用独立前缀：

```text
tsk_<random>   TaskSession
cnd_<random>   ContextCandidate
wep_<random>   WorkEpisode
```

禁止使用标题、路径、时间、用户名、外部单号或内容 Hash 作为领域 ID。

## 6. 事件与生命周期模型

### 6.1 事件类型

目标事件集合：

```text
space.created
space.intent_revision_added
context.created
context.revision_added
context.space_association_changed
context.lifecycle_changed
semantic_conflict.opened
semantic_conflict.resolution_added
```

TaskSession、TaskIntentRevision、WorkEpisode 和 ContextCandidate 不属于 Event。

### 6.2 Context 创建示例

```json
{
  "schema_version": "1",
  "event_id": "evt_7cc4...",
  "event_type": "context.created",
  "context_id": "ctx_aa91...",
  "revision": {
    "revision_id": "rev_a2f0...",
    "parent_revision_ids": [],
    "kind": "decision",
    "topic_key": "search-result/general-tab-visibility",
    "statement": "General Tab 的可见性由服务端响应字段决定",
    "rationale": "客户端本地推导会导致多端结果不一致",
    "applicability": {
      "domains": ["search-result"],
      "platforms": ["fe", "ios", "android"],
      "conditions": ["响应包含 general_tab_visible 字段"]
    },
    "assumptions": ["响应仍包含 general_tab_visible 字段"],
    "recheck_when": ["服务端重新定义字段语义"],
    "relations": [],
    "evidence": [
      {
        "evidence_id": "evd_1234...",
        "kind": "source_snapshot",
        "supports": "客户端直接消费响应字段",
        "content": {
          "response_fragment": {"general_tab_visible": false},
          "consumer_logic": "UI directly maps the field to tab visibility"
        },
        "interpretation": "未发现客户端本地计算规则",
        "limitations": ["不证明未来协议不会变化"]
      }
    ]
  },
  "annotations": {
    "producer": "codex",
    "engineering_references": [
      {
        "kind": "api",
        "locator": {
          "locator_kind": "api",
          "path": "api/search.yaml",
          "protocol": "http",
          "operation": "GET",
          "normalized_route": "/v2/search"
        },
        "relation": "consumes"
      },
      {
        "kind": "symbol",
        "locator": {
          "locator_kind": "symbol",
          "path": "src/search/SearchResult.tsx",
          "language": "typescript",
          "module": "search",
          "enclosing_type": "SearchResult",
          "symbol_name": "renderTabs",
          "signature": "renderTabs(): ReactNode"
        },
        "relation": "implements"
      }
    ]
  }
}
```

`annotations.engineering_references` 不参与 Context 身份、Revision 因果关系或生命周期归约，但索引器必须将其标准化为工程关联观察。

### 6.3 SpaceAssociation 因果图

Context 与 Space 的组织关系通过独立事件表达：

```json
{
  "schema_version": "1",
  "event_id": "evt_assoc...",
  "event_type": "context.space_association_changed",
  "context_id": "ctx_aa91...",
  "association": {
    "association_id": "asc_1234...",
    "previous_association_ids": [],
    "primary_space_id": "spc_search_ui...",
    "related_space_ids": ["spc_search_protocol..."],
    "reason": "该结论由搜索结果页需求产生，同时受搜索协议约束"
  }
}
```

Projection 规则：

- 一个合法 Head：当前 SpaceAssociation。
- 多个 Head：Association Conflict，阻止自动注入但不使 Context 内容失效。
- 新关联必须显式引用全部当前 Head 才能收敛冲突。
- Primary Space 可以修正；Context ID 和 Revision 历史不改变。
- Related Spaces 不表示复制或多份 Context，只用于组织、检索和解释。

### 6.4 LifecycleTransition 因果图

生命周期动作：

```text
activate
deprecate
```

Transition 显式引用：

- `context_id`
- `revision_id`
- `previous_transition_ids`
- `action`
- 可选 `superseded_by_context_id`

Projection 规则：

- 没有 Transition：未激活。
- 唯一 `activate` Head：该 Revision 为 Active。
- 唯一 `deprecate` Head：Context 为 Deprecated。
- 多个 Head：Lifecycle Conflict。
- 旧 Active Revision 被后继 Transition 替代时显示为 Superseded。
- 禁止按时间、Event ID、文件名或 Git Commit 顺序执行 Last-Write-Wins。

Candidate 确认生成首个 Activate Transition。

### 6.5 完整 Revision

- 新 Revision 保存完整快照，不保存文本 Patch。
- `parent_revision_ids` 表达内容演进关系。
- 单父 Revision 表示修订。
- 多父 Revision 表示显式合并。
- Revision Parent 必须属于同一 ContextItem。
- 一个 Revision Head 表示内容收敛；多个 Head 表示 Revision Conflict。
- Lifecycle 始终引用确定的 Revision ID，不引用“最新 Revision”。

### 6.6 语义冲突

系统对相同 `topic_key`、适用范围重叠且结论可能矛盾的 Active Context 生成冲突候选。FTS、规则或后续语义模型只能提出候选，不能直接改变生命周期。

确认后的语义冲突使用稳定 `conflict_id`，显式引用参与 Context、Revision 和 Lifecycle Head。未解决冲突阻止相关 Context 自动注入；查询必须同时展示各方及匹配原因。

Conflict Resolution 形成独立 DAG。只有唯一合法 Resolution Head 且覆盖所有当前相关 Lifecycle Head 时，冲突才能投影为 Resolved。

### 6.7 确定性校验

- 同一个稳定 ID 出现在多个定义中时，所有冲突定义一并失效。
- 一个 `context_id` 必须且只能有一个合法 `context.created`。
- SpaceAssociation、Lifecycle 和 Revision 的父引用必须存在并无环。
- ContextRelation 的目标 Context 必须存在；关系图允许普通环，但 `depends_on` 等需要无环的关系类型单独诊断。
- Evidence ID 在仓库内全局唯一。
- Quarantine 集合由完整事件集合确定，不依赖文件遍历顺序、SQLite RowID 或 Git Commit 顺序。
- 未知 Schema 事件保留在 Git 中并报告 Diagnostic，在 Reader 支持前不参与有效 Projection。

## 7. Evidence 与 Engineering Reference

### 7.1 Evidence 自包含原则

Evidence 必须在开发分支、Commit、业务代码仓库或 Agent Session 消失后仍能表达：

- 观察到了什么。
- 如何得到这个结果。
- 它支持哪条结论。
- 证据有哪些限制。

支持：

- `source_snapshot`：最小充分的代码、配置、协议或文档片段。
- `experiment_record`：实验前提、输入、步骤、期望和实际结果。
- `artifact_snapshot`：接口响应、测试结果或其他结构化材料。

Evidence 绑定 WorkObservation、CheckpointClaim、Candidate、ContextRevision 或 EngineeringReference 等工程断言。WorkingIntentSnapshot、TaskSignal、Hint 和 ArtifactFocusQuery 只提供工作理解或检索线索，不是 Evidence；Claim 显式引用的 owned Diff/TestOutcome 也必须先由 Builder 转换为自包含 EvidenceSnapshot，不能把 TaskSignal 原样提升为知识证据。

### 7.2 EngineeringReference

EngineeringReference 必须包含 RepositoryId 与一种 kind-specific、repo-relative 的确定性 ArtifactLocator。它是非权威工程观察，不能单独构成 Evidence。

标准 EngineeringReference：

```yaml
kind: module | file | symbol | api | schema | test
repository_id:
locator:
  locator_kind: symbol
  path: src/search/SearchResult.tsx
  language: typescript
  module: search
  enclosing_type: SearchResult
  symbol_name: renderTabs
  signature: "renderTabs(): ReactNode"
relation: implements | consumes | defines | validates | affected_by
supports:
limitations:
```

原则：

- File/Module 使用精确 Path；API、Schema、Symbol 与 Test 使用完整 kind-specific locator。
- locator 不使用内容相似度、版本摘要、Git 历史或重命名猜测。
- Reference 失效只影响检索能力，不影响 Context 内容和生命周期。
- 当前解析结果不得写回覆盖旧事件。

### 7.3 大型 Evidence 对象

大型文本证据保存在内容寻址对象：

```text
objects/sha256/ab/<digest>
```

事件引用 SHA-256、Media Type、Size、解释和限制。Writer 在提交前校验对象内容与摘要一致；新对象与引用它的 Event 必须在同一个 Batch 中提交。

## 8. Engineering Graph

### 8.1 图节点

Engineering Graph 是派生查询结构，节点包括：

- ContextSpace
- ContextItem / ContextRevision
- Repository
- Module
- File
- Symbol
- API
- Schema
- Test

边包括：

- ContextRelation：稳定知识关系。
- ContextArtifactAssociation：派生工程关系。
- Artifact-to-Artifact：contains、calls、implements、consumes、defines、validates。
- Context-to-Space：当前有效 SpaceAssociation。

Engineering Graph 由显式 build 产生，是独立于当前 Context Projection 的稀疏历史知识快照。Builder 从 EngineeringReference 的精确 `(ContextId, RevisionId)` 出发，只收录 roots 与 build-time ContextRelation 最多两跳的 closure；无 Reference 且不可达的 Context/历史 Revision 不进入 Graph SQLite 或 Artifact Generation。

每个 `GraphContextSnapshot` 固化：

- SpaceId 与构建时 Space title；
- immutable ContextId + 完整 ContextRevision；
- 构建时 lifecycle/governance status、Evidence completeness；
- typed automatic-safety blockers 与 eligibility；
- ContextRelation 的 build-time target SpaceId + ContextId + accepted RevisionId。

未接受、Evidence 不完整或存在治理/语义冲突的 Revision 可以保留给显式诊断，但 `automatic_injection_eligible=false`。Search 与 Agent Adapter 必须消费该 build-time safety，不得用当前 `context_item` 的 head、accepted、withdrawn 或 conflict 状态重新解释 Graph item。

### 8.2 Artifact 解析

业务代码仓库只读扫描生成 `EngineeringArtifact`。Scanner 的输入必须是有界、非空的 `RepositoryScanPlan`：

```yaml
scan_plan:
  repository_id:
  paths:                         # 去重后的 RepoRelativePath，至少一项
repository_snapshot:
artifact_id:
kind:
locator:
  locator_kind:
  path:
  # 其余字段由 Artifact kind 决定
```

持久 `EngineeringReference` 的每个 kind-specific `ArtifactLocator` 都包含明确的 `RepoRelativePath`。`association_rebuild` 先按 RepositoryId 分组 Reference，再收集并去重这些 path，最后只读取计划中的文件；不得调用 `git ls-files` 枚举 Repository，不得遍历目录，也不得在空计划或 missing path 时回退为全仓扫描。显式 `repository_scan` 使用相同计划约束。

计划内路径仍依次执行 canonical path、tracked-only、symlink escape、敏感/generated/vendor、单文件大小、文件数和总字节预算检查。新建或其他 untracked 文件不进入 Graph；这属于 #150 的确认边界。Task Retrieval 只消费已经构建的 Engineering Projection，一次 query 不运行 Scanner 或 `association_rebuild`。

内部增量解析可以使用私有 Git blob OID 或 file-version digest，但它们不进入领域对象、关联证据、Graph ranking、RetrievalPath 或 MCP response。

确定性解析：

1. Repository 不可访问：`unavailable`。
2. 完整 ArtifactLocator 无匹配：`missing`。
3. 完整 ArtifactLocator 对应多个当前对象：`ambiguous`。
4. 完整 ArtifactLocator 唯一匹配：`resolved`，并且只有该状态建立 ContextArtifactAssociation。

文件移动或 Symbol 改名后，旧 Reference 保留不变并变为 `missing`；系统不查询 Git rename/copy history，不生成 relocation candidate，也不要求会话 Agent 修复关联。

### 8.3 ContextArtifactAssociation

派生关联至少包含：

```yaml
context_id:
revision_id:
artifact_id:
relation:
confidence:
sources:
resolution_reason:
repository_snapshot:
```

`sources` 可以来自：

- Context Event 中的 EngineeringReference。
- WorkEpisode 中的文件和 Symbol 观察。
- 当前 Task Diff。
- API、Schema 和调用关系传播。
- 多个独立信号的一致匹配。

关联置信度只参与召回和排序，不能改变 Context 生命周期或自动注入资格。

### 8.4 图扩展边界

Task Retrieval 默认只扩展一至两跳：

```text
当前 Symbol
→ API / Schema
→ Contract Context
→ Validation / Decision Context
```

每条返回结果必须携带完整 `retrieval_path`。无路径解释的远距离关联不得自动注入。

## 9. Git Store

### 9.1 用户目录布局

```text
~/.shared-context/
├── config.toml
├── bin/
│   └── current/sctx
├── repository/
│   ├── .git/
│   ├── events/
│   ├── objects/
│   └── schemas/
├── state/
│   ├── index.sqlite
│   ├── runtime.sqlite
│   ├── repository-registry.sqlite
│   ├── writer.lock
│   ├── index.lock
│   ├── runtime.lock
│   ├── pending/
│   ├── capture/
│   └── authorized-session-scopes/
├── backups/
└── logs/
```

只有 `repository/` 是 Git 仓库。

- `index.sqlite`：可从 Git 和当前工程快照重建的知识及关联投影。
- `runtime.sqlite`：Task、WorkEpisode 和 Candidate 短期状态，不是知识事实。
- `capture/`：受 TTL 和容量限制的临时 Evidence/Observation 材料。
- `authorized-session-scopes/`：按 external Session locator digest 隔离、Catalog-bound、TTL-bounded 的私有 activation lease；不是 Task/Context 事实。
- `config.toml`：固定 Context Store 与本机显式 Repository Catalog，不包含 Workspace-to-Space 映射。

Repository Catalog 的配置语义：

```toml
[[repositories]]
id = "rpo_<uuid-v4>"
paths = ["/absolute/canonical/checkout", "/absolute/canonical/worktree"]
```

- Catalog 是 RepositoryId 的唯一权威；新 ID 只能由 `repository add` 使用现有 typed `RepositoryId::new()` 格式生成。
- 一个 ID 可有 `0..N` paths；同一 path 只能属于一个 ID。多个 worktree/checkout 是否同一逻辑 Repository 由显式配置决定。
- basename、remote、Git common-dir、父 Workspace 和 sibling 目录都不是 identity 或合并依据。
- `repository-registry.sqlite` 是可删除投影；setup、doctor、显式 Runtime open 从 Catalog 原子恢复相同 ID/locators。
- 运行时文件解析只在允许 Workspace 与 configured checkout 的交集内执行 canonical longest-prefix；输出 RepositoryId + RepoRelativePath。未配置路径为 typed `repository_not_configured`，Workspace 外与 symlink traversal 拒绝。
- Catalog 仅本机有效，不做团队同步，也不承载 Space/Requirement 关系。

### 9.2 仓库结构

```text
repository/
├── events/
│   └── <id-prefix>/evt_<random>.json
├── objects/
│   └── sha256/<hash-prefix>/<digest>
└── schemas/
    └── <schema-id>.json
```

约束：

- 每个 Event 一个文件。
- Event 路径由 Writer 生成，调用者不能传入。
- 路径只负责物理分片，不表达 Space、状态或事件顺序。
- 不维护可修改的全局清单、计数器或当前状态文件。
- Schema 使用不可变版本文件。

## 10. SQLite 与本地 Runtime

### 10.1 index.sqlite

核心表：

| 表 | 用途 |
|---|---|
| `meta` | Context Tree、Projection Generation 和实现版本 |
| `source_file` | Git Path、Blob OID、解析状态 |
| `space_projection` | 当前 Space Intent 投影 |
| `intent_revision` / `intent_head` | Intent Revision DAG |
| `space_intent_fts` | 完整 Intent 全文索引 |
| `context_item` | Context 逻辑实体 |
| `context_revision` / `context_head` | Context Revision DAG |
| `space_association` / `space_association_head` | Context 的 Space 组织关系 |
| `lifecycle_transition` / `lifecycle_head` | Context 生命周期因果图 |
| `context_relation` | ContextRevision 中的稳定关系 |
| `evidence` | Evidence Snapshot 和对象引用 |
| `scope` | Applicability 结构化索引 |
| `semantic_conflict` / `conflict_resolution` | 语义冲突与 Resolution DAG |
| `engineering_reference` | Git Event 中的非权威工程定位观察 |
| `engineering_artifact` | 当前业务代码快照中的工程对象 |
| `artifact_resolution` | EngineeringReference 的当前解析结果 |
| `context_artifact_association` | Context 与工程对象的派生关联 |
| `context_fts` | Statement、Rationale、Evidence、Relation 的 FTS5 索引 |
| `diagnostic` | 可由当前输入重建的非法事件、悬空引用和关系环 |

### 10.2 runtime.sqlite

核心表：

| 表 | 用途 |
|---|---|
| `task_session` | Agent Session 对应的 Task 状态 |
| `task_intent_revision` | `WorkingIntentSnapshot` 的不可变 `TaskIntentRevision` 链 |
| `task_signal` | Prompt、Workspace、Diff、TestOutcome 等非定位信号 |
| `task_space_association` | 当前多 Space 推断结果和原因 |
| `work_episode` | Task-owned、version-CAS 的 Open/Closed Episode boundary |
| `work_episode_intent_ref` / `work_episode_signal_ref` | ordered Intent Revision 与非定位 Signal provenance |
| `work_observation` / `work_observation_source` | server-owned normalized meaning 与 typed sources |
| `capture_ingestion` | `CaptureId → Episode/Observation` 唯一幂等记录 |
| `agent_checkpoint` | Episode parent-version 语义幂等的完整 Claims/Unknowns、server IDs 与 continue/close boundary |
| `candidate_build` / `candidate_build_item` | closed Episode/Claim-scoped Builder reservation、submission identity 与#117结果 |
| `candidate_analysis` | 可删除重算、按CandidateId替换的current derived review JSON与固定Context/Graph generations |
| `work_episode_diagnostic` | unconfigured/unsafe Capture Artifact 映射诊断 |

Capture 文件设置 TTL；Runtime Episode 不进入 Git。删除 `runtime.sqlite` 只会丢失 Task/Episode，不改变 Context Git、Index 或 `state/capture`。

### 10.3 投影 Generation

查询响应同时携带：

```text
context_tree_oid
projection_generation
artifact_generation
task_intent_revision_id
task_signal_fingerprint
```

一次 TaskContextPack 必须在一个固定的当前知识 Projection、一个固定 `TaskIntentRevision` 和至多一个固定 EngineeringGraphSnapshot 上生成，禁止拼装多个 Graph Generation。`indexed_tree_oid` 是当前 Context Tree；`graph_context_tree_oid` 是 Graph build provenance；二者允许不同。`context_tree_oid` mismatch 不影响 Graph eligibility，也不触发自动 rebuild。

### 10.4 重建

`index.sqlite` 重建输入分为：

1. Context Git 当前 Tree：重建稳定 Projection、FTS、EngineeringReference。
2. 当前可访问业务代码仓库：重建 EngineeringArtifact、Resolution 和 Association。

业务仓库不可访问时，知识 Projection 仍必须可用；Artifact Resolution 标记为 unavailable，检索退化到 Intent、Context、Scope 和稳定 ContextRelation。

## 11. Task-first Retrieval

### 11.1 TaskContextRequest

内部检索请求接收当前 Task，而不是 Space：

```yaml
task_id:
working_intent:                 # 当前 TaskIntentRevision 的 WorkingIntentSnapshot
task_signals: []                # 非事实、非定位工作线索
resolved_focus:                 # 仅 task_artifact_focus 本次请求可提供
token_budget:
max_spaces:
candidate_limit:
mode:
```

公开只读 `task_context` 只接收 ExternalSessionLocator 与输出边界，由 Runtime 钉定 ActiveTask、当前 `TaskIntentRevision` 和 Active TaskSignals。`task_intent_update` 只接收 Session locator、Task boundary、Revision CAS 与 `WorkingIntentSnapshot`。`task_artifact_focus` 另行接收绝对路径和 kind-specific coordinates，由服务端构造本次 `ResolvedFocus`。这些入口都不接受 `space_id`。

### 11.2 Working Intent 更新

更新边界：

- 第一次 `task_intent_update`：显式创建 TaskSession 和首个 `TaskIntentRevision`，只要求 Working Intent `goal`。
- Agent 后续调用 `task_intent_update`：旁路提交当前自然形成的 `WorkingIntentSnapshot`、Task boundary 与 Revision CAS；其余字段可省略。
- 目标、方向、范围、约束、验收条件、Hint 或自然形成的问题变化时，Agent 可以提交新 Revision；Runtime 不从 Prompt、Diff 或相似度自动推断完整 Working Intent。
- Agent 需要围绕某工程对象查询历史时调用只读 `task_artifact_focus`；Catalog 为本次请求解析 `ResolvedFocus`，不会创建 `TaskIntentRevision` 或任何 Focus Runtime 状态。
- PreCompact、TurnStop 只在 Agent 已自然形成更新时旁路记录 Working Intent，并只基于已有 Checkpoint 推进 WorkEpisode lifecycle。

`task_boundary=new` 只能由 Agent 显式声明；Runtime 只执行 ActiveTask 切换，不根据 Prompt 相似度猜测任务边界。`continue` 使用调用方的 parent Revision CAS，由 Runtime 原子判断 `created`、`already_current` 或 stale。

### 11.3 多路召回

候选来源：

1. EngineeringArtifact 直接关联的 Active Context。
2. API/Schema、调用关系和 ContextRelation 扩展的 Context。
3. Working Intent 通用文本与 Space Intent、Context 的结构化和 FTS 匹配。
4. `artifact_hints` / `interface_hints` 通过独立 `WorkingIntentHintText` channel 查询 Space Intent FTS 与安全 Accepted Context FTS；Hint 不解析 Artifact，也不产生 Graph 或 Evidence 语义。
5. Platform、Domain、Condition、Kind 的结构化匹配。

候选 Space 由上述 Context 和 Intent 信号共同推断，不能先选 Space 再搜索。

### 11.4 排序

排序优先级：

```text
工程对象直接关联
> API / Schema / Contract 关联
> 稳定 ContextRelation
> Space Intent 匹配
> Applicability 匹配
> Context FTS
> Evidence 完整度
> 稳定 ID Tie-breaker
```

创建时间、文件名、Git Commit 顺序和随机置信度不能成为最终 Tie-breaker。

### 11.5 TaskContextPack

返回：

```yaml
task_intent_revision_id:
candidate_spaces:
  - space_id:
    score:
    reasons:
contexts:
  - context_id:
    revision_id:
    primary_space_id:
    related_space_ids:
    statement:
    applicability:
    retrieval_paths:
    match_reasons:
    lifecycle:
    conflicts:
omitted:
```

每条 `retrieval_path` 必须说明 Working Intent 文本、结构化 Applicability 或本次 `ResolvedFocus` 如何连接到 Context。例如：

```text
SearchResult.tsx
→ consumes search-v2.general_tab_visible
→ Contract ctx_protocol
→ constrains Decision ctx_visibility
```

### 11.6 自动注入

只有同时满足以下条件的 Context 可以自动注入：

- 唯一 Active Lifecycle Head。
- 唯一 SpaceAssociation Head。
- 无 Revision Conflict。
- 无未解决且阻断的语义冲突。
- Evidence 满足最低完整度。
- Retrieval Path 可解释且达到最小相关性阈值。

Candidate、Deprecated Context、Annotation 和原始 Capture 内容只能作为不可信参考数据，不能提升为指令。

## 12. Low-tax Capture

### 12.1 WorkEpisode

WorkEpisode 聚合一次 Task 中的：

- TaskIntentRevisions 及其 WorkingIntentSnapshots。
- 检索过的 Context 及采用/忽略原因。
- 访问和修改的 Artifact。
- Diff 摘要。
- API、Schema 和调用关系观察。
- 测试、实验和验证结果。
- Agent Checkpoint。
- 未解决问题。

原始 Transcript 和完整 Tool Output 不进入 WorkEpisode。Runtime 只保存经过 Adapter 归一化、Secret/PII 扫描和容量限制的结构化观察。

#156/#157 当前可执行基础：

- `CaptureId` 是 typed `cap_<uuid-v4>`；Capture record 固化 ExternalSessionLocator、optional exact ActiveTask owner、redacted summary/file hints、TTL/privacy diagnostics 与 idempotent claim。
- CaptureStore 提供 bounded read/list/claim/cleanup；无 Session/ActiveTask 的 Capture 保留 `no_active_task` 诊断并不可 claim，跨 Task/Episode claim 拒绝。
- WorkEpisode 一 TaskSession 最多一个 Open 实例；open/read/list、显式 refs advance、normalized append、Capture ingestion、close preparation 和 source verification 使用 Episode version CAS。
- `capture_ingestion.capture_id` 唯一；claim 与 Runtime commit 任一侧崩溃都可重试且不产生重复 Observation。
- Catalog 只把 safe existing configured File hint 映射为 File ArtifactRef；unconfigured/unsafe path 保留 Capture source、summary 和 typed diagnostic，不猜 Repository。
- Hook 只 Capture，不自动 open/ingest Episode，也不生成 Claim。#163 的 AutomatedEpisodeBoundary 只能在工作 Agent 已写入 current-Intent Checkpoint 后补齐 ordered refs、关闭 Episode 并调用共享 Builder。
- `task_checkpoint` 由工作 Agent 显式提交 Task/TaskIntentRevision/Episode CAS、完整 Claims/Unknowns 和 typed refs；Runtime 在一个事务中 open/advance Episode、生成 inline Validation Observation、Claim/Checkpoint ID，并 continue 或 close。
- `(episode_id, parent_episode_version)` 唯一约束配合完整语义 JSON 实现 timeout retry；相同内容返回原 Checkpoint，不同内容冲突，stale version 拒绝。Checkpoint 不生成 Candidate 或 Git Event。

### 12.2 AgentCheckpoint

Agent 在 PreCompact、TurnStop 或形成重要结论时调用：

```yaml
agent_kind:
external_session_id:
expected_task_id:
expected_intent_revision_id:
expected_episode_version:
boundary: continue | close
claims:
  - statement:
    rationale:
    applicability:
    assumptions:
    recheck_when:
    evidence:
    artifact_refs:
    related_contexts:
unknowns:
```

Checkpoint 表达 Agent 已形成的工程认知，不是“工具执行成功”日志。Evidence 绑定 CheckpointClaim，并可复用 owned WorkObservation、同一 Index snapshot 的 ContextEvidence，或提交经 PrivacyScanner 检查的 self-contained inline Validation。TaskSignal 即使被 Claim 显式引用也仍是非事实来源线索：Prompt/Workspace 不可转成工程 Evidence，normalized Diff/TestOutcome 必须先由 Builder 组装为带 supports、content、interpretation 和 limitations 的 EvidenceSnapshot。ArtifactRef 只描述关联，不独立成为 Evidence。Unknown-only Checkpoint 可以 continue 或 close，但不产生知识主张。

### 12.3 Candidate Builder

Pipeline：

```text
WorkEpisode
→ 聚合 Claim
→ 组装最小充分 Evidence
→ 检索已有 Context
→ 判断重复、支持、修订、矛盾或新增
→ 推断 Applicability
→ 推断候选 Space
→ 生成 ContextCandidate
```

Candidate Builder 必须输出：

- 内容来源和 Evidence。
- 与已有 Context 的相似或冲突关系。
- 推荐的 Primary/Related Spaces 及原因。
- 置信度、未知项和需要重新检查的条件。

### 12.4 Candidate 确认

确认接口只要求：

```yaml
candidate_id:
confirmed_primary_space:
  existing_space_id:              # 与 new_space_intent 二选一
  new_space_intent:
confirmed_related_space_ids:
optional_edits:
```

用户不需要重新填写 Statement、Rationale、Applicability 和 Evidence。确认新 Space 时，Writer 在同一 Batch 中原子生成 Space、Context、SpaceAssociation 和 Lifecycle Events；确认失败不得留下部分领域事实。

## 13. CLI、MCP 与 Agent Adapter

### 13.1 CLI

```text
sctx setup
sctx doctor
sctx space create|revise|list|get
sctx task context|artifact-focus|checkpoint|intent update|signal supersede
sctx candidate list|get|confirm|discard
sctx context get|search|revise|deprecate
sctx repository add|list|doctor|scan
sctx repository group add|update|remove|list|doctor
sctx engineering-reference record
sctx association explain|rebuild
sctx index rebuild
sctx pending list|commit|move-aside
sctx demo seed|run
sctx hook --agent cursor|codex
sctx mcp serve --client cursor|codex
```

### 13.2 MCP Tools

| Tool | 类型 | 说明 |
|---|---|---|
| `task_intent_update` | 读/写本地状态 | CAS 写入 `WorkingIntentSnapshot` 的 `TaskIntentRevision` 并生成多 Space TaskContextPack |
| `task_artifact_focus` | 只读 | 钉定 ActiveTask 与 Intent Revision；仅接收 Session、absolute path 与无 path coordinates，由 Catalog 补全本次 `ResolvedFocus` 并立即返回 TaskContextPack，不保存 ID 或生命周期 |
| `task_signal_supersede` | 读/写本地状态 | 按稳定 Signal ID 失效当前 Task 信号 |
| `task_context` | 只读 | 按 external Session locator 重读已有 ActiveTask 的 TaskContextPack |
| `repository_scan` | 读/写本地状态 | 只扫描已在 Catalog 配置的 checkout，要求显式非空 repo-relative paths，并返回有界 Artifact/skip 摘要；不接受 RepositoryId 注入 |
| `engineering_reference_record` | 写知识事实 | 为已有 Context Revision 记录有证据的工程定位观察 |
| `association_explain` | 只读 | 展示解析状态、证据、歧义候选和 Graph Paths，不代选 |
| `association_rebuild` | 写派生状态 | 从 Git References 派生按 Repository 分组去重的 ScanPlan，并据此原子重建或诊断投影 |
| `task_checkpoint` | 写本地状态；close 或后续 verified lifecycle boundary 可写 Candidate Event | 提交结构化 Claim、Evidence Ref 和未知项；显式 close 或 Hook 对已持久 Checkpoint 的 close 触发同一确定性 Candidate Builder |
| `candidate_list` | 读 | 查看当前 Task 自动生成的 Candidate |
| `candidate_get` | 读 | 获取一个自动 Candidate 的完整、不可信 Review 内容 |
| `candidate_discard` | 写本地状态 | 在 Task/Intent/Review CAS 下显式放弃 Pending Review；不写知识事实 |
| `candidate_confirm` | 写知识事实 | 在显式人工选择、Task/Intent/Review CAS 与完整分析下，原子确认 Candidate、Primary/Related Space、Context Revision、Association 与 Publish |
| `context_search` | 读 | 面向诊断和显式探索的结构化搜索 |
| `context_get` | 读 | 获取确定 Context Revision、Evidence 和关系 |
| `space_search` | 读 | 显式查找 Space，不参与默认 Task 路由 |

Candidate 内容由 WorkEpisode 和 AgentCheckpoint 生成。

### 13.3 Canonical Agent Event

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

Canonical Context 至少携带：

```text
agent
session_id
cwd
workspace_roots
prompt（仅 PromptSubmit）
tool_name / outcome / normalized_file_hints（仅 PostToolUse）
```

Adapter 只翻译厂商 Payload。Working Intent、TaskIntentRevision、Git Diff、代码扫描、检索和 Candidate Builder 属于共享 Runtime，不得复制进厂商 Adapter。

### 13.4 Repository 准入与动态检索策略

- SessionStart：先用本地 Catalog 与 `AuthorizedSessionScopeStore` 同步、non-blocking 地解析 exact locator。Enabled 只注入固定 bounded activation marker，不构造或执行 Context Pack，也不注入知识项或 Space 摘要；Disabled 返回 neutral。先成功落盘的 locator decision 后续只读复用，不因 cwd 改变而重判。
- PromptSubmit：不重复 activation marker、不从 Prompt 文本构造 Intent 或 Signal，也不访问 Runtime/Search；SessionStart marker 是工作 Agent 进入完整 Shared Context 流程并显式调用 `task_intent_update` 的本地准入信号。MCP Server 已强制 current Enabled Session lease，条件 Skill loader 仍未实现。
- PostToolUse：Enabled 事件先完成全部结构化路径的安全校验与 Catalog 归属。全部目标可安全定位到已登记 checkout 时保留真实 Breadcrumb；安全 sibling/unregistered、mixed 或单 workspace 无法安全表达的多 registered checkout 整条降级为无路径的 non-locating Breadcrumb；ambiguous/relative/missing/symlink/特殊文件等 unsafe 输入整条丢弃。可识别 Test/Check/Lint 工具在 registered 或 non-locating 分支都可合并非定位 TestOutcome。File Hint 不再写入工程 TaskSignal，也不会隐式发起 ArtifactFocusQuery；Prompt 前没有 Session 时不隐式创建，不保存原始 Tool Output、Transcript 或命令文本。
- PreCompact：工作 Agent 先显式提交完整 current-Intent Checkpoint；Hook 不从摘要伪造 Claim，只在该 Checkpoint 存在时原子关闭 Episode 并调用共享 Candidate Builder。缺失或 stale Checkpoint 时 Episode 保持 Open，并返回修正提示。
- TurnStop：执行同一 AutomatedEpisodeBoundary；重复、乱序和并发事件复用 Closed Episode 与稳定 Build/Submission/Candidate 身份。Builder 暂时失败时 Hook fail-open，后续重复事件或显式 CLI 可恢复。
- SessionEnd：Enabled 时只清理过期 Capture/Review 状态；无论 Enabled/Disabled 都在业务动作后 non-blocking 地移除 exact locator lease。它不关闭 Episode、不生成 Candidate，也不清理其他 locator。

Agent Hook 不支持某事件时，通过 MCP 主动调用和 CLI 完成同一核心流程；能力差异只影响自动化程度，不改变领域模型。

若工作 Agent 已成功提交 `continue` Checkpoint，但 Hook 随后缺失、fail-open 或结果不确定，则用同一个 `task_checkpoint` 提交 `boundary=close`、当前 Episode version 和空 Claims/Unknowns。Runtime 在 Task/Intent/Episode CAS 下关闭既有 Checkpoint并调用 Builder，不创建第二个 Checkpoint，也不要求重新填写认知。

## 14. Git Writer 与一致性

### 14.1 唯一写入口

所有知识事实写入统一调用 Rust Writer：

```rust
append_batch(events, objects)
```

调用者不能指定：

- Event 文件路径或 Event ID。
- Git Parent、Tree、Commit 或 Ref。
- Update、Delete、Rename 操作。

Candidate Confirm 需要写入多个事件时，必须处于同一个 Batch Journal 和 Git Commit 中。

### 14.2 追加保护

Writer：

1. 生成随机 Batch/Event ID 和目标路径。
2. 在 `state/pending/<batch_id>/files/` 生成并 fsync 内容。
3. 校验 Schema、引用、领域不变量和敏感信息。
4. 原子写入 Batch Journal。
5. 获取 Writer Lock，检查受管文件和 Git Index。
6. 使用 `create_new(true)` 创建目标文件。
7. 只暂存本 Batch 的明确路径。
8. 复核 Staged Diff 全部为 A 且与 Journal 一致。
9. Commit 后更新 Projection，最后清理 Journal。

禁止 `git add .` 和 `git add -A`。

### 14.3 异常恢复

| 场景 | 处理 |
|---|---|
| Event 已准备、Commit 失败 | 保留 Pending 和 Journal；下次 Writer 按 Path/Hash 恢复同一批内容 |
| Commit 成功、Projection 更新失败 | Git 仍为事实；下次查询补建 |
| 部分文件已提交或 Hash 不一致 | 停止自动恢复并报告 Diagnostic |
| 已有受管文件 M/D/R | 拒绝产品自动提交，不覆盖用户文件 |
| SQLite 损坏 | 隔离损坏文件并重建 |
| 多个 Revision/Association/Lifecycle Head | 显式 Conflict，不使用 Last-Write-Wins |
| Runtime SQLite 损坏 | 重建空 Runtime；已确认 Context 不受影响 |

### 14.4 索引同步

`index.sqlite` 保存：

- `indexed_tree_oid`
- `projection_generation`
- `artifact_generation`
- DB、Parser、Reducer、Tokenizer、Association、Ranking 实现版本

每次查询前同步 Context Git Tree。知识投影更新在一个 SQLite Transaction 中切换；查询在固定 Snapshot 中完成。当前业务代码仓库变化只推进 Artifact Generation，不改变 Context Projection Generation。

## 15. 安全与隐私

- 原始 Conversation/Transcript 默认不写入 Git 或 Runtime SQLite。
- 原始 Tool Output 默认不保存，只保留归一化 Observation、最小 Evidence Snapshot 或摘要。
- Capture、Checkpoint 和 Candidate 写入前执行 Secret/PII 扫描。
- Event 和 Evidence 进入 Git 前再次扫描。
- Context 内容按不可信数据处理，不执行其中的命令或脚本。
- Candidate、Annotation、EngineeringReference 和检索分数不得提升为系统指令。
- 只有满足自动注入门槛的 Active Context 可以进入 Hook Additional Context。
- 本地业务代码扫描遵循显式 Allowlist/Denylist，并限制读取范围、文件大小和二进制类型。
- Capture 使用 TTL、单条大小和总容量上限。
- Git Store 位于当前用户目录，不宣称抵御当前用户主动篡改。

## 16. Rust 模块与分发

### 16.1 Rust Workspace

```text
crates/
├── domain
├── event-schema
├── git-store
├── index
├── task-runtime
├── engineering-graph
├── retrieval
├── candidate-builder
├── local-state
├── agent-adapter
├── adapter-cursor
├── adapter-codex
├── mcp
├── installer
└── cli
```

职责：

- `domain`：Space、Context、Revision、Association、Lifecycle 和 Conflict 不变量。
- `event-schema`：Event JSON 解析和校验。
- `git-store`：不可变事件、对象、Batch 和 Git Commit。
- `index`：知识 Projection、FTS、增量同步和重建。
- `task-runtime`：TaskSession、WorkingIntentSnapshot、TaskIntentRevision、非事实非定位 TaskSignal 和 WorkEpisode；不持久化 Artifact Focus。
- `engineering-graph`：Artifact 扫描、Resolution、Association 和图扩展。
- `retrieval`：Task 多路召回、排序、解释和 Context Pack。
- `candidate-builder`：Claim/Evidence 聚合、去重、冲突与 Space 推荐。
- `local-state`：runtime.sqlite、Capture、TTL、隐私扫描和配置。
- `agent-adapter`：厂商无关的 Canonical Event/Action。
- `adapter-*`：Cursor/Codex Payload 和配置适配。
- `mcp`：stdio MCP Server。
- `installer`：Setup、Backup、Doctor、Uninstall。
- `cli`：统一二进制入口。

### 16.2 NPM 分发

```text
@company/shared-context
@company/shared-context-darwin-arm64
@company/shared-context-darwin-x64
```

- 主包提供极薄的 JavaScript Launcher。
- 平台包通过 `optionalDependencies`、`os`、`cpu` 选择 Mach-O。
- 运行期协议是 CLI、Hook 和 MCP stdio，不依赖 N-API。
- 运行期不依赖 Python、Homebrew 或远端模型服务。
- 平台二进制签名并在包内提供摘要。
- arm64/x64 分别提供自包含离线 Bundle。

Agent 配置引用：

```text
~/.shared-context/bin/current/sctx
```

升级通过原子切换 `bin/current` 完成。

## 17. 安装、Demo 与 Doctor

### 17.1 Setup

```bash
npx -y @company/shared-context@<version> setup --agents cursor,codex
```

Setup：

1. 检查 macOS、CPU、Git、签名和磁盘空间。
2. 创建 `~/.shared-context/` 和唯一 Context Git Store。
3. 创建或重建 `index.sqlite` 与 `runtime.sqlite`。
4. 检测 Cursor/Codex 能力。
5. 展示并原子合并 Hook/MCP 配置。
6. 运行 Git、SQLite、MCP、Adapter 和 Task Runtime Smoke Test。
7. 显示需要重启或 Trust 的 Agent。

Setup 不要求选择业务仓库或 Space。业务 Workspace 只提供非定位 TaskSignal/Breadcrumb，不自动生成 Artifact Focus。

### 17.2 Demo

```bash
sctx demo seed
sctx demo run
```

Demo 验证：

1. 创建多个具有不同 Intent 的 Space。
2. 创建跨 Space 的 Contract、Decision 和 Validation Context。
3. 在不传 Space ID 的情况下提交一个 FE Task。
4. 根据 Prompt、文件、Symbol 和 API 信号推断多个 Space。
5. 返回带 Retrieval Path 的 TaskContextPack。
6. 记录 Checkpoint 和测试观察。
7. 自动生成未归属 Candidate。
8. 确认 Candidate 并生成 Context、SpaceAssociation 和 Lifecycle Events。
9. 删除并重建 SQLite 后得到相同知识 Projection，并重新解析工程关联。

### 17.3 Doctor

```bash
sctx doctor
sctx doctor --json
```

检查：

- Context Git Store 和 `git fsck`。
- 已有受管文件 M/D/R 和 Pending Batch。
- Event Schema、Revision、Association、Lifecycle 和 Conflict DAG。
- `index.sqlite`、`runtime.sqlite`、Generation 和 FTS。
- EngineeringReference 解析率和过期关联。
- Cursor/Codex 配置、MCP 和 Hook 能力。
- Capture TTL、容量和隐私扫描状态。

`doctor --fix` 只执行安全、可重建的修复，例如重建 SQLite 和工程关联；不得自动改写 Git 事实或确认 Candidate。

### 17.4 Uninstall

默认移除：

- 自己可精确识别的 Agent 配置条目。
- Runtime、日志、Capture 和可重建 SQLite。

默认保留 Context Git Store。删除知识数据使用独立命令，并展示绝对路径和二次确认。

## 18. 实施里程碑

### M1：Task-first 领域与入口基础 — 已实现

- `WorkingIntentSnapshot`、`TaskIntentRevision` 与非事实非定位 `TaskSignal` 已建模；Working Intent 不携带 Space 或 Workspace 路由，TaskSignal 不是工程 Evidence，Artifact Focus 是查询参数而非 Task 领域状态。
- `TaskSpaceAssociation` 已作为独立派生类型建模，集合允许零个或多个 Space。
- `WorkEpisode` 与无 Space 的 `ContextCandidate` 已建模。
- 已删除 Workspace-to-Space 配置、绑定命令、preferred-Space 请求字段和对应排序逻辑。
- Task-first 领域不携带 Space 路由；`context_search.space_ids` 仅作为显式探索的硬过滤。
- Candidate 创建身份与路径由服务端拥有；公开 MCP/CLI 不提供手工 Candidate submission，只有 M4 Builder 可调用内部 #117 admission。
- Candidate 使用独立投影，不进入 Context FTS，也不满足自动注入资格。
- 跨 crate M1 验收覆盖 WorkingIntentSnapshot/TaskIntentRevision、`0..N` 关联、无 Space Candidate、无 Workspace 路由和自动注入隔离；历史测试函数名中的 `task_intent` 仅保留为测试标识。

M1 复用此前已有的 Git Writer、Reducer、SQLite Context 投影、生命周期、CLI/MCP、Agent Adapter、安装器与 NPM 基础设施。复用这些基础设施不表示下面的目标里程碑已经完成。

### M2：Task Runtime 与多 Space Retrieval — 已实现

- `runtime.sqlite`、TaskSession、TaskIntentRevision 及并发线性 Head 已实现。
- 完整 Space Intent FTS、Task 多路召回、Space 关联推断、解释路径和 Session 隔离已实现。
- `task_intent_update` 按 external Session Locator 与 Revision CAS 更新 Runtime；只读 `task_context` 仅重取固定 Task Revision 与知识 Projection 上的 TaskContextPack。
- SessionStart 在模型推理前以本地 Catalog/lease 决定 Direct、显式 Group 或 Disabled；Enabled 才返回固定 bounded marker。PromptSubmit 始终 neutral，不重复 marker、不访问 Runtime 或 Search。
- Enabled PostToolUse 把 lease 作为 Session-level 准入，再做全事件路径安全与 Catalog 归属：registered cross-Repo 保留真实 mapping，安全 sibling/mixed/unrepresentable multi-Repo 保存 path-free non-locating Capture/TestOutcome，unsafe 事件保持零 Capture/Signal/Report/Git residue。结构化 TestOutcome 可进入 Task fingerprint，但不参与 FTS 或 qualified Test 匹配，也不独立产生工程关联。
- M2 跨 crate/E2E oracle 已证明严格 Intent 更新、只读 Locator 请求、无 Space 路由、`0/1/N` Space、同 Workspace Session 隔离、PostTool Signal 生命周期，以及 Tree/Generation/fingerprint 一致性。
- Workspace 位置 observation 保留在 Session 但不参与 FTS 或 Task fingerprint；裸 Repository/File 工程 Signal 已删除。
- Cursor Prompt 仍为显式 MCP；Symbol/Diff/API/Schema 的代码扫描、解析和关系扩展属于 M3，不冒充 M2 RetrievalPath。
- 固定 expected `fixtures/m2/repository-scoped-activation-v1.json` 与真实文档化 Codex/Cursor payload 验收 Direct、显式多成员 Group、Disabled、Catalog unavailable、resume/compact、并发重复 SessionStart、Prompt 前 marker 顺序以及完整生命周期残留；expected 不由 production 输出生成。
- MCP Server authorization 已由 #191 按 current Enabled Session lease 实现；#192 用最小 gate 让完整 workflow 只在可信 marker 后渐进加载。该指令级合约不物理卸载用户级 MCP 进程/工具 Schema，也不测量真实计费 token；最终 token proxy/NPM 验收仍待 #193。

### M3：Engineering Graph — 已实现

- 本机显式 Repository Catalog 是稳定 RepositoryId 的权威；SQLite Registry 只由 Catalog 同步并可删除恢复。一个 ID 支持 `0..N` checkout/worktree，绝不按 basename、remote、Git common-dir 或共同父目录推断合并/发现。
- `ResolvedRepositoryPath` 可无损转换为本次 File `ResolvedFocus`，但 Hook 当前不发起 ArtifactFocusQuery；热路径不启动 Git、不 scan/rebuild，不配置 sibling。Catalog lock/parse 故障 fail-open。
- Reference-derived `RepositoryScanPlan` 按 RepositoryId 分组、按精确 RepoRelativePath 去重；受限 tracked-source Scanner 只解析显式非空计划并生成 Module/File/Symbol/API/Schema/Test Artifact 摘要，不保存或返回完整源码，不执行全仓 `git ls-files` 枚举。
- 持久 EngineeringReference Event、ArtifactResolution、ContextArtifactAssociation 和 generation-stable historical Projection 已实现。
- `repository_scan`、`engineering_reference_record`、`association_explain`、`association_rebuild`/diagnose 已接入 MCP/CLI；服务端拥有 Reference/Event 身份与路径。
- Task Retrieval 已消费唯一 resolved Graph edge 和 Context Relation；歧义/不可用不自动选择，Graph 故障降级为 Context-only。
- Graph Builder 只固化 Reference roots 与最多两跳的 frozen ContextRelation closure；Graph item 使用 build-time immutable Revision/safety。后续 Candidate/Reference/Context/Publication append、新 Revision 或 Withdraw 不关闭旧 Graph，查询也不隐式 rebuild。
- Graph Retrieval 与当前 Intent/Context/Scope fallback 使用 revision-aware candidate key；同一 ContextId 的 frozen old Revision 与 current FTS Revision 可以同时返回，Graph path 不得重绑到当前 Revision。Graph relation traversal 只使用 frozen relation targets；当前 fallback relation 不混入 EngineeringGraph path。
- 已删除裸 Repository/File/Symbol/API/Schema/Test TaskSignal；Graph exact 只消费当前请求的 `ResolvedFocus`，并同时匹配 RepositoryId 与完整 ArtifactLocator。Intent/Context BM25 与 Scope 继续作为非 Graph fallback。
- Focus 不进入 `runtime.sqlite`、TaskSession snapshot、Signal history、Intent Revision 或 Task fingerprint。A→B 只消费 B，随后普通 `task_context` 无 Focus；重复、MCP 重启、Task 切换与 compaction 都没有 ID 或 active state 需要恢复。
- 公开 MCP/CLI `task_artifact_focus` 只接受 external Session locator、Intent Revision CAS、absolute file path、无 path 的六类 coordinates 与输出预算，所有层 `additionalProperties=false`。RepositoryId、RepoRelativePath、ArtifactKey、Generation、Workspace、Hook 和 corroboration 均由 schema/decoder 拒绝；响应只含 `resolved_focus + context`。
- Catalog declared-path resolver 允许配置 checkout 下的 missing leaf/tail，同时拒绝 dot segment、symlink traversal、现存非目录父节点和未配置前缀；不要求当前代码存在，不读取 Git、Scanner 或 AST。
- 不可达 Focus 只返回 budgeted `artifact_not_reachable_in_graph` 诊断，不声称当前代码 `missing`；只有当前 mode 实际形成 resolved safe EngineeringGraph path（Explicit 可形成 GraphDiagnostic）后才标记 reachable。历史 frozen Graph 中仍存在的精确节点继续可达。
- 固定跨 crate/E2E oracle 以手工 ID、Reference-derived path 计划、确定性 locator 和关系验证稀疏 Graph；独立 Scanner contract 用显式计划覆盖 Rust、TypeScript、JavaScript、Swift、Kotlin、JSON、OpenAPI 与 Proto。验收覆盖未引用 tracked 文件零 Artifact、path 去重、missing 无 fallback、API/Schema/Qualified Symbol/Test 精确 locator、Symbol→Requirement/Decision/Contract/Validation、File move/Symbol rename 变为 missing、FE API/Schema→跨端 Context、cycle/depth、歧义诊断、Repository unavailable、增量=scratch、投影删除重建、Generation 和 Token Budget。
- 固定 expected 位于 `tests/oracles/milestone-three-v1.json`，不得通过序列化生产结果生成或更新；只读源 Fixture 位于 `tests/fixtures/milestone-three/repository/`。
- `context_tree_oid` 仅保留 Graph build provenance；不作为启用谓词。新建或其他 untracked 文件仍不扫描，不实现 #150 范围。

#### Working Intent 与 Evidence 边界（#136）

WorkingIntentSnapshot 只保存当前非事实工作理解，不含 maturity、EvidenceSource 或 Evidence binding。Artifact/Interface Hint 只进入可解释文本匹配 `WorkingIntentHintText`，不得生成 EngineeringGraph path、事实、Candidate 或自动注入资格；`open_questions` 只记录自然形成的问题，省略是正常完整快照。TaskSignal 同样是非事实工作线索，可参与 Working Intent retrieval，但不独立支持工程断言。

Evidence 继续只约束 WorkObservation、CheckpointClaim、Candidate、ContextRevision 与 EngineeringReference 等工程断言。TaskIntentRevision 持久 authoritative text 和 canonical semantic hash；相同 continue 返回 `already_current`，真实变化创建唯一 successor，显式 new 始终创建独立 Task。M3 的 Context Evidence、EngineeringReference support/limitations、Graph provenance 与 build-time safety 服务知识断言链，不回流为 Working Intent Evidence。

### M4：Low-tax Capture — 已实现

- WorkEpisode/Capture 显式持久 API和 AgentCheckpoint MCP/CLI/Skill 已实现。PreCompact/TurnStop AutomatedEpisodeBoundary 只消费工作 Agent 已写入的 current-Intent Checkpoint，补齐 ordered refs、关闭 Episode 并调用共享 Builder；Adapter 不复制 Builder，SessionEnd 只清理 TTL。
- Candidate Builder 与最小充分 Evidence 组装已实现：closed Episode 的每个充分 Claim 形成一个无 Space Draft；Inline Validation 原样复用，normalized WorkObservation 可转换为 self-contained snapshot，Context Evidence 从一个 exact Index snapshot 复用。TaskSignal 本身仍是非事实线索；只有 Claim 显式引用的 owned Diff/TestOutcome 才由 Builder 转换为带完整解释与限制的 EvidenceSnapshot，Prompt/Workspace 线索不能成为工程 Evidence；原始 Capture 不进 Git。
- Builder 在 Git 前用 Runtime v8 固化 BuildId、Claim-scoped SubmissionId 和 content hash，#117 后回填 CandidateId/EventId；两个 crash window、语义重试和并发 close/build 均复用同一操作身份。缺 Claim、Unknown-only、Evidence 不充分为零 Git 写；kind 无 hint 固定 Discovery，topic 缺失保留 Unknown，不做关键词推断。
- Candidate relationship assessment 与 Space 推荐已实现为 Runtime derived review state：只有完整 canonical draft equality 是 exact duplicate；same statement/different Evidence 是 supports；same topic 加 explicit Context 或 exact Artifact Graph 是 revises；topic/scope 不同 statement 只是 potential contradiction；纯 FTS 是 unresolved related；无候选才 novel。所有结果固定 Context/Graph generation、typed path、confidence、RRF/top-k/token budget 与 stable target tie。
- Existing Space 推荐融合 assessment targets、source Task associations 与 Space Intent；conflicted Intent/unsafe Context 只作诊断或 Related，无安全 Primary 时生成一个完整 system-suggested Intent。分析可由 `candidate analyze` 重跑替换，不写 Git、不改变 Candidate submission/content/ID，不参与 Context Search、Hook 或自动注入。
- Runtime v9 只在 finalized Builder item 同事务初始化 Pending Candidate Review；list/get 以 ExternalSession ActiveTask 为发现边界，返回完整 draft/Evidence/provenance/analysis/Space 推荐并标记为不可信数据。手工或孤立 Git Candidate 不进入 Review，runtime 删除后也不会从 Git 复活。
- Review list 使用稳定 cursor、limit 和 whole-summary token budget；analysis pending/failed 以 typed diagnostic 可见但不 ready。discard 使用 Task/Intent/Review version CAS，同 reason timeout retry幂等，默认 list 隐藏 Discarded。
- CandidateConfirmation 与 ContextSpaceAssociation 的领域/Event/Reducer/Index 契约已实现：确认 input 只选择 existing/new Primary、Related 与 field-level edits；事实闭包引用 exact Candidate、Revision、Association、causal Publish 和 final draft hash。Association 是独立因果 DAG，不写入 ContextRelation；初始确认仍强制 result Context 的嵌套 owner 等于 Primary，未来 correction 不改变当前 Search owner。
- 一个 Candidate 的重复确认不论内容相同或不同都形成显式 conflict；Association 多 Head 同样显式 conflict。确认只验证其 causal Publication Event 为 exact Publish，后续 Withdraw/Supersede 不使历史 Confirmation 失效，当前检索继续服从现有 lifecycle。
- TTL cleanup 只删除重型 Runtime analysis并保留 terminal Expired tombstone；后续 Builder retry不得重新初始化 Pending。Runtime v10 在 Git 前保留完整 ConfirmationPlan，GitStore 将 existing/new Primary 的4/5个事实写入一个 Journal/Commit，Index v11 重建 operation/plan/batch/commit mapping，Git-before-Runtime retry可恢复 Review Confirmed audit。
- Candidate Confirm 只接受 existing Space 或 current proposed recommendation ID，不接受完整 new Intent 或生成 ID；Potential/ExactDuplicate assessment 可在明确人工调用下确认并在响应中回显 acknowledgment。PreCompact/TurnStop 已接入幂等 Episode close/Builder 触发，但绝不自动 Checkpoint、Review、Discard 或 Confirm。
- Candidate Builder 是 Candidate 创建的唯一产品入口，复用内部 #117 admission，并提供内部 CLI `candidate build-closed-episode` 恢复边界。公开 MCP/CLI 已删除手工 Candidate submission；孤立 Git Candidate 仍不进入 Review discovery。
- Mandatory Gate #117 已完成：一次创建操作携带稳定 `submission_id`，首次提交由服务端生成 `candidate_id`/`event_id`/路径并持久化 submission mapping；重试复用同一 `submission_id`。
- 相同 `submission_id` 加相同权威内容返回原 Candidate；相同 `submission_id` 加不同内容返回 `IdempotencyKeyConflict`；不同 `submission_id` 创建新的 Candidate，即使完整草稿相同。语义相近去重属于知识聚合，不由幂等键处理。
- `submission_id`、closed Episode ownership 和 writer batch annotation 进入 Git Event；SQLite 建 submission/conflict 投影并从 Git Tree 与引入 commit 重建。Candidate 主写路径使用索引 lookup，不扫描 Event 或读取 commit subject。known-v1 malformed Candidate 仅在有界解析出合法 SubmissionId 时合并到该 ID 的 conflict；unknown schema、无 hint 和其他 ID 只保留 diagnostic，不形成全局阻断。
- #156 的 WorkEpisode query 已接入 Candidate admission；不存在、Open、跨 Task 或 stale Intent 的来源在任何 Git 写入前拒绝。
- #164 固定 oracle 位于 `fixtures/m4/fixed-oracle.json`，直接驱动 WorkingIntent→Checkpoint close→Builder→Review list/get→existing/new Confirm，并与固定 Working Intent Hint、跨端 Graph、真实 Cursor/Codex Hook/Capture、submission crash/concurrency、analysis safety、pagination/budget/privacy 套件共同关闭 M4。Oracle 输入为手写 fixture，不从 production 结果生成。

## 19. 验收标准

### 19.1 Task-first Retrieval

1. 没有任何 Workspace-to-Space 配置，Task 仍可完成检索。
2. Task 请求不包含 Space ID，系统仍能返回零个、一个或多个 Space 候选。
3. 同一仓库中两个 Agent Session 同时执行不同 Task，TaskIntentRevision、Space 候选和 Candidate 互不覆盖。
4. 一个 FE Task 可以同时召回页面需求、服务端协议、兼容策略和埋点口径等多个 Space 的 Context。
5. 每个自动注入 Context 都包含至少一条可解释 Retrieval Path。
6. 关键文件、Symbol、Diff 或测试信号变化后，Context Pack 可以增量更新。
7. Candidate、Deprecated、冲突或 Evidence 不充分的 Context 不得自动注入。

### 19.2 Engineering Graph

1. 打开一个已关联 Symbol 时，可以查询其相关 Requirement Intent、Decision、Contract 和 Validation。
2. 文件移动后，原精确 Path locator 直接变为 `missing`，不自动猜测新位置。
3. Symbol 改名时，旧 qualified locator 直接变为 `missing`，不生成修复流程。
4. FE 消费 API 字段的代码可以通过 API/Schema 节点召回其他平台的 Contract 或 Validation。
5. 业务代码仓库不可访问时，Context 内容和 FTS 检索仍可使用。
6. 删除关联投影后，可以从 Git EngineeringReference 和当前代码树重建。
7. Rebuild 只读取 EngineeringReference locator 指定并去重的 path；大量未引用 tracked 文件不产生 Artifact。
8. 空计划与 missing path 不触发目录遍历或全仓 fallback；missing 保持 typed diagnostic。
9. Repository 根目录和内部子目录收敛到同一 RepositoryId；共同父目录下 sibling Repository 不被递归注册。
10. 删除 Registry SQLite 后从 Catalog 恢复相同 RepositoryId 与 locator；未配置、Workspace 外和 symlink path 均被 typed 拒绝。
11. 真实 cross 父 Workspace 中多个独立 Repo 的同名相对路径由不同 RepositoryId 隔离；Hook 新增映射 p95 <10ms、p99 <25ms，且无 Git/scan/rebuild 热路径。

### 19.3 Low-tax Capture

0. Capture/WorkEpisode 基础必须证明 typed CaptureId、Session/Task ownership、ordered Intent/Signal refs、Episode CAS、same-Capture idempotency、跨 Task 拒绝、runtime deletion isolation 与 source Episode verification；Hook 不得自动启动聚合。
1. Agent 完成一次包含代码探索、修改和测试的 Task 后，系统自动生成 ContextCandidate。
2. Candidate 自动包含 Statement、Rationale、Applicability、Evidence、RecheckWhen 和 Space 推荐。
3. 用户确认 Candidate 时不需要重新填写完整结构化内容。
4. Candidate 可以在没有确定 Space 时保存和展示。
5. Candidate Confirm 在一个 Batch 中原子生成已有/新 Space 所需事件、Context、SpaceAssociation 和 Lifecycle Events。
6. Candidate 与已有 Context 重复或矛盾时，确认前必须展示关系和证据。
7. 原始 Transcript 和 Tool Output 不进入 Git。

### 19.4 Git 与 Projection

1. 所有产品写接口只创建新事件或对象。
2. 调用者不能指定 Event 路径或覆盖已有 ID。
3. 100 个并发 Candidate Confirm 生成互不覆盖的事件文件。
4. 禁用 Git Hook 后，Writer 仍不覆盖已有受管文件。
5. Event 发现顺序不同，Reducer 输出完全一致。
6. 多个 Revision、Association 或 Lifecycle Head 必须显式投影为 Conflict。
7. 删除或损坏 `index.sqlite` 后，可以从当前 Git Tree 重建相同知识 Projection。
8. 每个 TaskContextPack 的 Context、Evidence、Conflict 和 Generation 来自同一查询 Snapshot。
9. 10 万条 Context Revision 下，纯知识 Warm Query P95 小于 100ms；包含已缓存 Engineering Graph 的 Warm Task Retrieval P95 小于 300ms。

### 19.5 Agent 与安装

1. Cursor、Codex 均完成 MCP Initialize、List Tools、TaskContext、Checkpoint、Candidate List/Confirm 和 Context Get。
2. Adapter 使用真实 Hook Payload Fixture 通过契约测试。
3. Hook 不可用时，MCP 和 CLI 仍能完成 Task Retrieval 与 Candidate 流程。
4. Setup 连续执行三次不产生重复配置。
5. 路径包含空格和中文时，Task、Artifact 扫描、Git 和 SQLite 流程正常。
6. 卸载后恢复 Agent 配置并保留 Context Git Store。

## 20. 核心不变量

1. ContextSpace 是 Requirement Intent 与 Context 组织容器，不是检索前置条件。
2. Workspace 只提供非定位 TaskSignal/Breadcrumb，不能决定 Artifact 或 Space。
3. Task 与 Space 是动态多对多关联，不存在全局 Active Space。
4. ContextCandidate 可以没有 Space，确认后再形成显式 SpaceAssociation。
5. ContextItem 的身份独立于 Space；归属修正不改变 Context ID。
6. Git Event 和 Evidence Object 是稳定 Context 事实源。
7. WorkingIntentSnapshot、TaskIntentRevision、TaskSignal、WorkEpisode、Candidate、置信度和当前代码解析结果不是知识事实。
8. Revision 保存完整快照，不保存文本 Patch。
9. Evidence 必须自包含；repo-relative Path 与 qualified Symbol/API/Schema/Test 坐标只能作为 EngineeringReference。
10. EngineeringReference 可以失效，ContextArtifactAssociation 必须可重建。
11. 文件路径、Git Commit、时间和 Agent Session 不参与领域身份或生命周期归约。
12. Revision、SpaceAssociation、Lifecycle 和 Conflict 只按显式因果关系归约。
13. 多个 Head 必须暴露为冲突，禁止 Last-Write-Wins。
14. 关联置信度只能影响检索，不能提升 Context 生命周期或自动注入资格。
15. Candidate、Annotation 和 Capture 内容不得作为高权限指令注入。
16. 产品写入路径只创建新文件，自动提交不得吸收或覆盖其他变化。
17. 相同 Context Git Tree 和实现版本必须得到相同知识 Projection 与稳定查询顺序。

## 21. Agent 版本策略

Cursor、Codex 等厂商 Hook 和 MCP Payload 属于易变适配能力，不属于领域不变量。

每个 Adapter 必须：

- 声明已验证的 Agent 版本范围。
- 使用真实 Payload Fixture 做契约测试。
- 对未知字段严格区分可忽略扩展与协议破坏。
- 在能力不可用或 Trust 未确认时降级为 MCP + CLI。
- 不因厂商 Hook 缺失而改变 Task、Context、Space 或 Candidate 领域模型。
