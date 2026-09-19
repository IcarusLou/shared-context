# 低优先级问题记录

来源：2026-09-01 召回注入链路诊断与 2026-09-02 真实会话验证（Codex 01a060a1、Cursor 桌面 cba4000e、Cursor d0773d8f）。本文件只收录**不直接阻碍 shared-context 目标达成**（跨会话知识沉淀、当前任务无路由发现历史知识）的问题；直接影响目标的问题在第二轮 WP（T1–T5）中处理，不在此列。

每条格式：现象 / 证据 / 为什么低优 / 升级条件或方案方向。

## 观测与诊断噪声

1. **PostToolUse 双行诊断**：每次 SharedContext MCP 调用产生 `attribution_failed(neutral)` + `ok(enabled)` 两行，detail「enabled PostToolUse has no merge operation」把预期路径写成失败语气。累计 30+ 次。纯观测噪声，不影响行为。方案：SharedContext 类工具事件跳过 attribution 记录或改中性 reason。
2. ~~**首条 prompt 不进 TaskSignal**~~ —— **2026-09-12 随 WP-G2a 修复**。原问题：`prompt_signal_skipped_no_task`，prompt_submit 必然早于 task_intent_update（d0773d8f ×1、01a060a1 ×5）。修法即当初的方案方向：`pending_prompt_signal`（runtime schema 21，无外键——正因为此刻还没有 `external_session` 行）暂存至多 2 条已脱敏截断的 prompt，`open_or_create`/`start_new_task` 在建 Task 的同一事务里排到 signal 前列（`task_signal` 无时间字段，ordinal 次序就是"更早"的全部含义）。斜杠命令不占槽；48 小时过期，256 行全局上限。**仍未覆盖**：全新安装的第一个会话——`runtime.sqlite` 尚不存在，而 prompt 不得成为创建它的那个事件，这条不变量优先级更高。
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
14. **artifact focus reminder 与其他 additional_context 的「先到先得」耦合：已随 R2-2c 退役解除** — 提醒执行体与去重状态访问已删除，旧文件保留为惰性遗留数据。self-heal 与 PreCompact 激活标记及其不覆盖已有 model context 的守卫保留；未来增加 additional_context 消费者时仍应明确字段所有权，而不是重新依赖提醒的调用顺序。

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

34. ~~**Codex `SessionEnd` 从未解码成功**~~ —— **2026-09-09 随 R1-1 接管修复**。N2 实际指纹 keys=[cwd,hook_event_name,reason,session_id,transcript_path] 缺少 `model`，真根因是 `Common.model: String` 与诊断 shape 校验均无条件必填，解码先于 reason 白名单检查失败。先前将问题归因为 reason 枚举并称「一行级修复」不充分；并行会话已提交的 reason 非空透传保留，本次叠加 `Option<String>` 与按事件校验：五个携带 model 的事件仍必填非空，SessionEnd 可省略。追加真实五键指纹 fixture，CLI 真二进制测试同时证明 `hook.codex.session_end` 成功遥测、无 fail-open 和租约删除；既有含 model 的清理用例保持通过。


## 2026-09-07 新增（F2LLM 换代收尾，W-C 发现）

35. **`model_fingerprint` 不跟随符号链接**：`crates/search/src/embedding.rs` 用 `DirEntry::metadata()`（Unix 上不解引用 symlink），模型目录若由符号链接拼成（如指向 HF 缓存）会以「holds no files」响亮失败。installer 写真实文件不受影响，仅手工用 `embedding_model_path` 指向 symlink 布局的操作者会踩到；失败是响亮的不是静默的。修法一词级：`fs::metadata(entry.path())`。

## 2026-09-08 新增（ADR-0006 白名单修复期间发现）

36. ~~**`carries_model_visible_context` 对 Codex 已经不准**~~ —— **2026-09-08 随 ADR-0006 同轮修复**。原问题：`crates/cli/src/main.rs` 的这个判定（`PostToolUse | PreCompact | TurnStop`）是 adapter 无关的，用来决定「自愈 activation marker」值不值得花掉那次一次性投递；白名单修复之后 Codex 的 `PreCompact`/`TurnStop` 已不再携带 `additionalContext`，于是投递被花在会被丢弃的字段上、`try_mark_activation_marker_delivered` 还把它记成已投递。修法：真值来源下沉到 adapter 本地——`sctx_adapter_codex::delivers_model_visible_context` 由 `hook_specific_output_event_name` 直接派生（两者不可能漂移），`sctx_adapter_cursor::delivers_model_visible_context` 与其 `encode_hook_output` 的 match 同形并有逐事件一致性测试；cli 只按已有的 `agent` 字符串分派（与 `agent_capabilities`、编码出口同一套），另加一条策略排除：Prompt 事件即便宿主能送也不投递。触发条件不是理论——遥测里确有 `maintenance_lock_busy` 导致的 SessionStart fail-open。集成覆盖见 `hook_task_signals::a_codex_lease_repaired_at_a_compaction_or_stop_boundary_keeps_its_marker_delivery`。

