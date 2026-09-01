# 04 · 限制目标实现的设计及改造方案

> 状态：进行中。每条含：设计点 → 受限目标（G1–G5）→ 机制层面的理由 → 候选改造方案（及其与三条不动原则的关系）。
> 三条不动原则（不可推翻，改造方案须在其内）：①四字段 checkpoint 不加模型填写项；②Prompt/Hook 不产事实；③只有 candidate_confirm 产生 accepted。
> 更新日志见文末。

## 测试与验收基础设施（来源：领域 F，已 review；5 条均不触及三条不动原则）

### F-1 「模型真的会用」这一环无 CI 覆盖 → 限制 G3/G4 可信度

- **机制**（已核实）：CI 只跑 `cargo test --workspace --locked`（`.github/workflows/ci.yml:30`）；scenario-runner 六场景全是脚本化协议动作，model-in-the-loop 显式 deferred；唯一真模型证据 `codex_checkpoint_model_probe.py` 不在 CI 也不在验收脚本里，且只覆盖 Codex 不覆盖 Cursor。
- **后果**：若改动（如 schema 描述变差）导致模型调用行为静默劣化，无任何自动信号。8/31 桌面会话"激活成功但使用为零"正是这一环的真实事故。
- **改造候选**：① 真模型探针缩规模（trials 5–10）接入非阻断 nightly job，形成通过率时间序列；② 为 Cursor 补对等探针（需先确认 CLI 非交互批处理可行性）。

### F-2 联想探针聚合阈值允许"东好西坏"漂移 → 限制 G4

- **机制**（已核实）：断言仅 `search_hits >= 18` / `intent_hits >= 18`（`association_probe_workflow.rs:51-55`），zh ≥19/24。`DEVELOPMENT.md` 已点名的已知失败探针（zh-01/02/06/07）之外，某条由中变败、另一条由败变中，总分不变、测试仍绿。
- **改造候选**：① 显式维护 `expected_currently_failing` 集合，集合外逐条契约断言不许倒退，集合缩小作为正向追踪信号（成本小，推荐先做）；② 按类别（自然语言/标识符/噪声）分别设阈值 + 标准 IR 指标（见 01 文档条目 2）。

### F-3 G5 只有单一固定 oracle，无覆盖式探针 → 限制 G5 可量化性

- **机制**：`milestone-three-v1.json` 是一个手写场景的精确匹配；G4 有 46 条改写探针可给出召回率区间，G5 没有"改述/换语言/换命名风格后还能否跨端关联"的等价度量。
- **改造候选**：① 参照 association probe 结构为 milestone-three 语料建跨端改写探针集（工作量不小，需多端语料）；② 轻量版：先给现有 oracle 做几种跨端命名风格变体，验证命名不一致时 EngineeringReference 关联的边界行为。

### F-4 `run-acceptance.sh` 已过期，构成"假安全感"来源

- **机制**（已核实）：脚本不含 association probe、Checkpoint ACK 性能、retrieval_quality 等 Mew #226 之后的近十个验收目标，而其命名容易被新贡献者当作完整验收入口。
- **改造候选**：① 与 `docs/acceptance-report.md` 的 Reproduction commands 收敛为同一份清单（半小时级）；② 或标注 deprecated / 移除。

### F-5 性能断言全在白盒层，黑盒安装路径无延迟回归保护

- **机制**：p95/p99 断言都在 mcp/search/local-state 的 crate 测试里，测的是"函数多快"；真实宿主从 Hook 发起到拿到 marker/ACK 的端到端延迟（进程启动、IPC 编解码、`DEVELOPMENT.md` 记录的 Gatekeeper 0.4–2.5s 首验等）无自动化覆盖。
- **改造候选**：① `InvariantKind` 新增 `StepDurationWithinBudget`（需防 flaky，duration 不进 semantic_digest）；② 保守版：在 `installed_live_host_workflow.rs` 加非阻断耗时打点。

## CLI 与安装/发布链路（来源：领域 E，已 review；除 E-5 外均不受不动原则约束）

### E-1 `bin/current` 版本切换无单调性校验，降级完全不设防 → 限制 G2

- **机制**（已核实 `switch_current` installer lib.rs:2614-2628 确无版本比较）：安装版本完全来自调用方传入，无 SemVer 比较；降级会成功切换，直到旧二进制遇到更高的 runtime schema 才整体失败（fail closed 不丢数据，但错误消息无恢复指引）。
- **现实印证**：2026-08-31 用户装 pre-WP-O 离线 bundle 把 `bin/current` 无警告降回 0.1.0，会话 db7bf53b 复现已修复过的空 generation_id 拒绝——正是此缺口的真实事故。
- **改造候选**：① 读现有 manifest 做 SemVer 比较，默认拒绝降级，留 `--allow-downgrade` 恢复口；② 最小改动：`SetupReport.notices` 加降级警告 + schema 拒绝错误提示"运行 sctx upgrade 恢复"。

### E-2 npm dist-tag 无 SemVer 单调性保护 → 间接限制 G2

