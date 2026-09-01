# 02 · 代码实现 → 功能 → 目标 映射

> 状态：进行中。目标编号 G1–G5 见文末附录；关系分级：**核心**（直接决定目标成败）/ **保障**（坏了目标静默劣化）/ **支撑**（工程基础设施）。
> 更新日志见文末。

## 测试与验收基础设施（来源：领域 F，已 review）

| 模块 | 功能 | 目标 | 分级 | 关键引用 |
|---|---|---|---|---|
| scenario-contract（场景 DSL + 校验） | 只读黑盒场景契约：`ScenarioAction`/`FaultPlan`/`InvariantKind` 闭集、依赖图校验、禁止伪造 `expected_*`、禁止硬编码真实域 ID | G1 | **保障** | `crates/scenario-contract/src/validation.rs:71-98` |
| scenario-runner（黑盒执行器） | 隔离 sandbox 启动 `sctx`、只读观察 SQLite/Git、确定性故障调度；`RawContentIsAbsent` + PrivacyCanary 字节匹配是不动原则②（Prompt/Hook 不产事实）**唯一**的黑盒自动化回归点；`ConfirmationIsAtomic` 等对应原则③ | G1 | **保障** | `crates/scenario-runner/src/lib.rs:1000-1088` |
| Dynamic Replay 六场景 | 崩溃恢复/TurnStop 幂等/PreCompact 续接/双 Session 隔离；只证协议状态机在故障下不腐化，**不测模型行为**（显式 deferred） | G1 | **保障** | `docs/dynamic-replay-phase-one.md:98-107` |
| 联想探针集 en 22 + zh 24 条及 harness | 用零/低 token 重叠的改写查询反查已确认 Context，统计 `search`/`task_intent_update` top-1 命中 | **G4** | **保障**（G4 唯一可量化标尺；排序改动的唯一验收依据） | `crates/cli/tests/association_probe_workflow.rs:51-55` |
| fixtures/agents（Cursor 3.13 CLI / 3.17 桌面 / Codex 0.147 payload） | 各宿主 Hook payload 真实形状快照，防解码器 drift 静默 fail-open | G3 | **保障** | `fixtures/agents/cursor-3.17-desktop.json` |
| schemas/event-v1.schema.json | 版本化事件契约，发布后只读；未知 schema 原样隔离保前向兼容 | G1 | **保障** | `schemas/README.md` |
| milestone-three oracle + 跨端 fixture 仓库 | 一个 requirement Space 关联 server/ios/android 三个实现 Space，Engineering Reference 覆盖 Symbol/API/Schema/File，跨 Kotlin/Swift/JS/Proto/OpenAPI | **G5** | **保障**（G5 唯一跨端固定验收样本，单一 oracle 非覆盖式） | `tests/oracles/milestone-three-v1.json` |
| demo 验收（`demo_acceptance.py`） | 独立 Python 黑盒 oracle 验证 `setup --demo` 幂等、事件精确、CLI/MCP 检索一致 | G2 | **保障**（新用户第一入口的承诺守门） | `tests/scripts/demo_acceptance.py:108-193` |
| `run-acceptance.sh` | 顺序跑旧阶段验收目标 | — | **支撑**（已过期，见 04 文档 D4-4） | `tests/scripts/run-acceptance.sh` |
| 真模型探针（`codex_checkpoint_model_probe.py`） | 真实 Codex+模型 100 会话统计 checkpoint 一次性合法提交率 | G3/G4 | **保障**（"模型真的会用"唯一证据源，不在 CI） | `docs/acceptance-report.md:112-128` |

边界记录：p95 性能断言（ACK、artifact_focus）全部落在 mcp/search/local-state 的**白盒** crate 测试，黑盒验收路径不含任何延迟断言（详见 04 文档 D4-5）。

## CLI 与安装/发布链路（来源：领域 E，已 review）

命令面有两条后端路径：Agent 侧走 `sctx_mcp::*_at_root`（与 MCP Server 共享入口）；**人工治理路径**（`space create`、`context publish/withdraw`、`semantic conflict resolve` 等，main.rs:1687-2510）直接构造 domain Event 写 GitStore，**不暴露给 MCP**——这是不动原则③在工具面的落地：Agent 只有 candidate 流程一条产出 accepted 的路。