37. **一次性 marker 投递只在「创建租约的那个事件」上提供**：`resolve_hook_authorization_inner` 只在 `AuthorizedSessionScopeRead::Missing` 分支里判断要不要投递 marker，`Current` 分支不判断。原因是租约本身分不清两种来源——`SessionStart` 渲染 marker 时**不**写 `activation_marker_delivered`，所以「SessionStart 建的租约」和「自愈建的租约」都是 `activation_marker_delivered: false`，在 `Current` 分支上放开投递会让每个正常会话的第一个 PostToolUse 重复一次 marker。后果：#36 修好之后，一个在 Codex `PreCompact`/`Stop` 上自愈的租约虽然不再谎报投递，但也拿不到第二次机会，该会话仍然全程没有 marker（与修复前的最终结果相同，区别只在租约不再撒谎）。正解方向：让 `SessionStart` 在建租约时就把 `activation_marker_delivered` 记成 true（最好作为 `try_authorize_missing` 的入参，避免热路径上多一次写），此后 `!activation_marker_delivered` 才是可信的「这个会话还没被告知过自己的 id」，`Current` 分支即可安全地补投一次。触发面与 #36 相同（宿主漏发 SessionStart 或租约损坏，且下一个事件恰好是 Codex 的 PreCompact/Stop）；Cursor 不受影响。`hook_task_signals` 里已有一条断言钉住当前行为，修这条时要一并翻转。


## 2026-09-09 新增（R1-1 边界记录）

38. **Codex fixture profile 与线上宿主版本脱节**：`FIXTURE_PROFILE_VERSION` 与 `fixtures/agents/codex-0.147.json` 仍标记 0.147.0，#34 指纹来自 0.153.2。R1-1 仅在数组末尾追加脱敏后的五键 SessionEnd 指纹，未改变已有下标，也未 bump profile。版本标签仅供参考、不门控能力；完整 profile 更新与版本命名另议，本轮不执行。

## 2026-09-09 新增（R4-2 / KD7 预注册）

39. **usage prior 重新进入排序须另行裁定**：`USAGE_PRIOR_ENABLED = false`。只有判决覆盖率 ≥60% 且 `checkpoint_derived` 强证据样本 ≥100 条，才具备重新讨论的条件；达到门槛不会自动打开。`ignored + session_close` 为弱证据未采用，只参与覆盖率与趋势，不计入强证据标定。观察期间不调整排序或权限面。

## 2026-09-11 新增（R0 语料 join 修复期间发现）

40. **bge-m3 探针未在修复后复测**：R0 把语义语料从 `context_fts`（`normalize_search_text` 输出）切回 `context_revision` 原文，`association_probe_ext_semantic_f2llm` 已三次复跑确认（29/39 lexical、32/39 fused 不变，最差正样本 4127→4553bp），但同一改动同样改变 `association_probe_ext_semantic`（bge-m3 臂）编码的每一条语料，而本机没有 bge-m3 权重、该测试 `#[ignore]` 且无法运行。该文件的断言以自身 lexical control 为基准、外加一条对 `SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS`(5200) 的噪声天花板断言——后者是唯一可能因语料换空间而翻的。`association_probe_ext_semantic_f2llm` 里的 `BGE_CROSS_LINGUAL_HITS`/`BGE_PARAPHRASE_HITS`（2/8）是从那次 2026-09-07 的 bge-m3 运行抄来的常量，也一并未复核。升级条件：任何人手上有 bge-m3 export 时跑一次该 suite；若噪声天花板失守，属于 bge-m3 臂的重标定，不影响默认 F2LLM 路径。

## 2026-09-11 新增（S2-4/S2-4b 双径重建的边界记录）