- **机制**（已核实 npm/scripts 无 `npm view` 调用）：`validateDistTag` 只校验 tag 名与 prerelease 的静态映射；npm 会无条件把 `latest` 移到新发布版本。旧 commit 重新打 tag 即可静默降级所有新装用户。
- **改造候选**：发布前 `npm view <pkg> versions` 做 SemVer 比较，更旧默认拒绝、显式 `--force` 放行。成本小。

### E-3 Skill 冲突即整体放弃安装，提示不足 → 限制 G2 开箱体验

- **机制**：已存在非本工具所有的同名 skill 目录时整体放弃（`SkillStatus::Conflict`），仅留一条 notice；用户可能不知情地缺失整条 workflow 引导。
- **改造候选**：安全边界值得保留；改善 doctor 输出，打印冲突路径与具体手动解决步骤。

### E-4 workflow.md「整会话只读一次」限制指导规则鲜度

- **机制**：控 token 成本的刻意设计；会话中途 upgrade 后新指导不生效直到下次 SessionStart。
- **改造候选**：给 SKILL.md 加版本号/hash，Hook 侧检测到版本变化才允许重读。

### E-5 candidate CLI 的 CAS 字段人工操作笨重（**受不动原则③约束**）→ 限制 G3 人工审阅可用性

- **机制**：confirm/discard 需手输 `--expected-task-id`/`--expected-intent-revision-id`/`--expected-review-version` 等长串 CAS 字段；这是原则③并发正确性的代价，**字段不能去掉**。
- **改造候选**（人体工程学层面）：`candidate get` 输出直接打印预填好 CAS 值的完整 `candidate confirm ...` 命令模板供复制。

### E-6 task-runtime schema 丢弃列表硬编码的维护性风险 → 限制 G2 升级可靠性

- **机制**：白名单手工枚举（11/12），schema 每次演进需人工记得更新，漏更会让合法升级路径误判为硬错误（安全但恼人的失败模式）。
- **改造候选**：改为"< 当前 schema 可丢弃"的整数区间规则，保留"未来版本一律 fail closed"。

### 附：review 过程中发现的现行缺陷（编排者验证，佐证 F-4）

**`sctx setup --demo` / `sctx demo` 当前已损坏**：WP-M2 把 MCP 工具面加到 **17 个**（`mcp/src/lib.rs:6789` 的 `tools_list()` 含 `space_create`），但 demo 主路径上的 `verify_demo_mcp`（`cli/src/main.rs:682→750`）仍硬编码 `tools.len() != 16` 即报错，Python oracle `tests/oracles/demo-v1.json` 的 `mcp_tools` 也只列 16 个（缺 `space_create`）。三处均已逐一核实。因为 demo 校验只被不在 CI 的 `run-acceptance.sh`/`demo_acceptance.py` 覆盖，此回归静默存活——这是 F-4「过期验收脚本造成假安全感」的一个已发生实例，直接损害 G2 的新用户第一入口。修复量：两处数字 + oracle 数组补一项。

## 存储与事件层（来源：领域 A，已 review；关键机制已逐条核实，均不受不动原则约束）

### A-1 「增量投影」实际比全量更贵，且实现版本变更=全量重建 → 限制 G2/G4

- **机制**（已核实 `schema.rs:1346-1348`）：`replace_projection_incremental` 第一步就 `create_tables(_next_)+populate(_next_, 完整 input)`——把整份影子投影（含全部事件字节）物化出来，之后才按受影响 Space 选择性搬运；进入前还要对新旧 blob 各跑一次完整 parse+reduce（全量只需一次），收尾 `refresh_superseded_by` 又对整个 live 投影重算。唯一净收益是未受影响 Space 的行不被重写——**净负收益**。同时 `source_file.content` 让 index.sqlite 完整复制事件语料，重建期间存在两份。
- **对目标**：G2——MCP 每次读先 `synchronize()`，语料变大后启动成本先被重建延迟吃掉；G4——改 tokenizer/排序权重必 bump 实现版本触发全量重建，**迭代成本与语料规模成正比，实质抑制召回算法演进**；且若未来加 embedding（重算贵几个数量级），必须先解耦"实现版本变更=全量重建"。
- **改造候选**：① 让增量名副其实（populate 接受 affected 过滤器、复用已缓存 old_input、JSON 全量 diff 改稳定哈希比较）；② 更彻底：删掉增量路径约 500 行，只保留全量 + 用 `cat-file --batch` 把全量做快 + `source_file` 不再存 content（index.sqlite 体积减半）。

### A-2 隐私门禁硬拒绝 + 规则激进，系统性挡掉最该沉淀的知识 → 限制 G1/G3

