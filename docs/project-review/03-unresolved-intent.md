# 03 · 无法推测实现意图的记录

> 状态：进行中。仅记录在查阅代码注释、technical-design、ADR、git log 之后仍无法推断意图的点；每条含位置、观察到的行为、推不出的原因、（可选）猜测与置信度。
> 更新日志见文末。

## 测试与验收基础设施（来源：领域 F，已 review）

**结论：0 条。** 该领域注释/文档密度极高（不变量与常量的"为什么"基本都写在 `DEVELOPMENT.md`、模块注释或断言消息里），核对 technical-design、ADR 0001–0003、git log 后未发现推不出意图的点。

两条"曾怀疑、后自证"的记录（供交叉核对方法参考）：
- `scenario-contract` 的 `DOMAIN_ID_PREFIXES` 保留已删除的 `rpo_`/`rpg_` 前缀 —— 测试注释明确"retained for historical redaction"，为拦截旧 fixture 残留，非死代码。
- `RawContentIsAbsent` 中 `Transcript` 对任意 product action 均 `compatible = true`（而 Prompt/ToolOutput 各自限定事件）—— 由 `fixtures/agents/*.json` 可反推：`transcript_path` 几乎出现在所有 Hook 事件里，是数据形状决定，非遗漏。

## CLI 与安装/发布链路（来源：领域 E，已 review；6 条）

1. **CLI 治理命令为何绕开 `sctx_mcp` 直接操作 Event/GitStore**（main.rs:1687-2510）：technical-design/ADR 均未写明分层理由。中置信度推测：这些是"人工专用、Agent 不可触达"的操作，故不放进 Agent 工具面 crate——与不动原则③一致，但代码无注释点明，属反推。
2. **Agent 版本探测在 installer 与 cli 各维护一份近似实现**（installer lib.rs:221-232 vs cli 的 `detect_agent_version`，同样的 2s 超时与 cursor-agent 优先注释）：git log 显示分别累积添加，更像历史重复而非有意设计（E 自评：代码质量问题，非意图不可推测）。
3. **`skills/shared-context/agents/openai.yaml` 的消费方不明**：仓库内无任何代码解析其字段，仅作为静态资产安装。低置信度猜测：满足 Codex 宿主侧 Agent 描述协议的固定 manifest。
4. **schema 丢弃白名单为何是错误消息字符串精确匹配的硬编码枚举**（installer lib.rs:3782-3789，仅 11/12）：ADR-0003 解释了"丢弃重建"的取舍，但未解释为何不用 `< CURRENT_SCHEMA_VERSION` 通用规则。中置信度猜测：刻意让"discard 是否安全"经显式 review 而非自动放行。
5. **`KNOWLEDGE_SYNC_PUSH_ATTEMPTS = 3`**（installer lib.rs:47）：固定 3 次、无退避，无解释。低置信度：经验值。
6. **publish-npm.yml 的 `CI_NAME: allow_same_version_${{ github.run_id }}`**（已核实存在于 :21）：非标准变量名，引入提交（75f700d）无说明。低置信度猜测：字节内部 registry（bnpm）识别的自定义变量，用于 CI 幂等重跑同版本发布；公开代码无法验证，**需向内部 registry 文档或作者确认**。

## 存储与事件层（来源：领域 A，已 review；6 条）

1. **`MAX_INTERNAL_BATCH_EVENTS = 512` 的来源**（`store.rs:33-37`）：同一常量复用为 Candidate 提交批上限与 Confirmation 事件批上限，但两者量级完全不同（一次确认产 5~20 个事件，实际只能装约 25~100 个候选）。TD/ADR 均未提。低置信度猜测：直觉安全上限，复用是省事。
2. **崩溃恢复的 `journals.swap(0, position)` 启发式**（`store.rs:1769-1792`）：把"files 覆盖全部 staged 路径"的第一个 journal 换到队首，无注释；多个候选时按目录名排序取第一个，而 BatchId 是随机 UUID，选择实际不确定。中置信度（55%）：意图是让上次已 stage 的批优先提交、避免后续批撞 foreign staged 全挂——若属实意图正确但表述脆弱。
3. **`MAX_REMEMBERED_HEAD_TREES = 8` 且满了整表 `clear()`**（`index/src/lib.rs:225-226,591-596`）：注释说"只会问当前 commit"，那容量 1 即可；容量 8 + 非 LRU 清空，既不是缓存也不是常规防泄漏写法。
4. **TTL 用 Git commit time 与「禁止按 commit 顺序做 LWW」（TD §6.4）的关系**：TTL 是由 commit time 驱动、会改变 Context 是否参与自动注入的决策；二者是否被认为不冲突（"TTL 是本地派生可见性策略，非生命周期归约"）无文档说明；`Annotations.created_at` 为何不用作时间源也无说明。中置信度（60%）推断不冲突，**建议补文档定论**。
5. **`bat_` 批次 ID 被写进事件 annotations 并成为反序列化硬校验**（`event-schema/src/lib.rs:323-331,1260-1274`）：批次是 Writer 运维概念，却提升成了事实契约；与 CONTEXT.md"annotations 是非权威边界"有张力。中高置信度（70%）：为让 index 仅凭事件字节把一次 Confirmation 的多个事件归组（`project.rs:111-116` 确实这么用）。
6. **文档与代码多处漂移，无法判断哪边是意图**（建议作为文档修订项，需人工确认方向）：
   - TD §6.1 事件类型集合与代码不符（TD 列的 `context.created`/`context.lifecycle_changed` 不存在；代码的 5 个类型 TD 未列）；§6.2 示例 JSON 用现 parser 解析直接失败；§6.4 的 `activate/deprecate` 实为 `Publish/Withdraw`。
   - TD §10.1 表清单过时（`space_intent_fts` 实为 `space_fts`；engineering 三表实际在**第三个数据库** `state/engineering.sqlite`，而 §9.1 目录布局完全没提这个库）。
   - TD §9.1 说激活租约"TTL-bounded"，代码与 CONTEXT.md 是"永不过期+30 天孤儿回收"（e3a4809 之后的方向）。