41. **径 A 反查 miss 时未咨询 relocation 记录**（裁定 4 的跟进项）。ADR-0007 的第一径把 Focus 或 Signal 解析出的 `(repository, path)` 与 index 里的 `engineering_reference` 行做**精确**匹配。此前图谱通道用 `MatchBasis` 解析，因此一个已经改名/移动的文件仍能反查到它的 Context；现在改名即失联——文件叫新名字，Reference 记的是旧名字，两边对不上，包里就什么都没有。这是有意的取舍而不是疏漏：图谱已裁定降级为 relocation 记账，pack 侧自解释实测负价值。跟进方向也因此是现成的——`a_renamed_artifact_is_reported_as_a_relocation_candidate_and_never_resolved` 证明 relocation 候选已经被记下来了，径 A 在精确匹配落空时可以再查一次这份记录，把「旧路径 → 新路径」的映射补上。升级条件：真实会话里观察到因改名而空包的案例，或 relocation 记录的准确率有了实测基线。在此之前，空包是诚实的答案。

42. **`ContextPackMode::Explicit` 是不可达的死代码，随 B1 一并清点**（裁定 5）。全仓唯一的生产 `TaskContextRequest` 构造点是 `crates/mcp/src/lib.rs` 的 `build_task_context_response`，它永远用 `TaskContextRequest::automatic`，因此没有任何 MCP 工具或 CLI 命令能构造出 Explicit 模式的包。ADR-0007 之后这条模式是融合栈（八通道加权 RRF、`AUTOMATIC_RELEVANCE_FLOOR_BASIS_POINTS`、词法覆盖门、`AutomaticQueryTokenExplanation`）在 pack 侧仅剩的入口；那套机器另一个存活理由是 `task_space_associations`，而它的唯一生产消费者是 candidate 分析栈（B1 冻结）。两者要一起看：B1 解冻时，先确认 candidate 侧还需要哪些通道，再把 Explicit 模式与它拖着的常量族一并删掉，而不是分两次拆。本轮只做了 pack 侧退役，没有删这些符号。

43. **同一次 Checkpoint 落下的多条 Claim 互为种子扩展**。Candidate Build 会把来源 Task 的问题附到它产出的每一份 draft 上，所以一次 Checkpoint 里的几条 Claim 拿到的是**同一个** `problem_view`；ADR-0007 的种子扩展正是按 `problem_view` 相等来连边的，于是它们在包里互相带出来——哪怕内容彼此无关。`retrieval_quality_workflow` 的「unrelated sibling」就是这个形状，该测试现在钉的是**路径**而不是缺席：兄弟条目只能以 `seed_expansion` 出现在一条已锚定 Context 旁边，永远不能自己锚一个它并不引用的文件。这是否算噪声取决于一个尚未实测的判断——「同一次 Checkpoint 提交的东西是不是同一件工作」。S2-2 的前提说是（真实装置上 map 会话的三条 Context 共享一个 `problem_view`，正是要一起读的），这个 fixture 说不一定。升级条件：真实流量里统计同 `problem_view` 兄弟条目的被采纳率；若明显偏低，候选修法是让 Build 按 Claim 而不是按 Task 派生 `problem_view`，而不是去动种子扩展。

## 2026-09-11 新增(双径重建 0c 回放发现)

44. **径 A 锚点的分支/时间耦合**:engineering reference 记录的是沉淀时刻的路径,回放/切分支后锚点文件可能在当前 checkout 不存在(实测:FE 会话 2 个锚点文件只存在于 feat/switch-live-tag),径 A 静默 miss。方向与 #41 relocation-aware 反查一致,可合并处理:miss 时先查 relocation 记录,再考虑分支感知。

45. **显式 context_search 成为 wire 大户**:自动包收敛到 ≤8000 tokens 后,单次 context_search 实测 65.8KB(被 Cursor 溢写成文件)。B1 解冻 Explicit 模式清点时,把显式检索纳入与 pack 相同的信封口径与降级链。