- **机制**（已核实）：`redact()` 生产路径零调用（仅测试用）、`PrivacyFindingKind::is_secret()` 分级方法**已存在但从未被使用**；规则匹配裸词 `token/secret/password` + `:` + ≥8 字符即拒——"决策：`auth_token: 必须由服务端下发`"这类正常工程结论会命中；任何邮箱（评审溯源）、`bearer `+12 字符（描述鉴权协议）、`sk-` 开头标识符同样命中。失败面是整个事件/整个 Confirmation 原子批，错误只有类别码无字段定位——用户在人工 confirm 的最后一步失败且不知道改哪。
- **对目标**：G1——鉴权/密钥管理类决策恰是最需要沉淀、误报率最高的一类；G3——直接压低确认转化率。
- **改造候选**：① **先做低风险版**：给 assigned-credential 加"值须具备凭据形态"（高熵/无空格/非自然语言）约束、裸词移出名字表、错误信息升级为字段路径+类别；② 分级处置：`is_secret()==true` 保持硬拒，PII/assigned-credential 在 confirm 前展示 redact 结果与位置，由人选"脱敏写入"或"确认误报原样写入"（在三原则内：不加 checkpoint 字段、事实仍只由 confirm 产生；需扩 confirm 契约入参）。

### A-3 v1 事件契约不可前向扩展，版本错配会让团队同步**彻底断掉** → 限制 G3/G5（本层投入产出比最高的改造点）

- **机制**（已核实）：`deny_unknown_fields` + 封闭 EventType——新事件类型/新字段对旧版本是**硬错误**而非隔离（隔离只覆盖 `schema_version != "1"`，测试已固化）；而 `validate_sync_head`（installer lib.rs:1767-1789）每次 sync 要求零解析错误+零 quarantine。后果链：任一成员先升级并推送新形状事件 → 未升级成员 merge 后校验失败 → 回滚 → **同步通道彻底中断直到全员升级**（不是"读不到新知识"这么轻）。
- **对目标**：G3——团队必须严格同步升级版本；G5——跨端团队升级节奏天然不一致，受影响更重。
- **改造候选**：① parser 内做 v1 前向兼容：加 `UnknownEventType` 隔离分支、`deny_unknown_fields` 改为"未知字段隔离+诊断、不参与归约与 semantic_hash"，sync 判据放宽为"零恶性错误"；② 最小缓解：只放宽 `validate_sync_head`，区分"我读不懂"与"事件本身坏"，前者 warning 不阻断（改动集中、风险低，但旧版本仍看不到新知识）。

### A-4 knowledge sync 校验成本 O(全部历史)，且无任何仓库维护 → 限制 G3/G5

- **机制**：每次 sync 前后各一遍全量校验（每事件 2 个 git 进程+完整 reduce）；每个 append batch 一个 commit、全仓无 gc/repack/prune（已核实 grep 零命中）；每次人工确认触发一次全历史 `git log`（新路径不在 memo）。共享路径成本是 O(团队累计知识总量)。
- **改造候选**：① 校验改增量（复用 `validate_append_only_since` 已拿到的 diff，只验新增事件；全量保留给 doctor 与首次 bootstrap）——append-only 已被独立证明，重复校验旧事件是纯冗余；② 在 maintenance 排他锁内跑 `git maintenance run --task=gc,commit-graph`（commit-graph 还能加速历史遍历）。

### A-5 语义召回缺口的存储层成因（已确认搁置，只记机制）

检索面全部是 FTS5 词法通道，无向量列、无 ANN、无 embedding 版本。跨端召回只能靠 RepositoryId/ArtifactLocator 的精确 token 命中，而跨端同一概念几乎不共享拼写（G4/G5）。**对未来有用的观察**：`IMPLEMENTATION_VERSIONS`+generation 机制已提供正确的失效框架，加 `embedding_version` 即可自然接入；真正的障碍是 A-1 的全量重建耦合。

### A-6 次要限制（低优先）

- **git 子进程不隔离用户全局配置**（已核实：生产路径无 `GIT_CONFIG_GLOBAL`/`gpgsign` 覆盖，只有测试基建隔离了）：用户的 `commit.gpgsign`（挂起等口令）、`core.hooksPath`、`core.autocrlf`（改变落盘字节）都会作用于知识仓库自己的 commit。修复成本极低（`Git::run` 统一注入 `-c` 覆盖），**建议顺手做**。
- **local-state 无条件 `use std::os::unix::*`**：Windows 整个 workspace 无法编译，与 npm 分发的跨平台隐含目标有张力（可能是有意范围限定）。

## 协议与宿主接入层（来源：领域 D，已 review）

### D-1 【高优先】Hook 是 MCP 授权唯一入口，静默失效时 G1/G2/G3 同时归零且能力报告谎报

- **机制**（已核实）：lease 唯一生产写入点是 SessionStart hook；MCP 每次调用硬要求 Enabled lease。Hook 未装/宿主不支持/Codex Trust 未确认/canonicalize 失败——任一情况下此后每次 MCP 调用都 `authorization_failed`。但 `evaluate_capabilities` 此时报 `mcp: true` + "MCP + CLI fallback" 文案——**`mcp: true` 不成立**（CLI 走 `*_at_root` 绕过 lease 确实可用，MCP 不行）。fail-open 的安静降级 + MCP 硬门 = 用户完全感知不到系统已停止工作。
- **改造候选**：① 给 lease 加显式非 Hook 建立路径（如 `sctx session authorize` 或 `task_intent_update` 在 cwd 落在已注册 checkout 内时自建；lease 是本地授权状态非事实，原则内；需补"自建 lease 会话不注入 marker"约束防削弱防注入属性）；② 最小改动：能力报告说真话（此状态 `mcp: false` + 可操作指引），并对"lease 完全缺失"这一种情况在 `authorization_failed` 里开口给指引（其余保持防探测折叠）。

