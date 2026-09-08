# 低优先级问题记录

来源：2026-09-01 召回注入链路诊断与 2026-09-02 真实会话验证（Codex 01a060a1、Cursor 桌面 cba4000e、Cursor d0773d8f）。本文件只收录**不直接阻碍 shared-context 目标达成**（跨会话知识沉淀、当前任务无路由发现历史知识）的问题；直接影响目标的问题在第二轮 WP（T1–T5）中处理，不在此列。

每条格式：现象 / 证据 / 为什么低优 / 升级条件或方案方向。

## 观测与诊断噪声

1. **PostToolUse 双行诊断**：每次 SharedContext MCP 调用产生 `attribution_failed(neutral)` + `ok(enabled)` 两行，detail「enabled PostToolUse has no merge operation」把预期路径写成失败语气。累计 30+ 次。纯观测噪声，不影响行为。方案：SharedContext 类工具事件跳过 attribution 记录或改中性 reason。
2. **首条 prompt 不进 TaskSignal**：`prompt_signal_skipped_no_task` —— prompt_submit 必然早于 task_intent_update。mid-session 已实证生效（d0773d8f ×1、01a060a1 ×5）；首轮语义由 Intent 本身承载，损失有限。升级条件：单轮短会话占比显著。方案方向：无 Task 时暂存一条、Task 创建时补挂。
3. **`doctor` 报 Codex trust unconfirmed 误报**：0.150.1 vs fixture 0.147.0，但同机 hook 全程工作（hook_event 为证）。诊断面与实际矛盾，属探测逻辑对新版本的误判。
4. **startup cwd 被删时 `canonicalize` 失败记 `authorization_internal`**（一次，与清理 worktree 时间窗吻合）。环境特例。方案：canonicalize 失败时沿用租约既有决定而非 fail_open。

## 提示与错误文案

5. **`invalid_input` 泄漏 serde 内部措辞**（「expected struct WorkingIntentSnapshot」）且无恢复指引。模型自我纠错效率问题，非链路阻断（cba4000e 中模型第二次就改对了）。方案：按 `session_not_authorized` 家族的模式拆分常见形状错误并给指引。**2026-09-08 部分修复**：`intent` 非对象这一条已在 serde 之前拦下（`validate_intent_composition`，`crates/mcp/src/lib.rs`），`task_intent_update` 与 `space_create` 各报自己的必填字段。选它先修是因为 cursor 会话 4b2e9fb5 实证了最坏形态——cursor 动态工具下 `inputSchema` 首调前不可见，错误消息是模型唯一的自纠依据，那次为此花掉一次失败调用加一次 schema 拉取。**其余形状错误仍是 serde 原文**（`claims[].evidence`、`locator`、各 id 字段等），要不要继续逐条拦取决于是否还有同样的实证。
6. **stop 催促可能过频**：无 checkpoint 的每一轮 turn_stop 都推「call task_checkpoint…」。单轮会话未成骚扰，长会话形态可预见。方案：每 Session 限次，或仅在本轮有实质工具活动时催。
7. **marker 512 字节上限仅 `debug_assert` 守护**（release 无检查）。当前两种形态都远小于上限。

## 升级与运维

8. **MCP serve 进程存活跨过升级**：`bin/current` 已切到新版，编辑器里已启动的 serve 进程仍持旧二进制（实测 4 个进程、最早早于安装时刻；cba4000e 因此在热修复已装后仍命中旧 bug）。方案：upgrade 输出加「重启编辑器/重连 MCP 生效」提示；serve 启动时记录自身版本并在响应元数据中携带。
9. **`graph_rebuild_pending` 在 `auto_scan=false` 时语义漂移**：从「重建失败」漂移成「Graph 落后」，未拆分状态码（R4 遗留）。
10. **`association_rebuild` 的 `RepositoryScannerLimits` 硬编码**，无配置入口；`--max-artifacts` 只影响 `repository scan` 的响应截断（R4 遗留）。

## 设计空白（进设计讨论，非急修）

11. **EngineeringReference 不携带分支上下文**：沉淀于分支 A 的引用在分支 B 的 checkout 下判 Missing（正确）但无「属于另一分支」提示；`recheck_when: branch_advanced` 机制存在未被用上。d0773d8f 前置分析中 4 条 `.kt` 即此类。
12. **CONTEXT.md 术语表与实现不符**：「Supersession belongs to Revision and governance causality, not ContextRelation」，而 domain/index/confirm 均把 `Supersedes` 作为普通 ContextRelation（P0 热修复后 search 亦然）。文档需对齐实现。
13. **跨版本 Candidate 恢复窗口**：旧版本已提交 Git、runtime 停在 Queued 的 Candidate，新版本重算 content hash 不同时可能触发 `deterministic Candidate content hash changed across retry`（R2-B 遗留，极窄窗口）。
14. **artifact focus reminder 与其他 additional_context 的「先到先得」耦合**：`add_artifact_focus_reminder` 在字段已被占用时放弃提醒；当前无重叠场景但耦合脆弱（R1-H1 遗留）。~~WP-V5 的 maintenance hint 是第二个消费者~~——2026-09-08 随 ADR-0006 移除，该 slot 回到单消费者（self-heal marker 与 focus reminder 之间靠调用顺序排他，`add_self_healed_activation_marker` 在前）。规则仍然是「谁先占谁赢」这个约定，没有共享的 slot 所有权结构；下一个消费者出现时应当先做这个结构。