46. **回放工具五项(归组)**:turn-timeout 仅在 stdout 有新行时检查(需独立看门狗);两个回放并发会让 --resume 全挂(需互斥);manifest 的模型 fidelity 与 hook payload 实测不符(auto-smart vs 宣称值,应回填);Cursor 子会话(无 sessionStart 的子 conversation)各自获发 activation marker(一次回放=三个被授权 session,授权面值得收紧);pack_deliveries 对"同批 ctx 重复投放"少计(task_injection 去重所致,跨宿主 count=pushes 的承诺不成立)。另:host/turn-N.jsonl 记录完整 MCP result,建议并回 hosts/cursor.py 作为 wire 观测源。**2026-09-12 追加(配对回放实证)**:三项已修(代理透传无记录、turn 失败仍写"完成"的 manifest、worktree detached 静默降级——见 `manifest.environment` / `manifest.run` / `manifest.checkout.branch_mode`);新增一条**结构性缺陷,记档不修**:`cursor-agent --print` 只触发 sessionStart/postToolUse/sessionEnd,五次回放**零 stop、零 beforeSubmitPrompt、零 preCompact**(且 sessionEnd 按 --print 调用次数计,2 turn = 2 次),而真实交互 Cursor 会发(hook-diagnostics 实测 turn_stop 5/6/11)。含义:**Cursor 回放路径结构上无法验证任何 TurnStop 与 prompt 侧行为**——提醒复活、提醒 cap、`## stop` 策略段通道等验收一律必须用 Codex host;Cursor 侧的零是"仪器缺席"而非"功能没跑"。不修的理由:交互式驱动 Cursor 超出无头审计范围,合成 stop 只能证明我们能调自己的 hook。能力矩阵见 `tests/scripts/session_replay/README.md` 顶部。升级条件:Cursor 给出无头的 turn 边界事件,或出现只能在 Cursor 复现的 TurnStop 缺陷。

## 2026-09-12 新增(WP-G2a 供给侧清扫发现)

47. **`hook_fail_open::cursor_undecodable_payload_shapes_fail_open_with_a_neutral_output` 是既有 flake**：断言"三个不可解码 payload 各留一行 `payload_decode_failed`"，并发跑 5 次里挂 1–2 次，单独跑必过；**在本分支改动之前的 HEAD 上同样复现**（`git stash` 后连跑 5 次挂 1 次），与 WP-G2a 无关。形状与 #30 同族但不是墙钟断言，而是共享的有界诊断视图计数——并发测试写入会把目标行挤出窗口。修法方向：该断言按 reason 过滤后断言 `>= 3` 或让 harness 用独占的诊断视图。

48. **pending prompt 让"未成 Task 的会话文本"首次落盘**：`pending_prompt_signal` 存的是与 Signal 完全相同的脱敏截断文本，但一个**永远不会声明 Task** 的会话，其 prompt 现在也会在磁盘上存在（此前完全不落盘）。已用两道闸收口：48 小时过期 + 全表 256 行上限，两者都在每次 stash 时清扫。代价记录在案：`codex-normal.json` 的 `raw_content_is_absent` 探针去掉了 `{"kind":"prompt","hook":"prompt"}` 一项——那条封闭断言此前之所以成立，正是因为"建 Task 前的 prompt 被丢弃"这个缺陷；prompt 侧的隐私性质改由 `hook_task_signals` 的两条 secret 断言覆盖（暂存的和记录的都断言 `PROMPT_SECRET` 不出现在持久化状态里），比 canary 探针更强，因为 canary 本就不是密钥形态。升级条件：若要恢复该探针，需要一个"raw 内容确实永不落盘"的 prompt 形态（如斜杠命令）并让 runner 支持给 canary 加前缀。

49. **符号点名派生尚无真实流量基线**：`derived_from_symbol_mention` 的准确率只有 fixture 证据（276,848 文件里 `MapSceneRuntime`/`CameraController` 各唯一匹配是离线核过的，但没有跑过一次真实 Build）。每 claim 上限 3、只在零字面路径派生时开闸、每条都带 limitation，三道控噪都在，但"点名派生占最终 accepted Reference 的比例与其被采纳率"要等真实流量。升级条件：攒到 ≥20 条 `derived_from_symbol_mention` 的 accepted Reference 后统计一次；若误指率明显，先收紧到"仅当仓内该 stem 的文件扩展名属于代码类"。

## 2026-09-12 新增(WP-G2b 清扫期间复核)