### D-2 PostToolUse 不产生任何代码位置信号，G4 自动通路断在协议层

- **机制**（已核实）：归属机器算出"仓库 X 相对路径 Y"后整体丢弃（03 文档 D 领域第 2 条）；G4 只剩两条路：模型主动调 `task_artifact_focus`（"不知道自己不知道"）、`artifact_focus_reminder` 实验（**恰好是自动推送"该文件有 N 条历史 Context"的机制，但默认关闭**）。
- **改造候选**：① **推荐：把 `artifact_focus_reminder` 转正默认开启**——提醒只含 context_id+截断标题（≤800B/≤3 条），不带 statement/Evidence，是导航信息非事实注入（三原则均安全）；预算已提到 150ms、hook p99 门 500ms，可行；已有每(会话,仓库,文件)一次的去重。② 恢复 `normalized_tool_signals` 消费 file_hints 产 Workspace 信号（该 kind 定义明确非事实，8ea7136 之前就这么做；**需作者确认 #228 移除动机**）。
- **附带发现（确认为缺陷）**：`agent-adapter` 的 `is_shared_context_tool` 仍是 16 个工具名、**漏 `space_create`**（已核实）——该工具的 PostToolUse 会重新进入 Runtime 观察处理，正是同文件测试要防的事；无"两列表一致"断言可发现它。与文末"现行缺陷"的 demo 16/17 漂移同根因（WP-M2 加第 17 个工具时的硬编码漏改面），TD §13.2 也仍写 16 个工具。

### D-3 宿主支持矩阵只有 Cursor/Codex，Claude Code 用户的知识完全进不来 → 限制 G3/G5

- **机制**：`AgentKind` 封闭二元、installer 只写两家配置、`--client` 只收两值。代码里已有 Claude Code 形状的痕迹（`mcp__shared-context__` 前缀识别正是 Claude Code 的 MCP 命名约定）。接入成本实际很低：Claude Code hook payload 与 Codex 高度同构、六事件一一对应，纯策略层与 MCP 工具面都不用动。
- **改造候选**：① 新增 `adapter-claude-code` crate（主要工作量在 fixture 与 settings.json 合并逻辑）；② 若不打算支持，至少在 readme/user-guide 显式声明支持矩阵，避免 G3 被理解为"任意 Agent"。

### D-4 宿主能力不对称，同一份知识在不同宿主的采集质量不同 → 限制 G1

- **机制**（已核实关键点）：① Cursor 对 TurnStop 一律返回 `{}`——checkpoint 提醒被算出来后丢弃，Cursor 只在 preCompact 收到提醒，每轮工作结束无"该 checkpoint 了"的推动，知识沉淀率结构性低于 Codex；② Cursor 无失败信号（outcome 恒 Succeeded）——Issue/Risk/Validation 类 Context 最需要的"测试失败"输入在 Cursor 上不存在；③ Codex 的提醒走 `systemMessage` 通道，**是否进模型上下文未经验证**（9a95e41 的提交名暗示 marker 曾在此踩坑、被挪到 additionalContext）——若不进，所有 checkpoint/bootstrap 提醒对模型不可见。**第③条需实测确认。**
- **改造候选**：① 提醒统一改走已验证的 additional_context 通道；Cursor 的 stop 若确无输出通道，把提醒挂到下一次 PostToolUse；② Cursor 从 tool_output 只提取 exitCode/isError 结构化标量补失败信号（不带输出文本过缝）。

### D-5 Codex 子会话共享 session_id，并发 Agent 在协议层不可分辨 → 限制 G3/G1

- **机制**：Codex 对派生 thread 上报同一 session_id（上游限制，crate 文档已记录）；MCP 一切以 `(agent_kind, external_session_id)` 为主键——两个并发子 Agent 共享 lease/ActiveTask/Candidate 所有权域，**子 Agent A 能看到并确认子 Agent B 的 Candidate**。Runtime 的 goal-fork 只在 Intent 层补偿。
- **改造候选**：① 等 Codex 暴露 thread id（上游），建议先在 TD §13 显式记录为已知限制；② 服务端生成会话内子标识要求后续携带——考虑到"模型会臆造 session id"的实证教训，收益存疑，**不推荐优先做**。

### D-6 marker 一句话 + 无任何强制机制，pull 式成功率完全依赖模型合作 → 限制 G2/G1