| 模块 | 功能 | 目标 | 分级 | 关键引用 |
|---|---|---|---|---|
| Hook 写入 + `sctx hook` 运行时 | installer 把 hook 命令写进宿主配置（6 类事件）；运行时解码 payload、解析 Session 授权、PostToolUse 归因到 Repository、注入 additional_context | G2/G4 | **核心**（pull 式注入唯一挂载点，此步失败整条链路不触发、无降级） | `installer/src/lib.rs:64-79,2714-2816`、`cli/src/main.rs:780-1013` |
| SKILL.md 门禁 + workflow.md 引导 | 只信任精确 marker 激活；引导"建 Task→检索→Checkpoint→审阅→关联"顺序；强调 Untrusted 边界 | G2/G3 | **核心**（原则②③从代码约束落到 Agent 行为约束的唯一渠道） | `skills/shared-context/SKILL.md`、`references/workflow.md` |
| CLI 治理命令（space/context/semantic） | 人工操作员直接写 Event，不走 candidate 流程 | G1/G3 | **核心**（对 G1：accepted 事实的两条写入通道之一，仅限人工） | `cli/src/main.rs:1687-2510` |
| installer 事务/回滚（setup/upgrade/uninstall/reset） | 幂等安装、`bin/current` 原子切换、journal+崩溃回滚；installer_matrix 约 47 个测试覆盖各崩溃点 | G2 | **保障** | `installer/src/lib.rs:590-799,1440-1575` |
| doctor / data reset | 安装健康诊断、结构化 recheck、可恢复的销毁式重置 | G2 | **保障** | `installer/src/lib.rs:803-899` |
| npm launcher SHA-256/codesign 校验 | 分发端防篡改，与 installer 侧 `verify_signature` 互为镜像 | G2 | **保障**（信任链，非纯支撑） | `npm/packages/shared-context/lib/launcher.js` |
| publish-npm.yml 发布流水线 | tag 触发、tag↔版本一致性校验、平台包构建 | G2/G3 前提 | **支撑**（dist-tag 单调性缺口见 04 文档 E-2） | `npm/scripts/publish-release.js:69-107` |
| Skill 资产 `include_bytes!` 内嵌二进制 | 保证 Skill 与 CLI/MCP 工具面版本严格一致，代价是无法独立热更新 | G2/G3 | **支撑** | `installer/src/lib.rs:49-63` |
| workspace_contract 架构守护测试 | 断言依赖只指向更低分层（硬编码分层图） | — | **支撑**（保证本 review 依赖图长期成立） | `cli/tests/workspace_contract.rs:34-71` |

## 存储与事件层（来源：领域 A，已 review）

Runtime 态与 Git 态的边界（结构性强制，做得扎实）：**进 Git 的只有 11 类不可变事件 + content-addressed evidence + schema 文件**；Task/Intent/Signal/WorkEpisode/Checkpoint 收据/Outbox/Review 等全部只落 `runtime.sqlite`；`stale_reason` 等派生态只落 `index.sqlite`（重建即清空）；`append_event` 直接拒绝 Candidate/Confirmation 事件绕过服务（`store.rs:812-824`）。