## 协议与宿主接入层（来源：领域 D，已 review；5 条，另 1 条确认为缺陷移入 04 文档）

1. **`render_untrusted_task_context_pack` 无任何生产调用者**（已核实：仅测试引用）：79 行 + 5 道不变式的注入渲染器，TD 明确 SessionStart 不注入知识项，MCP 走 structuredContent——它服务于哪条路径？中置信度猜测：push 式注入改 pull 前留下的已废弃安全门，因不变式检查有价值而未删。**建议确认删除或标注保留原因。**
2. **PostToolUse 的仓库归属计算结果被整体丢弃**（已核实：`normalized_tool_signals` 四参数全 `_` 前缀弃用，只产 test runner 成败字面量）：数百行 path-hint 提取+Catalog 归属机器的唯一残余效果是"归属出错时抑制信号"。git log 显示 8ea7136（移除机械证据管线）之前参数就已未使用。中高置信度：归属链作为"事件不逃出已注册仓库"的安全边界被保留，信息产出随管线死掉。**需确认是有意保留的安全 gate 还是待清理死代码**（改造方向见 04 文档 D-2）。
3. **PromptSubmit 钩子被注册、解码完整 prompt 明文过适配器缝，但策略恒 neutral、无人读取**：每条用户 prompt 触发一次进程启动+配置解析，产出 `{}`。适配器类型文档声明排除 transcript/身份/时间戳——按同一标准 prompt 明文更该排除。中置信度：为 prompt-aware 注入占位（`prompt_aware_injection` 能力位存在但同样无消费者）。**建议确认：无近期计划则摘掉注册并删字段，是成本与隐私面的净收益。**
4. **Cursor 的 `ToolOutcome` 恒为 `Succeeded`**（已核实 adapter-cursor 硬编码）：`tool_output` 字段实际持有 `{"exitCode":0,...}`。无注释说明是"Cursor 不提供成败"还是"刻意不解析以免原始输出过缝"——若是后者，exitCode 属结构化元数据本可安全提取。后果见 04 文档 D-4。
5. **`initialize` 原样回显客户端 protocolVersion**：客户端声明未来版本时服务端会宣称支持但不实现其语义。中高置信度：最大化宿主兼容的刻意 permissive 选择，但把不兼容从握手期推迟到调用期。**建议确认是否有意。**

## 领域模型与生命周期（来源：领域 C，已 review；9 条 + 文档漂移清单）