## 文档与测试债

15. **三处旧文档仍写 16 个 MCP 工具**：`technical-design.md:1076`、`docs/acceptance-report.md:13/102/126`、`docs/mew-iteration-timeline.md:150`（代码与 oracle 已是 17 并有一致性断言）。
16. **零散过时注释/冗余字段**：`hook_session_activation.rs` 的 lease_record_path 注释仍称「SessionStart 是唯一写者」；`insert_review_aids` 的 `derived_problem_view` 响应字段对新 Candidate 与 `content.problem_view` 重复。
17. **scenario-runner 设计上无法驱动真实 agent**（明令拒绝 codex/cursor/claude 可执行）：「模型看到 marker 后是否真调工具」只能靠 `tests/scripts/codex_checkpoint_model_probe.py` 类手动探针覆盖。长期验收缺口。
18. **B3 英文 Intent vs 中文知识库**：workflow 语言指引已缓解（d0773d8f 实证中文 Intent），剩余跨语言召回由 embedding 通道（ADR-0004）覆盖；此处仅记录现象。
19. **zh-09 类 Space Intent BM25 抬升无关 Space**：`space_intent_bm25` 通道命中「配置/置下」类弱词即可让无关 Space 反超，item 层 coverage 乘子下限 5000 拉不回（R3 分析）。单探针问题，待 embedding 通道落地后重估。
20. **探针 harness 的 usage prior 自我干扰**：连跑探针累积 `ignored` 计数可影响 1bp 级排序差（R1-S1 发现）；T1 修 usage 判定时顺带在 harness 中隔离 usage 状态。

## 2026-09-04 新增（第三轮 U1–U4 期间发现，均只报告未修）

21. **`AssignedCredential` 规则可能误伤域 id**：`token:`/`secret:`/`credential:` 等 key 名 + 分隔符 + ≥8 字节值即命中；若文本写成 `auth_token: ctx_<uuid>`，即便 id 已被 U4 的掩码替换，该规则仍按「关键字+分隔符+非空值」结构触发。与 U4 的修复正交，需单独跟踪。
22. **`scan_phone_numbers` 的反向缺陷（漏检）**：真实号码后紧跟 `(` + 单词（如 `+1 415-555-0132 (building on ...`）时，`(` 被当作格式字符继续吞入，随后遇字母使 `boundary_after` 失败，整段贪婪匹配被丢弃且不回退到更短的合法前缀 → 假阴性。同一规则表内的不对称问题。
23. **长十六进制串（commit sha 等）的同类小概率误判**：纯数字子串恰好落入电话号码窗口时，与 U4 修掉的 UUID 场景同源，概率远低，未处理。
24. **installer 展示层未呈现 `timed_out_during_backfill`**：U3 已把该字段写入 `encode_sample` 与 `EncodeLatencySummary`，但 `sctx embedding status` / `doctor` 的文案没有把「回填窗口内超时」与「预算不匹配硬件」的区分展示给操作员（数据已在，仅缺展示）。
25. **1200ms 编码预算对满长语料几乎无余量**：U3 在高负载下实测 1400 字符 encode 达 1062ms（T5d 定预算时机器较闲，测得 816ms）。查询侧因抢占已不受影响，但该预算本身的余量值得后续复核。

## 2026-09-04 新增（治理自动化 V1–V6 实施期间发现，均只报告未修）

26. **compact triage 行缺目标 Context 的 status**：自动丢弃第一档要求「exact_duplicate 且目标仍 accepted」，但 `candidate_list` 紧凑行只给 `target_context_id` 不给其 status，模型须额外 `context_get` 才能判定。若实测出现误 discard 指向已 deprecated Context 的重复条目，正解是服务端在紧凑行带上目标 status。
27. **git 网络子进程超时用 SIGKILL、不杀进程组**：`Child::kill()` 不会终止 git spawn 的传输助手/`receive-pack`，超时后可能短暂残留孤儿进程（workspace `unsafe_code = "forbid"` 无法用 `pre_exec` 建进程组）。独占锁必然释放，影响仅进程残留。
28. **doctor 无 LaunchAgent 检查**：plist 被手工删除或 job 未注册，目前只有下次 setup 的 notice 会提示。方案：doctor 加 `launch_agent` 检查（文件存在 + `launchctl print` 探测）。
29. **launchd job 依赖 gui domain 注入的 `HOME`**：plist 未写 `EnvironmentVariables`，`maintain run` 靠 HOME 解析安装根。可改为把 `--root` 写进 ProgramArguments。
30. **墙钟断言型测试在高负载下假失败率偏高**：`engineering_workflows::public_mcp_artifact_focus_...`（本日并行负载下挂 3 次，安静时段一次跑过全量）、`scenario-runner::concurrent_fault_...`、`hook_hot_path` p99、`cli_contract::embedding_remove_...`。隔离重跑均过。方向：latency 断言改条件化（如负载探测）或串行执行；CI 必须 `--no-fail-fast` 否则 flake 会截断后半程。
31. **`skills/*/agents/openai.yaml` 的宿主实际效果未验证**：仓库只有字节级断言，无任何测试证明宿主读取该文件；`sctx-review` 的 `allow_implicit_invocation: false` 是按形态推断写的。
32. **机会轨 spawn 使 Enabled SessionStart 多写一行 hook_event**（`opportunistic_maintenance_started`/`_unavailable`，Neutral）。目前无精确条数断言，未来加断言时注意。
33. **`~/Library/LaunchAgents` 若由本产品创建为 0o700**（沿用 `ensure_directory_preserving_mode`），比系统惯例 0o755 严；功能正常，仅与惯例不一致。