- **机制**：SessionStart 全部产出是 ≤512B 一句话 + SKILL.md 门控；此后是否调 `task_intent_update`、是否 checkpoint 无一处强制。IntentBootstrapReminder 一次性；TurnStop 提醒 Cursor 上还被丢（D-4）。pull 式是用户定死的范围，此处只在 pull 内提高触达率：
- **改造候选**：① Reminder 从一次性改为带退避的有限次（如第 1/5/20 次安全工具事件，上限 3 次；布尔位改小计数器，不触授权面）；② `sctx doctor` 增加"最近 N 会话中 M 个 Enabled 但从未调 task_intent_update"——让 pull 式失败对人可见，零风险纯加法。

## 领域模型与生命周期（来源：领域 C，已 review）

### C-1 四字段 Checkpoint → Candidate 的字段坍缩：`assumptions`/`recheck_when`/`topic_key` 结构性恒空 → 限制 G1/G4（**受原则①约束，改造在服务端派生侧**）

- **机制**（已核实 lib.rs:4173-4178 硬编码空值）：`ContextRevision` 设计了 4 个"为什么/何时失效"字段，四字段路径只填得满 2 个（rationale、conditions）。所有自动生成的 Context 的 assumptions/recheck_when 永远空数组——Agent 拿到的是无时效标记的断言。同时 `topic_key` 只在 Claim 里写了可解析路径时才有值，纯业务决策（最常见）的 Context **永远不参与语义冲突检测**（冲突检测要求相同 topic_key）——G4 的"发现矛盾历史结论"对这类知识失效。
- **改造候选**（均原则内，服务端派生不受①约束、有 `derive_claim_references` 先例）：① Build 期派生：非 blocking 的 unknowns 映射成 assumptions（打 derived 标记、review 期可改可删）；路径解析失败时用最高频标识符构造 `identifier:` 形式的 topic_key 让业务决策也进冲突检测；② 保守版：review 视图把三项作为必填提示暴露给人（缺点：与 §12.4"用户不需重新填写"相反，人多半跳过）。

### C-2 结构化 recheck_when 子系统的输入集恒为空 → 限制 G1/G4

- **机制**：`doctor --recheck` 只认两种机器语法（`branch_advanced:`/`file_changed_since:`），而能填它们的入口只有 CLI 手工与 confirm edits——人不会手打机器格式。整条"知识失效自动检测"链路在实际部署中无输入。沉淀的知识没有过期机制，召回时已失效旧决策被同等对待。
- **改造候选**：① **推荐**：Build 期从已解析 Reference 自动生成 `file_changed_since:<HEAD>:<path>`（纯机械派生，advisory 不自动 deprecate，风险可控）；② 评估挪到检索期做排序降权（缺点：检索热路径引入 git 调用，与 ACK 性能约束方向相反）。

### C-3 Evidence"自包含"在四字段路径上坍缩为一句话 → 限制 G1/G3（**受原则①约束**）

- **机制**（已核实）：`supports`/`interpretation` 是 statement/rationale 的字面副本，`content` 是单键 `{"summary": 一句话}`——"最小充分证据"退化为"Agent 写的一句话"，另一个开发者读 accepted Context 时 Evidence 无法回答"这个结论怎么得出的"。evidence 非空不变量形式满足、语义落空。
- **改造候选**（原则内）：① `content` 组装时塞进服务端已有的确定性上下文（`{"summary":..., "references":[{repository,path,head_commit}], "derived":true}`）——零模型工作量，让证据在会话消失后仍能定位"当时看的是哪个版本的哪个文件"；② supports/interpretation 与原文相同时不落库（让 review 界面诚实显示证据密度，引导人补充；波及 event schema 兼容，成本可控）。

### C-4 【高优先】Candidate 评审入口绑死 ActiveTask 且无回切入口，唯一闸门会静默关死 → 限制 G3

- **机制**（已核实 switch_active_task 无生产调用者）：list/read/reserve 全部硬绑当前 ActiveTask；CLI 也强制 locator 走同一路径；30 天后终态过期。现实序列：checkpoint 产出 3 个 Candidate → 继续工作 goal 变化 → 自动 fork 并切换 ActiveTask → **3 个 Candidate 从此在任何入口都列不出来** → 30 天静默过期。原则③规定人工确认是唯一闸门，G3 卡死不是"人懒得确认"而是"人没有入口确认"。
- **不受原则约束**：③只规定 accepted 须经 confirm，不规定 confirm 只能由产生它的 ActiveTask 发起。
- **改造候选**：① **推荐**：installation 级评审收件箱（`list_all_pending_candidate_reviews` + `sctx candidate inbox`），确认路径的 ActiveTask 检查放宽为"source Episode 存在且 Pending"，group key 从 Candidate 反查 source Task（`read_proposed_space_group_mapping_for_candidate` 已存在正好是这个反查）；② 补充：恢复 `switch_active_task` 入口（方法已写好有测试，但只解决同会话回切）。

### C-5 一个 Task 只能开一个新 ContextSpace → 限制 G3/G5