1. **`submit_agent_checkpoint` 里恒真的版本 CAS**（lib.rs:1543-1545）：`require_open_episode_version(actual, status, actual)` 两参相同，第二个检查恒真。内容寻址提交本就无调用方 CAS（ADR-0003），但那应写成只查 status 的断言而非伪装成 CAS。中置信度：从真 CAS 的 `write_agent_checkpoint` 复制的残留。行为无害但误导读者。
2. **`checkpoint_operation_identity` 返回两个逐字相同的字符串**（lib.rs:4205-4207）：operation_key 与 operation_id 相同却写两列、读回互相校验。中低置信度：为未来 `ckop_` 前缀可读 id 预留。
3. **`IntentRevisionRange` 文档说"一个 Episode 观察到的区间"，实现是"Task 至今全部 revision"**（episode.rs:68-71 vs lib.rs:5151-5192，SQL 无下界）：Episode 当前 1:1 对应 Checkpoint 所以无可观察后果，**一旦恢复多 Episode 语义就是正确性问题**。
4. **整套显式 Episode API 约 1500 行无生产调用者**（已核实 `switch_active_task` 等 9 个方法 + `NormalizedWorkObservation` 8 变体中 7 个生产不可达）：8ea7136 移除机械管道时只断开了产生方、保留全部消费端与类型，commit message 未说明是"暂留待恢复"还是"漏删"。中置信度：ADR-0003"产品未发布可直接替换 runtime"下的低风险选择。
5. **`AutomaticContextCandidate`（9 字段富视图）与 `ContextCandidate`（4 字段）的职责切分**：注释只说"M4 contract 之外"；若前者是本地 runtime 富视图、后者是进 Git 的最小载体，命名方向反了（`Automatic` 反而更像最终产物）。
6. **`ContextRelationHop` 的 depth 上限为 2**（episode.rs:886-891）：TD §8 讲的是 Reference 的 bounded closure 不是关系跳数；为什么是 2 无数据支撑。低置信度：token 预算经验值。
7. **`CheckpointUnknown.recheck_when` 在四字段路径恒空却仍是必需字段**：唯一填充点是服务端自动生成的两条 unknown。较高置信度：服务端专用槽位——但类型不区分"Agent 写的"与"服务端写的"，读者无法判断可信度。
8. **ProposedSpaceGroup 已改绑 task_id（74fb278，理由清楚），但 CONTEXT.md、TD、对外 JSON tag `proposed_from_task_intent_revision`、数据库列 `intent_revision_id` 四处仍留旧语义**：推不出是刻意保持契约兼容还是遗漏。
9. **三套彼此独立的"替代/失效"语义**：revision 级 `Superseded`（Publication 推导）、Context 级 `Supersedes` 关系边（只在 index 层推导 superseded_by，reducer 不感知）、治理级 `Deprecated`（Withdraw 产生）；三者作用域/推导位置/对自动注入的影响各不相同，仅第三种进 AutoInjectionBlocker。TD 未提关系边这条，无法判断它是补充还是替代方案。

**C 层文档漂移清单**（需人工定夺哪边是意图）：ProposedSpaceGroup 绑定对象（见第 8 条）；CONTEXT.md 说 ExternalSession"不推断任务边界"/ActiveTask 变更是"显式决策"vs 实现里 goal-fork 自动切换（04 文档 C-10）；TD §6.3"Association 多 head 阻止自动注入"未实现（04 文档 C-7）；"Primary Space 可修正"无入口（04 文档 C-6）；TD 生命周期动作 activate/deprecate 实为 Publish/Withdraw；TD"禁止内容 Hash 作领域 ID"vs `ProposedSpaceGroupKey` 正是内容哈希派生（本地对象或有意豁免，文档未写例外）。

## 检索与联想层（来源：领域 B，已 review；6 条）

1. **`FUSION_CHANNEL_WEIGHT` 结尾的裸 `+3`**（已核实 lib.rs:3648-3652）：其余四类都是具名常量算术，唯独三个权重 1 通道写成裸字面量。85% 置信是 `3 * M2_FUSION_CHANNEL_WEIGHT` 简写——但**将来新增权重 1 通道时极易漏改**，后果是 fused score 被系统性高估、影响 100bp 过滤线。
2. **`candidate_query` 只取最长单 token**：中文 statement 下所有 bigram 等长（6 字节），实际退化为"字典序最大的 bigram"、与语义无关；注释只描述后果不解释动机。50% 置信：单词宽召回+精排交给其他通道的设计，中文退化看起来非有意。
3. **`resolve_incremental`/`rebuild_incremental` 只是全量路径别名**：`previous` 校验后被丢弃。70% 置信是占位（若打算永远丢弃连校验都不必写）——但需定夺：是接口预留还是该删的死参数。
4. **停用词表收录 `android`/`ios`/`tiktok`**（已核实 lib.rs:1770-1776）：跨端知识库里 platform 词是最典型领域词汇，且它们是 `Applicability.platforms` 的合法取值——同一词结构化通道强信号、文本通道当噪声。60% 置信：针对某条噪声探针的调参残留；若是"平台词只走结构化维度"的有意分工，应有注释。`tiktok` 作为具体产品名进通用检索库硬编码表尤其可疑。
5. **覆盖率乘子的 30%/50% 阈值来源**：注释解释了为什么需要饱和点与下限，但两个具体数值无依据；65% 置信来自 probe fixture 调参——若有调参数据集应在注释里指向它。
6. **`MatchBasis::confidence()` 恒为 1.0/0.0 且排序完全不读它**：TD 说"置信度参与召回和排序"，实现里它只是展示字段。70% 置信：为将来非精确匹配基准预留的维度。

---

## 更新日志

- 2026-09-01 建立骨架，分析任务派发中。
- 2026-09-01 领域 F 结论：0 条，附 2 条自证排除记录。
- 2026-09-01 录入 E 6 条、A 6 条（含文档漂移清单）、D 5 条、C 9 条（含漂移清单）、B 6 条；六领域齐，共 32 条。每条均已先排除代码注释/TD/ADR/git log 可解释的情况。