| 模块 | 功能 | 目标 | 分级 | 关键引用 |
|---|---|---|---|---|
| Git append-only Writer | 唯一知识写入口；九步追加协议（pending+fsync→隐私扫描→journal→锁→create_new→只 stage 显式路径→复核 staged 全为 A→`commit --only`） | G1/G3 | **核心** | `git-store/src/store.rs:1795-1946` |
| Batch Journal + 崩溃恢复 | phase 标记、崩溃后按 path/hash 恢复同一批；13 个 CrashSeam 全覆盖测试 | G1/G3 | **保障** | `store.rs:1622-1792` |
| Candidate Confirm 原子事实闭包 | 一次确认的全部事件同一 commit——**唯一产生 accepted 的路径**（原则③的实现体） | G1/G3 | **核心** | `store.rs:1093-1347` |
| Candidate 提交幂等（SubmissionId） | 同键重放不产新 Candidate；同键不同内容→显式冲突 | G1 | **保障** | `store.rs:889-1091` |
| 隐私门禁（写 Git 前硬拒） | Secret/PII 扫描命中即拒整个事件/确认批 | G3 | **保障**（误报问题见 04 文档 A-2） | `store.rs:2378-2401`、`local-state/src/privacy.rs` |
| 事件类型与 payload 契约 | 11 个封闭 EventType、`deny_unknown_fields`、手写 validate | G1 | **核心**（前向兼容缺口见 04 文档 A-3） | `event-schema/src/lib.rs:105-286` |
| 未知 schema 隔离 | 仅覆盖 `schema_version != "1"`；原文保留+诊断 | G3/G5 | **保障** | `event-schema/src/lib.rs:1195-1230` |
| Schema 捆绑与 legacy 自修复 | 老仓库补 schema commit；字节冲突 fail closed | G3/G5 | 保障 | `store.rs:749-804` |
| 远端 Knowledge Store bootstrap | 克隆团队远端、拒内嵌凭据 URL、installation work branch（ADR-0002） | G3/G5 | **核心** | `store.rs:585-654` |
| SQLite 投影全量重建 | HEAD tree 确定性重建 29 表+2 FTS；影子表+单事务切换 | G2/G4 | **核心** | `index/src/lib.rs:433-575` |
| 投影 generation + 实现版本 | 6 个实现版本任一变更即全量重建；查询固定在单 generation 只读事务；dev/inode 检测重开 | G2/G4 | **保障** | `index/src/lib.rs:36-55,875-935` |
| 增量投影 | tree diff 全为 A 时按受影响 Space 选择性替换 | G2 | 支撑（**实际净负收益**，见 04 文档 A-1） | `schema.rs:1339-1479` |
| 损坏隔离与自愈 | quick_check 失败→隔离改名→重建 | G2 | **保障** | `index/src/lib.rs:804-824` |
| FTS5 + 自写 tokenizer | NFKC/case-fold/标识符切分/Han 二元组 | G4 | **核心**（当前唯一召回通道，无向量结构） | `tokenizer.rs:9-129`、`schema.rs:466-493` |
| Repository Catalog（config.toml） | 团队约定 exact-case RepositoryId ↔ 本机 checkout（ADR-0001） | **G5** | **核心**（跨仓身份稳定性的承载） | `local-state/src/config.rs:187-232` |
| ActivationScope 派生 + 租约 | 启动目录派生 Enabled/Disabled、home/根守卫；永不过期租约+30 天孤儿回收 | G2 | 保障 | `config.rs:239-274`、`session_scope.rs` |
| MaintenanceLock | 安装级共享/排他门禁 | G1/G2 | **保障** | `maintenance.rs:22-129` |

**目标覆盖小结（A 层）**：G1 最扎实（原子确认闭包、Evidence 自包含均落地）；G4 只有 BM25+规则、无向量承载结构（已确认缺口）；G5 本层给了身份稳定性（RepositoryId），缺语义桥接（属检索域）。

## 协议与宿主接入层（来源：领域 D，已 review）