50. **`estimated_tokens` 仍不含响应本体的 `active_signals`**(S2-5 口径下复核,结论不变)。`charged_task_context_tokens`(`crates/search/src/lib.rs`)计的是 `(associations, items, graph_diagnostics, omitted, query_token_explanation)` 五元组加 `TASK_CONTEXT_ENVELOPE_TOKEN_RESERVE`;`active_signals` 是 `TaskIntentUpdateResponse` / `CompactTaskIntentUpdateResponse` 的字段,由 `crates/mcp/src/lib.rs` 的响应装配追加,**不在**那个五元组里。实测(cursor 2d5ab8ea)空 Pack 报 189 token 而响应正文 5808 字节。载荷有界(`MAX_ACTIVE_FILE_SIGNALS` 16 + `MAX_ACTIVE_PROMPT_SIGNALS` 8 = 至多 24 条记录),所以这不是无界泄漏,但按 caliber v2 双份计,它能吃掉 `PACK_WIRE_TOKEN_CEILING`(8000)对 10000 cell 留的那 2000 余量的相当一部分。**本轮只复核不修**:诚实计费要求信号载荷进入 Pack 的降级链,也就是 budget 语义改成"检索载荷 + 响应本体",这是设计裁定不是 bug 修复。与之相对,顶层 `retrieval_paths` 重复副本是同族问题的另一半,它没有消费者、是逐字节重复,已在本轮直接删除而不是补计费。升级条件:真实会话里观察到 `active_signals` 参与一次宿主截断,或 budget 语义获得裁定。

