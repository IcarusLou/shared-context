# Authoritative refactor plan (2026-09-09)

Current amendment: R2-1 follows the user-approved [H-001 Disposition](r2-1-compatibility-gate.md#disposition-2026-09-09-reviewed-and-ruled--h-001-closed). Empty-required wire DTO and tombstone writes are approved; a LegacyClaimFields carrier is rejected. The original transcription below remains historical authority for all other scope.

Source: /private/tmp/claude-501/-Users-bytedance-workspace-shared-context/25a5e2fe-a1ea-4361-a744-a650de089725/scratchpad/refactor-execution-plan.html

SHA256: 2ec8c20d0826ac7c32eaeeeb77c8d569dbba2eac0fc8f1b1f58ef93b610c8e1a

Shared Context · 执行方案


重构执行方案 R1–R4


2026-09-09 · 依据:全链路重构裁定(A 层批准 / B6 先收数 / #34 接管)· 执行模式:每个 Milestone = 实现 agent 按本方案落地 + 我逐 diff review、独立复跑测试、按 hunk 拆分提交 · 全部动作的对抗验证原文在会话 scratchpad/audit-result.json,执行时按 move id 查阅完整 rescope




四个里程碑,严格顺序推进:M1 缺陷修复(召回排序两处 + SessionEnd 接管)→ M2 减法(死代码/死字段/刷屏/源头措辞)→ M3 同源与地基(文本单源 + NeedsEvidence + 审计迁移)→ M4 北极星度量(注入判决全覆盖,然后运行收数 ≥2 周)。B 层设计与 ADR-0005 修订以 M4 的数据为入场券,不在本方案内。


七个关键决策(KD1–KD7)已给推荐,无异议即按推荐执行。每个原子项都标注了"明确不做"边界——这些边界来自对抗验证,越过即踩已证实的硬伤。






M1 · 缺陷修复(WP-R1)


性质:三个已实证缺陷,修复即净收益。M1 完成即提交,不等后续里程碑。



R1-1 · Codex SessionEnd 按真根因修复(A3,含 #34 接管)


根因真实指纹 keys=[cwd,hook_event_name,reason,session_id,transcript_path] 不含 model,而 Common.model: String 必填——serde 在 reason 白名单检查之前就以 missing field 失败。并行会话工作区里的修复只放宽了 reason,修不好。


改动四件一套,缺一不可:① crates/adapter-codex/src/lib.rs 的 Common.model 改 Option<String>,非空校验移到真正携带它的五个事件(SessionStart/Prompt/PostTool/PreCompact/Stop 校验 Some 非空),SessionEnd 允许 None;② require_one_of("Codex SessionEnd reason", …, ["other"]) → require_nonempty;③ fixture:向 fixtures/agents/codex-0.147.json 只追加(payload_contract 按下标 remove(0/1/2/5) 寻址,插中间会静默改写 5 处测试)一条无 model 的真实指纹 SessionEnd,payload_contract 加解码用例;④ 同步 adapter 严格解码的两处文档契约(模块文档与 decode_hook_input 的 "Rejects … undocumented enum values"),写明 SessionEnd 的放宽依据(#34 指纹)。


接管机制见 KD1。完成后更新 docs/deferred-issues.md #34 为已修(注明真根因与并行修复的偏差)。


验收用真实指纹 payload 打 sctx hook --agent codex 不再 fail_open;遥测出现 hook.codex.session_end 事件;既有含 model 的 SessionEnd 测试全部保持通过;租约清理(CleanupSessionState)在 SessionEnd 路径可达——加一条 cli 级断言。


明确不做不 bump FIXTURE_PROFILE_VERSION(0.147 与 0.153 的脱节记入 deferred,单独议);不动 SubagentStart/Stop 等未实现事件。





R1-2 · 修 RRF rank 摧毁缺陷(A1)


根因crates/search/src/candidate.rs:615 add_ranked_channel 对 Vec<ContextRevisionRef> 先 sort()(按 context_id 字典序)再赋 rank——bm25/graph 两条携带相关性序的通道被摧毁,其余五条本就无序不受损。


改动调用方传入已按相关性排序的序列;add_ranked_channel 只做去重(保留首次出现位次),删除内部 sort();explicit 通道保持 sort+dedup(本就是无序集合)。


验收新增单测:乱序输入 + 重复项 → rank 反映输入序且首现位次保留;bm25 通道端到端断言(构造两个候选,BM25 高分者获得更小 rank);全量 cargo test -p sctx-search;用真机 34 条候选重放对比 relation 分布变化并记录(预期 supports/pc 命中率可能上升——记录进提交信息,不做阈值断言)。


明确不做不动七级梯子本身、不动任何阈值常量(那是 B1)。





R1-3 · DF=0 伪 token 不再挤占查询预算(A2)


根因crates/search/src/lib.rs:2416-2422 rarest-first 排序让 DF=0 的 token(Han 跨词边界伪 bigram 是最大来源,实测 78 中 60 个)排最前,优先占满 MAX_AUTOMATIC_QUERY_TOKENS=64,把 DF>0 真词挤出截断。DF=0 对 BM25 匹配是纯 no-op。


改动按 KD2(推荐保守变体):排序键改为"DF=0 者排最后,其余仍 rarest-first",截断照旧——真词优先存活。不动 tokenizer、不动索引、不 bump 任何 version。


验收两套探针棘轮(association_probe_workflow / association_probe_ext_semantic_f2llm)按仓库惯例三次一致实测:不回退;若提升,按先例更新棘轮数字并在提交信息记录三次读数。同时复核 answerable/selected 比值门(AUTOMATIC_MIN_ANSWERABLE_RATIO_BASIS_POINTS=2500)在真机回放语料上的触发次数变化,写进提交信息。


明确不做不从 selected 集合中剔除 DF=0(那是激进变体,留待 M4 数据后与 B5 一起议);不改 automatic_short_token 的非 ASCII 规则。





M2 · 减法(WP-R2)


性质:纯删减 + 一个刷屏修复 + 源头措辞。每项独立可回退,合并为一个 work package、分逻辑提交。



R2-1 · Claim 死字段内部清理(A4a)


改动只在 Claim 层删五个恒空字段(assumptions/recheck_when/artifact_refs/relations/related_contexts):CheckpointClaim(domain/episode.rs:566-660 含 from_parts+validate)、CheckpointClaimDraft(task-runtime:188-203)、direct→fat 转换(task-runtime:5267-5310)、checkpoint_semantic_json 死键(task-runtime:5187-5216)、candidate_artifact_refs 的 claim 半边(mcp:5053-5065,保留 engineering_reference 半边)。


明确不做(硬边界)不碰 schemas/event-v1.schema.json(byte-frozen,git_writer.rs:228/280 与 installer_matrix.rs:2666/2688 逐字节断言,且 installer 会重写);不碰 ContextRevision/Draft(deny_unknown_fields + 非 Option 字段,删了 Git 历史回放硬失败);不碰 Context 修订层的 recheck_when(活机制:4 条非空、2 条 stale 生效中)。


验收build_claim_material 路径核查无这五字段消费;task-runtime/mcp/domain 全量测试;git 事件回放测试(既有)不受影响。





R2-2 · 确证死代码清扫(A5 放行子集)


改动执行 agent 先读 audit-result.json 中 M5 rescope 全文,只做放行清单:① ResolvedTaskOperation.additional_context 死字段(cli:2463,三处构造点全 None,删字段并简化 :2456 的 .or());② hook_event 写端 API 与类型(task-runtime:4128/4145/4201/4235 及常量——HEAD 无生产写入者,doctor 已读遥测);③ rescope 放行的其余项逐条核对后执行。表本身不 DROP(留给 M3 的迁移批次或 B 层,只删代码)。


明确不做(硬边界)search_pages 保留(search_contract.rs:608 唯一钉住事务内翻页交叉验证);[hooks]/[context_ttl] 配置字段保留(LocalConfigDocument deny_unknown_fields,删了用户手写 config 解析失败且在 hook 热路径上);ADR-0006 冲突的 (h) 项撤回;space_intent_candidates 公开包装保留(内层活通道的唯一测试入口)。





R2-3 · Closed 分支去重刷屏(A8)


改动三点一体,缺一即回旋镖:① cli Closed{newly_closed:false} 分支不再重发 "durably closed" 文案,例外:本次重放把 CandidateBuild 推到 Complete/Incomplete 时仍报一次(Builder 恢复回执);② 同批删除 agent-adapter:728-731 的静态兜底 system_message——它今天恒被 finalize 的 Some 覆盖,是死代码;若 ① 返回 None 而不删它,兜底文案会顶上来,刷屏换个文本继续(对抗验证抓出的回旋镖);实现上用显式静默(空 Some 或三态)以避免 .or() 语义歧义,取其一并写清注释;③ 分支本体(should_build 重建 + recover_one_pending_episode_build)一行不动。


验收episode_lifecycle_hooks.rs:431-442 改为"首次含 durably closed,其后 N 次静默且 Candidate 数不变";Cursor TurnStop 首次通知的模型可见语义(user_message)保持(episode_lifecycle_hooks.rs:405-411)。





R2-4 · SpaceAdvisory 搁置(A7)


改动删 provisional_space_advisories(mcp:447-521)、MAX_SPACE_ADVISORIES、PROVISIONAL_SPACE_MERGE_THRESHOLD(mcp:401)、两个 Response 的 space_advisories 字段(带 skip_serializing_if,输出未冻结,删除安全)。


明确不做maintain 的 provisional_space_survey 步骤与 digest 计数保留(digest 契约有消费者);Space Intent 修订事件模型不动。





R2-5 · provisional 谓词收敛(A6)


改动domain 新增 space_is_provisional()(口径:唯一 current head 且 provisional),替换 mcp:421 / mcp:6965 / cli:3096 三处;index/schema.rs:780 改调同一函数。零行为变更,加一条四处一致性单测。





R2-6 · workflow §1 噪声优先改版(N1)


改动按候选处置降噪方案已拟好的三处原文落地(成本不对称句补全、三问自检、not-worth-keeping 补"常规编译通过/复述单函数行为"两条);byte oracle(fixtures/m5 workflow_bytes)与 user-guide 字节数照例 resync;全仓 rg 确认无处断言旧句。


验收mcp_contract 的 workflow 长度上限(21000)与内容断言、repository_scoped_context_acceptance 全过。





M3 · 同源与地基(WP-R3)



R3-1 · 三档与 worth-keeping 文本同源化(A9)


改动① mcp 内新增一个三档 triage 文本常量,candidate_list 描述(:8177)与 ACK notice(:9072-9086)共同派生 + 等值断言;② 修复 worth-keeping 实际失同步:给 candidate_list 描述与 ACK notice 补回丢失的 "counter-intuitive finding" 与 "用户纠正" 两条判据(直接影响第二档自动确认口径);③ workflow.md §7 加 contains 断言,断言对象是既有的 "This section is the one place the three disposition tiers are defined" 权威句。


预算见 KD4:补回两条约 +100~150 字节,现余量 203 字节——若超出,在 candidate_list 描述内等量精简措辞,不动其它工具的描述;重生成 golden 用 SCTX_UPDATE_TOOL_SURFACE_GOLDEN=1,提交信息记录字节前后值。


明确不做(硬边界)不动 byte-frozen 的 decision_source 描述;不把描述降为指针(Skill-less host 与压缩后的唯一耐久通道);不动 fixtures/m2/m5 的 marker 模板(已被生产函数钉死,占位符技术上不可由生产函数生成,且牵动 token 预算会计);不倒转 workflow.md 的定义权威。





R3-2 · NeedsEvidence 死角修复(A10)


改动candidate.rs:1296 的 confidence.basis_points < 5_000(以 relation 强度冒充证据强度)改为真缺证据判据:存在 blocking unknowns 或分析 Failed——与 mcp:2968-2971、mcp:5472 两处既有语义源对齐,只保留这两条。


验收review_status 单测:unresolved@4000 且证据完备 → 不落 NeedsEvidence;blocking unknown → 落;mcp:3376 对 Draft|NeedsEvidence 的硬拒语义不变。顺手把 M6a 残余并入:权限面闸 1 收窄为 review_status != Pending 的 replay 检查,同步 mcp_contract.rs:9224-9226 的子串断言(对抗验证给出的等价改写,净删一个冗余合取)。





R3-3 · 审计迁移 18→19(N4 + B6 地基,一次迁移)


改动按 KD5:单次 migration 加齐两件事的列——① candidate_review.top_relation TEXT(analysis 完成时写入,循 decision_source 列的迁移测试范式);② context_usage 增 basis TEXT(取值 checkpoint_derived | session_close,既有行回填 checkpoint_derived)。③ sctx candidate stats 增加 relation × decision_source 分组输出;cli_contract 断言同步。


明确不做proximity_only 列不加(随 B1 定稿);不做任何清库式迁移(14→15、15→16 两次清空的教训已记档)。





M4 · 北极星度量(WP-R4 / B6)



R4-1 · 注入判决全覆盖


改动新增会话尾判决写入点:SessionEnd hook(R1-1 之后 Codex 亦可达)对该会话各 Task 中尚无判决的 task_injection 写保守判决——Task 曾 checkpoint 且 claim 引用 → 既有路径已写 reused/ignored(basis=checkpoint_derived,不重写);Task 从未 checkpoint → outcome=ignored + basis=session_close(口径见 KD6)。写入走 hook 既有的 fail-open 纪律与短 busy timeout,失败静默。


验收cli 集成测试:注入后不 checkpoint 直接 SessionEnd → usage 行出现且 basis 正确;已 checkpoint 的 task 不被重写;cursor/codex 两 host 各一条用例。





R4-2 · usage prior 显式停用 + 读端


改动① 排序中的 usage prior(ignored 惩罚阈值 3)在覆盖率达标前显式停用:加常量开关 + 注释写明激活条件(见 KD7),当前置 off——它 5 天来从未满足阈值,是未激活通路,摘除即减一条隐性排序影响;② 新增只读 sctx recall stats:注入总数、判决覆盖率、outcome × basis 分布、按 task/session 聚合。


收数运行M4 落地后正常使用 ≥2 周或 ≥20 个会话,期间不动排序与权限面;届时用 recall stats + candidate stats(relation 维度)产出 B 层设计的入场数据。





关键决策 KD1–KD7



KD1 · #34 接管机制


工作区里并行会话的 adapter-codex diff 含两部分:reason 放宽 + 其配套测试(方向对、不充分),以及 HookDecodeErrorClass 错误分类枚举(~130 行,疑似其遥测工作的一部分,可能有其 cli/telemetry 侧消费者)。推荐:R1-1 基于工作区现状实现(吸收 reason 放宽与其测试,叠加 model 修复),提交时按 hunk 只提交 SessionEnd 修复相关改动;HookDecodeErrorClass 若与 decode 路径纠缠不可拆,则停下向你报告,由你通知并行会话先提交或撤出它的 adapter-codex 改动。执行前先 git diff crates/adapter-codex/ 全文核对纠缠度。





KD2 · A2 的保守 vs 激进变体


激进(从 selected 剔除 DF=0)会抬高 answerable/selected 比值,必须连 2500bp 门一起重扫两套探针。推荐保守变体(排到末尾),探针棘轮做验收但不重定门槛;激进变体并入 B5(floor 语义重定义)一起议。





KD3 · A3 的 model 字段形态


两案:per-event Common 拆分 vs Option<String> + 按事件校验。推荐后者:改动面最小、五个真携带事件的严格性不变、SessionEnd 单独放行;per-event 拆分留给 B 层若 adapter 还要大改。





KD4 · A9 的字节预算取舍


补回 worth-keeping 两条判据约 +100~150 字节,余量 203。推荐:优先在 candidate_list 描述内部等量精简(它 1123 字节是最大项,有压缩空间);绝不动其它 16 个工具与冻结描述;若最终仍超,削三档常量派生文本的冗词而不是砍判据条目。





KD5 · 迁移合批


N4 与 B6 各需加列。推荐单次 18→19 在 M3 加齐(top_relation + usage basis),M4 只写数据不再迁移——少一次 schema 变更,B6 落地时地基已在。





KD6 · 会话尾判决口径


"没 checkpoint 一律 ignored"会高估无用功——这正是要防的。推荐:outcome 枚举不扩(消费者稳定),用 basis 列区分证据强度;所有统计口径把 ignored+session_close 单列为"弱证据未采用",与 checkpoint_derived 的 ignored 分开呈现,B 层标定只用强证据段,弱证据段仅作覆盖率与趋势。refuted 保持只从 checkpoint 显式产生。





KD7 · prior 重新激活条件


推荐预注册:判决覆盖率 ≥60% 且强证据(checkpoint_derived)样本 ≥100 条时,才重议 prior 入排序;写进常量注释与 deferred-issues,防止悄悄打开。





风险与回滚



风险缓解 / 回滚


R1-2/R1-3 改变召回排序行为探针棘轮三次一致实测做闸;真机 34 候选重放留档;单提交可 revert


并行会话继续改 adapter-codexKD1 的纠缠检查前置;R1-1 尽早提交使其 diff 基线前移;冲突即停手上报


M2 删减误伤隐藏消费者每项都过了对抗验证的放行清单;执行 agent 必读 audit-result.json 对应 rescope 全文;逐项独立提交


迁移 18→19additive 列 + 既有迁移测试范式;禁清库;迁移单独提交先行


SessionEnd 打开后判决写入拖慢 hook沿用 fail-open + 短 busy timeout;hook_hot_path 预算测试作闸


N1/A9 措辞改变 agent 行为方向(欠沉淀回潮)M4 的 recall/candidate stats 即为观察面;连续走低即回调措辞(纯文档 revert)






验收总表与提交纪律



每个 Milestone 完成:cargo fmt / clippy --workspace --all-targets 零告警;受影响 crate 全量测试;涉及 oracle/golden 的按各自机制重生成并在提交信息记录前后值。


提交:沿用本分支惯例(type(scope) + 讲清 why 的正文 + 实证引用);外来未提交改动(log-sync 等)全程不碰、不 fmt、提交前 diff 清点。


M1 与 M2 之间、M3 与 M4 之间各跑一次真实会话冒烟(cursor 或 codex 任一),对照本方案验收点。


M4 收数期结束的产出物:一份 B 层入场数据报告(relation 分布、agent_policy 尝试/拒绝、判决覆盖与强证据 reuse 率),作为 ADR-0005 修订与 B1–B5 各自设计文档的输入。




上游文档:全链路重构裁定(A/B/C 分层与保留清单)· 候选处置降噪方案(N1/N4 原文)· ADR-0003/0005/0006 · 对抗验证原文 scratchpad/audit-result.json。B 层(B1 梯子收敛 / B2 Episode 折叠 / B3 权限面+ADR-0005 / B4 space_group 换 Git 派生 / B5 floor 重定义)各自单独出设计文档,入场券为 M4 数据。