| 模块 | 功能 | 目标 | 分级 | 关键引用 |
|---|---|---|---|---|
| MCP 工具面（**17 个**） | Agent 与知识库唯一读写接口。`task_checkpoint`→G1 核心（决策入库唯一入口，ACK 路径不开 Git/index）；`task_intent_update`/`task_context`→G2 核心；`task_artifact_focus`→G4 核心（按六种坐标做代码召回）；`candidate_confirm`→G3 核心（原则③协议层落点）；`space_create`→G3 保障（真实会话被拒两次后补的治理闭环） | G1–G5 | **核心** | `mcp/src/lib.rs:6406-6427,6789-6925` |
| ToolInputContract（去 oneOf + 服务端组合校验） | 对外保持扁平 object，排他约束下沉为具名 `invalid_input`；`detail_level` 在严格解码前剥离（token 预算设计，默认 compact） | G1/G3 | **保障**（坏了不报错，模型"读不懂参数→放弃调用"，真实故障 01a05278 验证过因果链） | `mcp/src/lib.rs:6466-6571` |
| 授权与 lease | 每次 tools/call 命中 Enabled lease 才放行；agent_kind 与 `--client` 一致性校验防跨宿主冒用；所有失败折叠为同一 `authorization_failed`（防探测，有测试断言） | G1/G3 | **保障** | `mcp/src/lib.rs:6591-6632` |
| Hook 生命周期 + 三层 fail-open | 六事件→纯策略函数（无 I/O，无法访问 Prompt/Context——原则②的机械化落点）→类型化操作计划；payload 解不开/锁拿不到/授权失败均安静降级 | G1/G2 | **保障**（fail-open 经真实故障打磨，不建议动；但见 04 文档 D-1 的静默性代价） | `agent-adapter/src/lib.rs:557-659`、`cli/src/main.rs:832-947` |
| activation marker + external_session_id 贯通 | ≤512B、只带宿主会话 id + 逐字抄写指令（防模型臆造 id 的真实教训）；SKILL.md 用同一 marker 形状做激活门防注入；IntentBootstrapReminder 一次性补位 | G2 | **核心**（pull 式注入的全部起点） | `agent-adapter/src/lib.rs:191-236` |
| 宿主 payload 适配器（Cursor/Codex） | 厂商线格式↔canonical 事件严格双向翻译；刻意不过缝：transcript 路径/用户身份/时间戳/原始工具输出；版本从不 gating | G3/G5 | **支撑**（粘合层）+ G1 保障（严格解码防脏 payload 变事实）；能力不对称见 04 文档 D-4 | `adapter-cursor`、`adapter-codex` |
| render_untrusted_task_context_pack | 五道不变式的不可信 Context Pack 渲染器 | — | **无生产调用者**（见 03 文档 D 领域第 1 条） | `agent-adapter/src/lib.rs:675-754` |

## 领域模型与生命周期（来源：领域 C，已 review）

三条不动原则在本层的落地位置均已核实遵守：原则①=`TaskCheckpointInput` 四顶层参数；原则②=`TaskSignal` 仅 4 kind 且 `CheckpointEvidenceRef::TaskSignal` 生产路径从不构造；原则③=`Accepted` 只能由 `PublicationAction::Publish` 产生，唯一构造点是确认计划与 CLI 手工 publish。