- **机制**：group key 绑 task_id（74fb278 为修真实问题的权衡，但粒度从 IntentRevision 一路放宽到整个 Task）；ActiveTask 实践中生命周期很长、常横跨多个议题，所有新知识被强制收敛进同一个 Space——Space 的语义纯度归零，按 Space 组织/检索的价值随之归零。
- **改造候选**：① group key 改 `(Task, 推荐 Space Intent 的 canonical hash)`——保留 74fb278 的跨 revision 稳定性，允许一个 Task 开多个语义不同的 Space（`fallback_proposed_space_group_key` 已有派生实现可复合；本地表可丢弃重建）；② 显式 `allow_additional_space` 开关（把内部概念推给人，不推荐）。

### C-6 + C-7 Space 治理的两个互相掩盖的缺口（需同批修）→ 限制 G3、G1/G4

- **C-6 归属不可修**：CONTEXT.md/TD 把"错误归属可通过新关联事件修正"当核心卖点，但 `Correction` origin 在 cli/mcp **零出现**（事件构造器注释自认 domain-only）；叠加 C-5（归属出错概率不低）= **放错 Space 常见且不可修复**，团队知识库组织质量单调劣化。改造：`sctx context reassociate` CLI 子命令（reducer 已完整支持这条路径，只是从未被走过；保持 CLI-only 符合"组织决策是人的决策"）。
- **C-7 AssociationConflict 不阻断自动注入**（与 TD §6.3 明文不符）：冲突被计算、投影进 index，但 `AutoInjectionBlocker` 无此变体、search 层无引用——归属冲突的 Context 仍带着任选一个 head 的 Space 信息注入。当前不暴露纯因 C-6 让冲突不可达——**两个缺口互相掩盖，修 C-6 会立刻显形 C-7**。改造：reducer 加 `AssociationConflict` blocker（计算顺序已就绪，改动小），与 C-6 同批。

### C-8 accepted 知识没有 Agent 可达的废止路径，知识库只增不减 → 限制 G1/G4（**部分受原则约束**）

- **机制**：Deprecated 只能由 CLI 手工 withdraw 产生；`Supersedes` 关系边只影响 index 层 superseded_by 列（检索降权），**不进 AutoInjectionBlocker**——被推翻的决策与推翻它的决策 governance 状态完全相同，随仓库年龄增长 G4 召回准确率单调下降。原则③未规定废止，但"让 Agent 直接 withdraw"与"模型不做治理决策"的精神相悖。
- **改造候选**：① **推荐**：把 `Supersedes` 提升为领域层 blocker——边本来就是人工 confirm 时确认的，治理决策仍在人手；需加环检测（已有非无环关系诊断框架）；② doctor 的"废止建议"报告（又一个要人主动跑的入口，与 C-2 同病）。

### C-9 Applicability 精确字符串重叠 + 空维度视为无限制 → 限制 G5

- **机制**：G5 在领域层的唯一抓手是 `platforms`，但它是 Agent 自填的可选自由文本：没填 = 适用所有平台（对 G5 无信息量）；填了，`"fe"/"FE"/"前端"/"web"` 互不相等——冲突检测会漏掉真正矛盾的跨端结论（`"ios"` vs `"iOS"` 不重叠）。检索层已做归一而领域层冲突检测没做——**两层对同一字段读法不一致**。
- **改造候选**：① 比较处做与 search 层一致的归一（保留原始拼写、只在比较归一，与 canonical 快照同模式；需一次 rebuild）；② 封闭平台枚举（与 §5.7"不引用外部可变分类表"冲突，不推荐）。

### C-10 goal-fork 是被术语表明文否定的隐式任务边界推断 → 限制 G2

- **机制**：CONTEXT.md 写 ExternalSession"不推断任务边界"、ActiveTask 变更是"显式决策"，而 `continue_working_intent` 在 parent 过期 + goal 归一化不等时**自动 fork 并切换 ActiveTask**。它解决的是真实问题（并发 Agent 互相覆盖 Intent head，有注释与回归测试），但与 C-4 叠加成了 Candidate 消失链的触发器——是"被文档否定过的取舍被重新采纳且术语表未同步"。
- **改造候选**：① fork 但不切 ActiveTask（切断 Candidate 消失链；但 checkpoint 需接受按 TaskSession 定位，扩散到 MCP 层，成本中等）；② 至少同步 CONTEXT.md 承认这条唯一的隐式边界（CONTEXT.md 是被当规范读的文档，漂移本身就是风险）。

## 检索与联想层（来源：领域 B，已 review；关键机制已逐条核实）

### B-1 无语义向量通道：所有召回都是 token 级精确匹配 → 限制 G4/G5/G2（已确认搁置，记录机制）

- **机制**：文本通道全走 FTS5 MATCH 字面切分；唯一"同义"机制 token_alias 的两个来源（identifier_split 拼写变体、domain_term 人工词表）都不是语义的，且别名扩展要求查询已命名同组 ≥2 成员——只能"补全说对了一半的标识符"，无法映射完全不同的说法。
- **候选**（供 ADR 参考）：① sqlite-vec 做第 9 个 RRF 通道、**不单独构成自动注入资格、需他通道背书**（三原则无冲突；模型版本并入 generation）；② 不引入模型：`candidate_confirm` 时人工顺手登记同义词扩充 domain_term（须做成人工编辑而非 checkpoint 字段，否则违反①）。