## 2026-09-04 新增（真实会话 01a06b3e 分析，用户裁定暂缓）

34. **Codex `SessionEnd` 从未解码成功**：`adapter-codex/src/lib.rs:223` 的 `require_one_of(reason, ["other"])` 只接受字面量 `"other"`，Codex 0.153.2 发送其他 reason 值 → 累计 53 次 `payload_decode_failed`（N2 指纹确认 keys=[cwd,hook_event_name,reason,session_id,transcript_path]）。WP-O 当初只放宽了 Cursor 的 lifecycle 枚举透传，漏了 Codex 此处。后果：SessionEnd 的租约/提醒清理从不执行，孤儿租约靠 30 天回收兜底。修法：与 `adapter-cursor/src/lib.rs:198,274` 对齐（透传 + 非空校验），一行级。用户 2026-09-04 裁定：不紧急，后续再修。


## 2026-09-07 新增（F2LLM 换代收尾，W-C 发现）

35. **`model_fingerprint` 不跟随符号链接**：`crates/search/src/embedding.rs` 用 `DirEntry::metadata()`（Unix 上不解引用 symlink），模型目录若由符号链接拼成（如指向 HF 缓存）会以「holds no files」响亮失败。installer 写真实文件不受影响，仅手工用 `embedding_model_path` 指向 symlink 布局的操作者会踩到；失败是响亮的不是静默的。修法一词级：`fs::metadata(entry.path())`。

## 2026-09-08 新增（ADR-0006 白名单修复期间发现）

36. ~~**`carries_model_visible_context` 对 Codex 已经不准**~~ —— **2026-09-08 随 ADR-0006 同轮修复**。原问题：`crates/cli/src/main.rs` 的这个判定（`PostToolUse | PreCompact | TurnStop`）是 adapter 无关的，用来决定「自愈 activation marker」值不值得花掉那次一次性投递；白名单修复之后 Codex 的 `PreCompact`/`TurnStop` 已不再携带 `additionalContext`，于是投递被花在会被丢弃的字段上、`try_mark_activation_marker_delivered` 还把它记成已投递。修法：真值来源下沉到 adapter 本地——`sctx_adapter_codex::delivers_model_visible_context` 由 `hook_specific_output_event_name` 直接派生（两者不可能漂移），`sctx_adapter_cursor::delivers_model_visible_context` 与其 `encode_hook_output` 的 match 同形并有逐事件一致性测试；cli 只按已有的 `agent` 字符串分派（与 `agent_capabilities`、编码出口同一套），另加一条策略排除：Prompt 事件即便宿主能送也不投递。触发条件不是理论——遥测里确有 `maintenance_lock_busy` 导致的 SessionStart fail-open。集成覆盖见 `hook_task_signals::a_codex_lease_repaired_at_a_compaction_or_stop_boundary_keeps_its_marker_delivery`。

37. **一次性 marker 投递只在「创建租约的那个事件」上提供**：`resolve_hook_authorization_inner` 只在 `AuthorizedSessionScopeRead::Missing` 分支里判断要不要投递 marker，`Current` 分支不判断。原因是租约本身分不清两种来源——`SessionStart` 渲染 marker 时**不**写 `activation_marker_delivered`，所以「SessionStart 建的租约」和「自愈建的租约」都是 `activation_marker_delivered: false`，在 `Current` 分支上放开投递会让每个正常会话的第一个 PostToolUse 重复一次 marker。后果：#36 修好之后，一个在 Codex `PreCompact`/`Stop` 上自愈的租约虽然不再谎报投递，但也拿不到第二次机会，该会话仍然全程没有 marker（与修复前的最终结果相同，区别只在租约不再撒谎）。正解方向：让 `SessionStart` 在建租约时就把 `activation_marker_delivered` 记成 true（最好作为 `try_authorize_missing` 的入参，避免热路径上多一次写），此后 `!activation_marker_delivered` 才是可信的「这个会话还没被告知过自己的 id」，`Current` 分支即可安全地补投一次。触发面与 #36 相同（宿主漏发 SessionStart 或租约损坏，且下一个事件恰好是 Codex 的 PreCompact/Stop）；Cursor 不受影响。`hook_task_signals` 里已有一条断言钉住当前行为，修这条时要一并翻转。