| 子系统 | 功能 | 目标 | 分级 | 关键引用 |
|---|---|---|---|---|
| `ContextRevision`/`EvidenceSnapshot`/`Applicability` | 完整快照式知识条目：statement/rationale/assumptions/recheck_when/relations/evidence（至少一条） | **G1** | **核心**（"为什么"能否沉淀取决于这些字段是否被填满——四字段路径只填得满 2/4，见 04 文档 C-1） | `domain/src/model.rs:238-295` |
| `AgentCheckpoint`/`CheckpointClaim` | Agent 结论载体；Claim 强制 evidence 非空、Checkpoint 强制归属确切 Episode 与 Intent revision | G1/G3 | **核心** | `episode.rs:563-714` |
| `CandidateAnalysis`（typed assessment path） | "新结论 vs 已有 Context"关系的类型化（ExactDuplicate/Novel 等各有强制路径证明） | **G4** | **核心**（embedding 缺口下语义关联的唯一落地形态） | `episode.rs:886-1057` |
| `CandidateReviewView::validate` | 评审状态机自洽性 + `untrusted_data` 恒为 true | G1/G3 | **保障** | `episode.rs:1514-1583` |
| `CandidateConfirmationPlan`/`CausalRefs` | 一次人工确认的完整事实闭包与稳定 ID 预留（原则③执行点） | G1/G3 | **保障** | `confirmation.rs:407-663` |
| `reducer::reduce` | 事件多重集→确定性投影：5 个 DAG 的 head/quarantine/governance/superseded/auto-injection blocker | G1/G4 | **保障**（"禁止 LWW"的唯一防线） | `reducer.rs:1584-1700` |
| `ArtifactLocator`/`EngineeringReference` | kind-specific 仓库坐标；Reference 是非权威观察 | **G4/G5** | **核心**（跨端召回坐标基础） | `engineering.rs` |
| `ExternalSessionSnapshot`/`TaskSessionSnapshot` | 会话↔ActiveTask 恰好一个的不变量；Task Intent 线性单父链；序列化不含 space/workspace 字段（测试强制） | **G2** | **核心** | `task.rs:165-366` |
| `WorkingIntentSnapshot` | goal 必填的轻量意图快照，canonical 归一后哈希 | G2/G4 | **核心**（召回查询侧输入） | `working_intent.rs:27-127` |
| `hints.rs` | 散文→路径/标识符抽取，Reference 派生与 FTS 共用同一份读数 | **G4/G5** | **核心**（embedding 缺口下唯一"代码↔知识"文本桥；能力上限见 01 文档条目 11） | `hints.rs:91-112` |
| `open_or_create`/`start_new_task`/`continue_working_intent` | 会话原子创建、显式任务边界、意图续写；20 并发同内容收敛、不同 goal 分叉（隐式边界问题见 04 文档 C-10） | **G2** | **核心** | `task-runtime lib.rs:706-1017` |
| `submit_agent_checkpoint` + 内容寻址幂等 | ADR-0003 落地：解析→Episode→物化 Claim→关闭→outbox 预留→receipt 全一个事务；重放返回同一 operation | **G1/G3** | **核心** | `lib.rs:1471-1707,4188` |
| `reference_derivation` | Build 期一次性把 Claim 散文解析成 Reference/topic hint，首次结果永久固化 | **G4** | **核心** | `reference_derivation.rs:190-274` |
| 确认两阶段（reserve/finalize） | runtime 预留→Git 提交→finalize；operation_hash 不同即冲突 | G1/G3 | **保障** | `lib.rs:2881-3083` |
| `record_context_usage`（reused/ignored/refuted） | 注入后反馈信号，Refuted 最高优先不可降级 | **G4** | **保障**（检索排序先验） | `lib.rs:3233-3400` |
| 显式 Episode API（9 个方法 + 7 个观察变体） | — | — | **无生产调用者，约 1500 行**（见 03 文档 C 领域第 4 条） | `lib.rs:821-2085` |

**目标覆盖小结（C 层）**：G1 结构完备但四字段路径只填得满一半；G2 任务边界扎实（并发收敛/CAS/线性链均有测试）；G3 管道类型完备、瓶颈在确认入口可达性与归属可修正性（见 04 文档）；G5 本层只有 `RepositoryId` 与自由文本 `platforms` 两个抓手，**是对 G5 支撑最薄弱的一层**。

## 检索与联想层（来源：领域 B，已 review）

### search crate（Task-first 多路召回）