### B-2 Engineering Graph 没有 Artifact↔Artifact 边，TD §8 的两跳扩展实为 Context 关系跳 → 限制 G4/G5

- **机制**（已核实 `EngineeringProjection` 只有 contexts+references 两个集合）：TD 承诺 contains/calls/implements 边与"Symbol→API/Schema→Contract Context"扩展，实际可达路径是 focus→精确 locator 相等→Reference→宿主 Context→ContextRelation 两跳。**从当前 Symbol 走不到它调用的 API**，除非有人为那个 API 单独记过 Reference 且两 Context 间有人建过关系边。测试已确认同一 locator 在两仓是互不相通的两个节点——G5 的跨端验收成立的前提是两端手工登记了同一坐标。
- **改造候选**：① **先做最保守的 `contains` 边**（File→Symbol，scanner 现有数据已够，不需类型推导；focus 一个文件可扩展到文件内所有 Symbol 挂的 Context，覆盖"我在改这个文件"最高频场景；投影 schema 升级+全量重建一次+路径解释加 artifact_hops）；② calls/implements 走 SCIP（与有界扫描约束正面冲突，不建议近期）。**同时修正 TD §8 把未实现的边标注清楚。**

### B-3 精确 locator + 不做 relocation：知识↔代码的连接随重构静默衰减 → 限制 G4/G2

- **机制**：三个设计叠加——只有唯一精确匹配才建关联、move/rename 后旧 Reference 保持 missing 且明确不查 rename history、检索不触发 rebuild。代码每次重构一批 Reference 悄悄变 missing，权重最高的 Graph 通道随之失效，检索静默退回纯文本，**没有任何机制告知任何人**。与 G1 价值主张的张力：知识本应比代码活得久，这套设计让"知识↔代码"的连接比代码死得快。
- **改造候选**：① **先做可见性**：`association_rebuild` 返回四态计数+新增 missing 列表、diagnose 报告 missing 比例（数据已有，只缺聚合出口）；② missing 时给人工确认的 relocation **建议**（系统只提议、人确认后写新 Reference 事件——在三原则内，但与 TD"不生成 relocation candidate"字面冲突，需先修订该设计决策，它不是三条不动原则）。

### B-4 【最低成本高价值】自动路径的 60% 覆盖率闸对中文长问句几乎不可通过 → 限制 G2/G4

- **机制**（已核实两处代码）：自动路径覆盖率分母是**全部查询 token**；显式搜索路径已经修过同一问题（answerable 分母，注释明说不修"整条查询什么都返回不了"）——**修正没同步到自动路径**。18 字中文问句经 bigram 产 17 个 token，语料认识的可能只有 3 个，覆盖率 ~1765bp 远低于 6000bp 闸；自动召回只剩三条各有前提的旁路（整句子串包含/提问带标识符/双通道）。探针验收允许 22 条过 18 条，**最可能挂掉的 4 条恰好落在中文自然语言类**——聚合阈值（F-2）掩盖了这一系统性弱点。
- **改造候选**：① **把 answerable 分母下沉到自动路径**（DF 数据已在手上；需重跑探针校准并守住 noise 探针零命中——放宽分母同时放宽噪声）；② 词典分词（见 01 文档条目 15，需全量重建+全面调参）。

### B-5 跨仓不是一等检索维度，G5 = "人工已连才可召回" → 限制 G5

- **机制**：`Applicability` 无 repository 维度，RepositoryId 只在 Graph 通道可见且是**相等条件而非可跨越维度**。"FE 召回 Android 约束"只有三条人工路径：同一 locator、人工关系边、同 Space/同 platforms 值——没有一条是系统自动发现的。
- **改造候选**：① RepositoryId 提升为 Applicability 第四维度，**由服务端从 ActivationScope/Catalog 派生**（Agent 填则违反①，服务端派生完全在原则内，且是"服务端拥有身份"的既有做法；涉及 domain+event-schema 跨层变更）；② 保守：在 contains 边之上做跨仓同名 API/Schema locator 的"契约对齐"候选、人工确认后成关系边（受③约束、在原则内）。

### B-6 Usage prior 是安装本地的，"哪些知识真的在被用"无法跨人共享 → 限制 G3

- **机制**：A 反复复用证明有效的 Context，在 B 的安装上与从未被用过的完全同权。Git 共享的是事实，使用统计不在其中。
- **改造候选**：① usage 计数做成可选发布的聚合**非知识事实**（不能作 Evidence、不影响 lifecycle——与关联置信度的既有约束同类，有先例；有隐私面需评估）；② 保守：只共享"被人工 confirm 引用过"这一强信号（完全复用③的既有产物，零新事实类型，但信号稀疏）。

### B-7 检索热路径六处 O(N)/N+1，且失败静默 → G2 静默劣化