51. **candidate 分析不再读图谱安全位,`ContextSafetySource::EngineeringGraphSnapshot` 的清点面随之扩大**。WP-G2b 的 ⑥a 删掉了 `candidate.rs` 里 `state.safe |= graph_context.safety.automatic_injection_eligible` 这一处 `|=`(它映射的是与 domain 投影相同的三个 Git 侧 blocker,对 supersession 与 `[context_ttl]` 一样瞎,只能把权威读数抬高)。S2-4b 已记 pack 侧失去生产者;现在 candidate 侧也不再消费,于是 `crates/engineering-graph/src/resolver.rs` 的 `graph_context_safety` 与 `crates/search/src/lib.rs` 里那个 `ContextSafetySource::EngineeringGraphSnapshot` 构造点的唯一去路是**不可达的 Explicit 模式**(#42)。B1 解冻清点 Explicit 时应当把这三处一起看,而不是只删 Explicit 入口。

52. **`EmbeddingProvider::encode` 与 onnx 的交互优先级闸只剩一个生产调用者**。ADR-0007 修正案退役查询侧之后,`encode`(带 instruction 前缀的查询编码)在生产里只被 `sctx embedding status --verify` 的自检调用一次;`SessionGate` 的抢占逻辑(`INTERACTIVE_QUIET_PERIOD` / `MAX_BULK_DEFERRAL` / `BULK_PROGRESS_DEADLINE`)因此保护的是一条几乎不来的交互流。**有意保留**:它是每一套标定与探针套件重新测量的前提——删掉编码查询的能力就等于删掉将来复核这些裁定的能力,P3(F2LLM vs arctic 同空间重跑)正需要它。升级条件:若 P3 结束后不再有同空间重跑需求,可评估把闸简化为一把普通互斥锁;在此之前它是"为可测量性保留"的已声明例外,不是遗漏。

## 2026-09-12 新增(WP-G3 真模型套件重基线)

53. **对照臂 `association_probe_ext_semantic` 没有数值棘轮,只有"正负样本不重叠"**。它原先钉的是 bge-m3 在 `SEMANTIC_CORPUS_VERSION` `"1"` 语料空间上的读数(noise 天花板 5095、最弱正样本 5640、断言线 5200);语料口径 2026-09-11 改为 `"2"` 之后这些数字属于一个不再存在的空间,而本机没有 bge-m3 权重可以重测(装机默认导出已是 F2LLM)。把旧数字当棘轮留着就是钉一个没人量过的空间,所以这一臂改成"任意导出的文档空间分离度表 + 只断言 margin > 0",数值棘轮只留在默认导出那一臂。**升级条件**:P3 真的下载 bge-m3 或 arctic 权重跑一次选型对照时,顺手把该导出的四个读数(worst positive / best noise / cross_lingual 与 paraphrase 类内最低)按三次一致定数写进这一臂,它就重新有棘轮了。命令与指标口径见 `DEVELOPMENT.md`"真模型套件手动清单"。

54. **`--ignored` 真模型套件没有任何"多久没跑过"的提示机制**。G3 发现的过时不是某条断言写错,而是**四个月没人跑**:门禁只跑非 ignored,于是 ADR-0007 退役查询侧、S2-4 换掉自动注入之后,两套语义探针的全部断言在无人察觉的情况下指向了两条不存在的路径(词法对照实测从 27/39 跌到 5/39 才暴露)。清单与触发条件现在写在 `DEVELOPMENT.md`,但那仍然是"人记得看文档"级别的保障。**升级条件**:若这类失效再发生一次,考虑机械化——例如让改动 `crates/search/src/embedding*`、`lanes.rs` 或 `embeddable_revisions` 的提交在 pre-commit 里打印本清单,或把各套件最近一次实测日期写进常量 doc 并由一个非 ignored 测试断言它不早于某个源文件的 mtime(后者跨 checkout 不可靠,需先想清楚)。

55. **`DEVELOPMENT.md`「当前实现边界」里的 embedding 段落整体停留在 T5b/ADR-0004**。有五条相邻 bullet 描述的是已经不存在的东西:查询侧 automatic 接线与 `SEMANTIC_ENCODE_BUDGET` 超时降级、`EmbeddingProvider::similarity_floor_basis_points` 与两个 floor 常量、`SEMANTIC_CHANNEL_LIMIT = 16`、`SEMANTIC_TEXT_CHANNEL_WEIGHT` 与 `FUSION_CHANNEL_WEIGHT` 分母取舍、"语义命中 ≥ 阈值构成一条独立注入资格"以及 `retrieval_paths[].source = "semantic_similarity"`。这些在 ADR-0007 修正案(查询侧整条退役)与 S2-4(融合层拆除、自动注入改两径)之后全部作废,`user-guide.md` 已按新架构改过而 `DEVELOPMENT.md` 没有。**本轮只记不改**:G3 的范围是 `--ignored` 真模型套件,而这五条是"当前实现边界"这一节的架构陈述,重写它应当与 ADR-0007/S2-4 的文档面一起做,顺手改容易漏掉彼此矛盾的半句。已修的只有三处失效的 rustdoc 链接(`SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS`、`SEMANTIC_CHANNEL_LIMIT`、`QWEN3_SEMANTIC_SIMILARITY_FLOOR_BASIS_POINTS` 指向已删常量)。

## 2026-09-12 新增(配对回放实证)

56. **~~novel 结构性不可达~~ 已撤回,第二档不再是死条款**:`docs/refactor-r1-r4/candidate-noise-plan.md` 硬发现 ① 断言"任何持续使用的装置上 novel 永不判给,第二档自动确认是死条款"。本轮实测 `candidate_get` 返回 `relation: "novel"` / `paths: [no_sufficient_candidate]` / 6000bp 推翻它;而且它当时就与自己的审计计数(207 行里 1 条 novel)和 `tests/scripts/real_host_smoke_pair.py` 的必过断言(`candidate_relations == [["novel"]]`)矛盾。漏在假设评估集不变:G2b(`9cfc7fe`)之后 `current_revision_ids` 只收治理 revision,`bm25`/`graph`/`explicit` 三个外部定序通道按精确 `ContextRevisionRef` 挂载、命中落在已不在表里的 revision 上被静默丢弃,而 BM25 最宽且搜索已退休状态。**已改**:机制与撤回写进 `crates/search/src/candidate.rs::novel_assessment` 的文档注释与该方案文件 1.2 节。第二档窄的真原因是硬发现 ②——novel 分析没有 target,拿不到 `safe_strong_target`,只能靠已解析的 `proposed_space_group_space_id` 才能到 `ready_for_review`。**升级条件**:若"提议 Space 组落地率"被测出来接近零,第二档才真的近似死条款,那时值得重新裁定 D4。

57. **#30 的并发假失败里多了一种非墙钟签名**:#30 已记 `engineering_workflows::public_mcp_artifact_focus_...` 在高负载下偶发,但当时的形态是 latency 断言。本轮 workspace 全量(1137 passed / 3 failed)里它挂在 `assert_eq!(rebuilt.status_counts.resolved, cases.len())` 上,实测 8/9——**少解析了一条 Engineering Reference,不是慢,是结果不同**。同批还有 `scenario-runner::runner_contract::matching_typed_failure_...` 与 `concurrent_fault_...`(后者确实是 `elapsed() < 2s` 的墙钟断言,#30 原样)。三者单独重跑与在基线 commit 上整套重跑均通过,本轮改动与工程图/scenario 无交集。**升级条件**:8/9 这个签名再次出现,即优先于其余 #30 条目排查——查 `association_rebuild` 的仓库扫描是否对 mtime 或目录枚举顺序有隐含依赖(那会是真缺陷,不是 flake)。