| 子系统 | 功能 | 目标 | 分级 | 关键引用 |
|---|---|---|---|---|
| 多路召回编排 + RRF 融合 | 5 类候选源 8 通道按 `1/(60+rank)` 加权：Graph 13 > Relation 10 > FocusFallback 6 > Hint 3 > 文本 1 | **G4/G2** | **核心** | `search/src/lib.rs:2410-2514,3641-3796` |
| AutomaticTextEligibility 三层闸 | 五选一准入（phrase/coverage≥60%/双通道/text+ExactScope/exact Graph）防弱文本混入自动注入 | G4/G1 | **保障**（中文误伤见 04 文档 B-4） | `lib.rs:4174-4209,5484-5569` |
| 标识符加权覆盖 + token_alias | 命中 ≥2 个同组 token 时标识符算 3 次——**中英跨语言召回的唯一机制**；别名只能"补全说对了一半的标识符" | **G5/G4** | **核心** | `lib.rs:4059-4093,7147-7330` |
| problem_view 检索字段 | "这条知识回答什么问题"作为一等检索面，BM25 权重 8.0 与 statement 齐平 | **G1/G2** | **核心**（质量上限问题见 04 文档 B-8） | `schema.rs:473`、`lib.rs:6304` |
| Scope 结构化匹配 | domain（含别名）/platform 精确匹配——platform 是跨端唯一结构化维度 | **G5** | 核心 | `lib.rs:2152-2226` |
| ContextRelation 两跳扩展 | depth=2、去环、自动模式 seed 须先过文本闸（弱文本不得让同 Space 无关 Context 继承资格） | G4/G1 | 核心 | `lib.rs:3081-3262` |
| TaskRetrievalPath 类型化路径 | 9 种路径变体，每条结果必带可解释路径 | G1/G4 | **核心** | `lib.rs:717-753` |
| Context Pack 预算打包 | Full 逐级退让；Compact 事实优先（70% 先给 items、保底 3 条挤压 Evidence） | **G2** | **核心**（决定 Agent 实际拿到几条历史） | `lib.rs:5830-6104` |
| 排序修正器组 | 覆盖率乘子（floor 50%/饱和 30%）、hop-only 降级、usage prior（复用+15%/忽略≥3 次-10%）、冲突 ×0.7 陈旧 ×0.6、TTL historical | G4/G2/G1 | **保障** | `lib.rs:4232-4332,5682-5743` |
| candidate.rs 候选分析 | 5 通道 RRF 判定 duplicate/support/revise/contradiction/novel + Space 推荐 | **G3/G4** | **核心**（个人知识→团队资产的把关点；embedding 缺口下语义关联的实际落地） | `candidate.rs:95-248,842-1091` |
| 显式 context_search | BM25×覆盖率、answerable 分母修正、稳定 cursor 分页 | G4/G3 | 支撑 | `lib.rs:6433-6762` |

### engineering-graph crate（代码侧确定性锚点）

| 子系统 | 功能 | 目标 | 分级 | 关键引用 |
|---|---|---|---|---|
| RepositoryRegistry | RepositoryId 只来自显式 Catalog，路径/basename/remote 绝不推断身份；fingerprint 短路避免每请求起 git 进程 | **G5** | **核心** | `engineering-graph/src/lib.rs:115-346` |
| RepositoryScanner | 有界只读扫描（≤10000 路径、tracked-only、symlink/敏感文件/预算检查），产六类 Artifact | G4 | 核心 | `scanner.rs:140-142` |
| EngineeringReferenceResolver | 四态确定性解析，**只有唯一精确匹配才建立关联**，move/rename 后保持 missing 不猜 | **G4** | **核心** + G1 保障（不猜=不产假事实；衰减问题见 04 文档 B-3） | `resolver.rs:650-730` |
| Graph 快照构建 + Safety | 从 Reference 出发 BFS 两跳，冻结 build-time revision/lifecycle/typed blockers；不合格必须有 blocker | G4/G1 | **保障** | `resolver.rs:110-288` |
| projection_generation | policy+全部输入的 SHA256，任何变化换代 | G4 | **保障** | `resolver.rs:896-937` |
| ArtifactFocusReader | Hook 热路径最窄读者：只读、busy_timeout=0、150ms 预算、超时/缺文件返回空 | **G2** | **核心**（"打开文件即被告知有历史决策"是 G2 最直接形态；性能与静默问题见 04 文档 B-7） | `artifact_focus.rs:25-122` |

**目标覆盖小结（B 层）**：G4 通道齐全但全部 token 级精确匹配（词面不同即断连）；G5 的机制上限是"如果有人已把两端连起来则可召回"——连接本身不会自动产生；G2 的最直接形态（artifact focus）有规模化静默失效风险。

---

## 附录：目标编号

- G1 沉淀代码之外的关键决策（业务约束、取舍、历史决策及原因）
- G2 降低后续任务启动成本（自动获取相关历史上下文）
- G3 跨开发者、跨 Agent 的知识传递（个人知识 → 团队资产）
- G4 基于代码与语义的历史上下文召回
- G5 跨仓库、跨端的上下文打通

## 更新日志

- 2026-09-01 建立骨架，分析任务派发中。
- 2026-09-01 录入领域 F（测试与验收基础设施）10 项模块映射。
- 2026-09-01 录入 E、A、D、C、B 五领域映射；六领域齐，覆盖全部 16 个 crate。各层均附目标覆盖小结。