- **机制**（关键项已核实）：(a) Graph 快照每查询全量反序列化两次；(b) focus 匹配 O(refs×contexts) 线性查找（另一处已建 map，风格不一致）；(c) ContextRelation 加载 2N+1 查询；(d) Scope 匹配全量扫描+Rust 端过滤（索引建了没用上）；(e) DF 统计每检索数百条 FTS COUNT；(f) **最要命**：Hook 热路径 `json_extract` 全表扫（150ms 预算超时静默返回空、`ok()?` 吞掉一切错误）——G2 最直接的兑现形态会随规模"无声地不太灵"。
- **改造候选**：全部等价重写、三原则无冲突；**优先 (f) 生成列+索引、(b) 建 map（五行）、(e) fts5vocab**。另补一条：focus 超时写本地 diagnostic 让 doctor 能报（本地诊断不是事实，②无冲突）。

### B-8 problem_view 质量完全取决于 Agent 写 Intent 的详细度（**受不动原则约束**）→ 限制 G1/G2

- **机制**："用问题找知识"的关键字段 BM25 权重最高档（8.0），但来源是 `goal|in_scope|open_questions` 的派生拼接且除 goal 外全可选——Agent 只写 goal 时问题面退化成一句话，高权重×低信息量=通道形同虚设。最自然的两条修复路（模型显式写问题/从 Prompt 推断）分别违反①②。
- **改造候选**（均原则内）：① **首选**：confirm 的 review 呈现里把 problem_view 提为显眼可编辑项（机制已具备——edits 优先、缺省回填派生值；人在填不是模型在填，走③）；② 派生规则纳入 constraints 与 acceptance_conditions（纯服务端派生零填写负担；注意 400 字截断改按字段优先级）。

**B 层优先级清单**（报告原文，我认可）：① focus 生成列+索引；② answerable 分母下沉自动路径；③ focus 匹配建 map；④ fts5vocab；⑤ diagnose 报 missing 比例。B-2（A2A 边）是目标层面最大缺口但成本高，先做 contains 子集+修 TD。

---

## 跨领域综合结论（编排者，基于六份已核实报告）

1. **G4 的"自动"通路存在三段独立断裂，叠加后默认安装下几乎不通**：协议层把仓库归属信号整体丢弃且 focus reminder 默认关闭（D-2）→ 检索层中文覆盖率闸卡死自然语言召回（B-4）→ 领域层纯业务决策的 topic_key 恒空、永不参与冲突检测（C-1）。三个领域各自独立发现、指向同一条主链。修复优先级建议：B-4（最便宜）→ D-2 方案①（reminder 转正）→ C-1 派生。
2. **G5 的现状是"人工已连才可召回"**：F（唯一样本是单一固定 oracle）、B（无跨仓维度、无 A2A 边）、C（platforms 自由文本+精确比较）、A（身份稳定性已就绪但无语义桥）。四层结论互相印证：G5 目前只有地基（RepositoryId/EngineeringReference），没有自动连接机制。
3. **"静默失败"是系统性模式，不是孤立 bug**：lease 失效但能力报告谎报 mcp:true（D-1）、focus 超时吞错（B-7f）、团队同步断裂无恢复指引（A-3）、模型环路无任何回归信号（F-1）、降级安装无警告（E-1）、Candidate 静默过期（C-4）。共同点：失败发生时**没有人类可见的出口**。建议把"可观测性"立为一个横切工作包（doctor 聚合 + 各失败点写本地 diagnostic），成本低且是其他修复的前提。
4. **WP-M2（加第 17 个工具）暴露"公开工具面无单一来源"**：四处硬编码漏改——demo 校验 16、demo oracle 16、`is_shared_context_tool` 16、TD §13.2 写 16。除逐处修复外，应加"工具面清单唯一来源+各消费点一致性断言"。
5. **文档漂移是普遍现象**（A 6 处、C 9 处、D 2 处，含 CONTEXT.md 两条术语定义被实现推翻）：CONTEXT.md/TD 在本项目里被当规范读，漂移本身就是工程风险。建议独立的文档修订 WP，且每条漂移先由人定夺"文档错还是实现错"（清单见 03 文档）。
6. **已证实的现行缺陷两处**（修复量极小）：demo 主路径因 16/17 漂移已损坏（本文档 E 节附录）；`is_shared_context_tool` 漏 space_create（D-2 附带发现）。

---

## 更新日志

- 2026-09-01 建立骨架，分析任务派发中。
- 2026-09-01 录入领域 F 的 5 条设计限制（F-1 至 F-5），其中 F-1/F-2/F-4 的机制性声明已抽查核实。
- 2026-09-01 录入 E 6 条（附 demo 16/17 现行缺陷）、A 6 条、D 6 条（附 space_create 漏改缺陷）、C 10 条、B 8 条；六领域齐，共 41 条。每领域的最关键机制声明均经编排者抽查核实。
- 2026-09-01 增加「跨领域综合结论」6 条：G4 三段断裂、G5 人工连线现状、静默失败系统性模式、工具面无单一来源、文档漂移普遍、两处已证实现行缺陷。
