Shared Context · 设计方案


候选处置降噪方案


2026-09-08 · 基于 cursor 会话 7d8cbab1 的 7 条真实 candidate 与 relation 梯子代码实证(crates/search/src/candidate.rs @ d819aa7)· 待裁定




定调(你的原则,方案的第一公理):丢失可接受,噪声不可接受。噪声一旦进入 accepted 库,检索会把它排在真实事实旁边,直接稀释每一次召回;而漏记的结论下次要用时重新 checkpoint 即可。


推论:自动化的扩展方向是"收紧入口 + 扩大自动弃置",而不是放宽自动确认。本方案分两阶段:阶段 1 四个工作项(源头过滤改版、三档措辞补全、纯邻近标注、审计地基)全部零召回污染风险,可立即开工;阶段 2(把纯邻近纳入自动确认)只留判据,达标后再裁定。


两个硬发现改变了问题形状:① novel 在真实装置里结构性不可达,第二档自动确认本来就是死字母(**2026-09-12 撤回,见 1.2**);② 即使放宽 relation,7/7 条卡在 needs_space_review 第二道闸,只动 relation 的方案实效为零(仍然成立,且它才是第二档窄的真原因)。






1. 诊断:为什么自动化收益是零



1.1 relation 梯子与实测


relation 判定是七级短路梯子,第一个命中即定。关键常量:statement 相似度强阈值 8000bp、复核阈值 5000bp、同类标识符强重叠 ≥2。全部不中则落入兜底:



#触发条件(摘要)relationbp


1相似度 ≥8000 且否定词集合不等potential_contradiction6500


2整份规范相等 / 同 topic 同句 / 近重复(≥5000)exact_duplicate9500–10000


3statement 相似度 ≥8000 无否定冲突supports9000


4topic 相等 + explicit/graph 链接revises8500


5同 kind 共享标识符 ≥2(±复述)supports / potential_contradiction9000 / 6500


6topic 冲突 / 共享 Artifact + ≥5000potential_contradiction6500


7兜底unresolved_related4000





7d8cbab1 的实测:7 条 candidate 共 102 个 assessment,100% 落在兜底,confidence 100% 为 4000,trigger 100% 是 "Path: retrieval proximity only, statement similarity …"(实测相似度 574–1538bp,离任何强阈值都差一个量级)。零 topic_equality、零同 kind 标识符重叠(8 次共享标识符全部跨 kind,对 relation 无影响)。



1.2 硬发现 ①:novel 结构性不可达 —— **2026-09-12 撤回**


novel 只在八个检索通道全空时判给。但 scope 通道的命中条件仅是 domains 有交集,而每条 Claim 的 domains 从 Task Working Intent 继承(APPLICABILITY_INHERITED = true)——只要知识库里存在任何一条同 domain 的 Context,novel 就不可达。实测 candidate 的"邻居"来源全部是 scope_overlap(search/frontend) + 单 token 全文命中(BM25 查询只取 statement 中最长的一个 token,实测恒为 ["programmed"] 或 ["position"])。


结论:在任何持续使用的装置上,第二档"novel 可自动确认"是永远不会触发的死条款,supports(要求相似度 ≥8000,近乎复述)是唯一活路——三档设计从未真正在聚焦语料上运转过。

**撤回(2026-09-12,配对回放实证)**:上面这条结论错了,novel 可达。三条证据:(a) 本轮实测 `candidate_get` 返回 `relation: "novel"`、`paths: [no_sufficient_candidate]`、6000bp;(b) 本文自己的审计计数里就有一条(207 行里 1 条 novel,走同一条 `no_sufficient_candidate`)——当时把它当成"极稀有"而非反例;(c) `tests/scripts/real_host_smoke_pair.py` 把 `candidate_relations == [["novel"]]` 当作配对冒烟的**通过判据**,结构性不可达的关系不可能是必过断言。

论证漏在哪:它假设评估集不变,但评估集在 G2b(`9cfc7fe`)之后缩小了。`current_revision_ids` 现在只收每个 Context 的治理 revision,而此前revision DAG 的每个节点各自成为一个 target;`bm25` / `graph` / `explicit` 三个外部定序通道按精确 `ContextRevisionRef` 查表挂载,**命中落在已不在表里的 revision 上就被静默丢弃**——BM25 是其中最宽的一路且搜索已退休状态,所以"只命中旧措辞"的全文命中现在直接蒸发。`scope` 通道没被改动,所以原论证的机制仍然真实,只是并不穷尽:一条继承 domains 与库内无交集的 Claim 依然会落到零邻居。

对第二档的影响:第二档**不再是死条款**,但它窄的原因换成了硬发现 ②。`review_status` 只在有 `Existing { role: Primary }` 推荐时才给 `ready_for_review`,Primary 选举要求 `safe_strong_target`,而 novel 分析没有 target、拿不到这个位,只能靠已解析的 `proposed_space_group_space_id`。所以 **novel 候选进第二档的条件是"来源 Task 的提议 Space 组已经落到一个真实 Space"**。机制注释见 `crates/search/src/candidate.rs::novel_assessment`。



1.3 硬发现 ②:第二道闸 needs_space_review


服务端自动确认权限面(require_auto_confirm_permitted)有四条,按序短路:① Review ready_for_review;② candidate_status == ready_for_review;③ 无 edits;④ relation ∈ {novel, supports}。实测 7/7 条的 candidate_status 是 needs_space_review(Space 推荐均为 proposed_new_space_intent 临时空间)——即使把 relation 集合放开,第 ② 条也会全部拒掉。任何只动 relation 的方案对这批真实数据的通过率为 0。



1.4 源头噪声与第一档闲置


7 条中 2 条(29%)是 §1 明文排除的 process-level 内容(如"getBadgePosition 应先尊重显式 badgePosition"——直接可从代码读出),仍进了 checkpoint,最终由用户口述、agent 代录 reason 弃置。第一档"process-level 可自行弃置"明文不要求任何 relation,且服务端对 discard 完全没有权限面(decision_source 在 discard 上是纯 provenance 标签)——这个已授权、零风险的能力,实际使用次数为 0。



2. 方案:两阶段五工作项


阶段 1 的四项互相独立、全部不放宽任何确认权限,噪声风险为零或负(净降噪)。阶段 2 只预注册判据。



WP-N1 · 源头过滤改版(workflow.md §1)阶段 1


参照 Claude 自身 Memory 机制的提示词结构改版,把"噪声威胁召回"立为第一原则。三处具体改动(拟议原文,可直接裁定):



改动 1 · 成本不对称句补全(现有句保留,追加)


Leaving a row out costs the knowledge base nothing. A row that only says you read a file costs every later reader.


Leaving a row out costs the knowledge base nothing: a lost conclusion is re-checkpointed the next time it matters. A low-value row costs retrieval itself — it ranks beside the real facts and dilutes every later query, and it keeps costing until someone withdraws it. When unsure, leave it out.






改动 2 · 三问自检(§1 末尾新增短段,对应 Memory 机制的 "ask what was non-obvious")


Before submitting a Claim, ask three questions. Would this change what the next person does? Would it still hold, with its conditions stated, after this diff is merged and forgotten? Could a later reader re-derive it from the code, the diff, or the PR in under a minute — and if they could, what is the one non-obvious part worth keeping instead? A Checkpoint that survives all three is usually one to three Claims; needing more than five in one call is a sign the filter did not run.






改动 3 · Not-worth-keeping 清单补两条(实证反例)


- a routine build, compile, or test success; a validation is worth keeping only when its result is counter-intuitive or constrains someone else;- restating what a single function or component visibly does, however precisely — that is the code's own job.





预算核对:workflow.md 现为 18602 字节,mcp_contract 上限 21000,余量约 2400 字节,足够;byte oracle(fixtures/m5)照例 resync。风险见 §3。



WP-N2 · 三档措辞补全(§7 + review.md)阶段 1


unresolved_related 目前在三档里未被任何一档点名,按 "everything else" 全落第三档——这正是 7/7 人工的机制成因。补全(依赖 WP-N3 的标注字段):



第一档扩句:纯邻近(proximity_only)的 unresolved_related 行,若其内容过不了 §1 过滤器(process-level、常规验证、复述代码行为),自行弃置,reason 点名依据。强调:第一档的 process-level 分支从来不看 relation——这句现有文本已成立但被普遍忽视,提级为显式句。


第三档收窄:其余 unresolved_related(非纯邻近,或内容过了 §1)仍升级人工,但按 review.md 既有的紧凑表格形态批量呈现。


review.md 第 15 行放宽:现文把 candidate_get 展开限定在 contradiction/revises;若 N3 落地,纯邻近判定直接在 compact 行可读,无需展开,该句不需大改——只需在批量弃置示例边上补一个纯邻近 reason 范例。


ACK notice / tool description:description 预算实测 7977/8180,余量仅 203 字节。建议本轮不动 description(三档定义处唯一在 §7,Skill 通道已实证被 cursor/codex 加载),把这 203 字节留给未来真正需要 host 必达通道的措辞。若你要求 description 同步,需等量删减既有文字。




WP-N3 · 纯邻近标注(服务端派生字段,只加信息)阶段 1


判定谓词(从梯子结构推导,已被 102 个实测 assessment 完全印证):


proximity_only :=
     relation == unresolved_related
  && paths 不含 topic_equality          // 排除"topic 相等但被读作改写"分支
  && paths 不含 exact_artifact_graph    // 排除"同代码但相似度不够"分支
  && paths 不含 explicit_related_context // 排除用户显式关联(梯子现有漏洞,一并封堵)
跨 kind 的 shared_identifier 不阻断——结构上已证明 unresolved_related 下出现的共享标识符必然跨 kind(同 kind ≥2 会在第 5 级被截走)。


落点:CompactCandidateAssessment 新增 proximity_only: bool(skip_serializing_if false 时省略)。可行性已核实:candidate_list 输出没有被逐字节冻结(冻结只覆盖 inputSchema,不存在 outputSchema),现有测试全是存在性断言而非穷举 key set;唯一软约束是 compact tokens 严格小于 full,一个布尔字段无碍。谓词实现放在 candidate.rs 梯子旁边、由梯子函数派生(沿用 delivers_model_visible_context "派生而非重述"的先例),加一致性测试锁住。



WP-N4 · 审计地基(为开闸裁定供数)阶段 1


开闸与否必须用数据裁定,而现状无法审计:relation 只活在 candidate_analysis.candidate_json blob 里,且该表会被过期清理;sctx candidate stats 只有 decision_source × status 两维。



candidate_review 加两列:top_relation TEXT、proximity_only INTEGER,build 落库时写入(migration 循 decision_source 列先例)。这是唯一能长期审计的形态。


sctx candidate stats 增加 relation × proximity_only × decision_source × 处置 的分组输出;auto_confirm_rejection 表现成可用(拒绝消息文本已含 relation 名)。




阶段 2 · 自动确认开闸评估预注册判据,暂不实施


若阶段 1 运行后仍要提升确认侧自动化,预注册以下判据(现在定、防止事后拟合;阈值待你裁定,见决策点 3):



样本门:累计 ≥30 条 proximity_only 行经人工处置(N4 供数)。


质量门:人工对"proximity_only 且过 §1 过滤器"行的确认率 ≥90%;期间零例"确认后又被 withdraw/supersede 的 proximity_only Context"。


开闸内容:权限面第 ④ 条纳入 unresolved_related && proximity_only;同时必须解决第 ② 条(见决策点 4),否则实效仍为零。


回撤路径(已在):sctx context withdraw --decision-source agent_policy [--external-session] [--dry-run] 批量回撤,annotations 里的 decision_source/author/session 全程可审计。注意其非原子(部分失败留已撤部分)且仅覆盖本机 runtime 确认的 Context。




3. 风险分析



工作项风险缓解 / 回撤


N1 源头收紧欠沉淀回潮(cursor 曾有 4 轮零 checkpoint 前科);"三问"被读得过严,真决策也被滤掉"announcing is not making one" 句已在;N4 上线后观察每会话 claims 数与人工确认率,连续走低即回调措辞。纯文档改动,回撤 = revert


N2 第一档扩权(措辞)agent 误弃真知识(discard 无服务端权限面,纯靠 prompt 自律)设计文档原话:"a wrong discard costs a later re-Checkpoint, not a correction"——弃置不写 Git 事实,成本恰好落在"丢失可接受"一侧,是与你的原则对齐度最高的一项。reason 必填且入库可审计


N3 标注字段谓词随梯子演化漂移;compact 行体积增长谓词由梯子函数派生 + 一致性测试(先例:delivers_model_visible_context);布尔字段仅在 true 时序列化


N4 migration + statsschema migration 风险;stats 输出形状变化影响 cli_contract 断言循 decision_source 列的既有 migration 测试范式;stats 是加法字段,断言同步改


阶段 2 开闸最大实质风险:低相似矛盾天然漏检。否定冲突检测只在相似度 ≥8000 时激活,纯邻近区间(实测 574–1538bp)的矛盾表述结构上不可能被梯子捕获,自动确认会把它们直接写进 accepted 库这正是判据里"零 withdraw/supersede 后验"一条存在的原因;开闸后 proximity_only 自动确认单独计数,发现后验矛盾即熔断回人工。是否值得为此增加对称否定检查,留为开放问题,不预先过度设计





单主题偏置声明:本方案全部实证数据来自一个 search 域单专题知识库。判据阈值(90%/30 条)在多主题、多 Space 语料上可能需要重校——这与 F2LLM floor "在真正出货的栈上重测"是同一条纪律,阶段 2 裁定时按当时语料重新核对分布。



4. 待你裁定的 5 个决策点



D1 · 阶段 1 四项是否全部开工


N1/N2/N3/N4 互相独立但 N2 的第一档扩句依赖 N3 的字段。建议:全部开工,一个 work package,顺序 N3→N4→N1→N2。





D2 · 标注字段形态


A. proximity_only: bool(简单,一步到位);B. relation_basis 枚举(proximity / topic_paraphrase / weak_artifact,信息更全但消费方今天只用得上一个值)。建议 A,枚举等出现第二个消费者再升级(与"第三个 additional_context 消费者出现时再做结构"同一纪律)。





D3 · 阶段 2 判据阈值现在预注册,还是到时再定


预注册防事后拟合,但 30 条/90% 是拍的。建议:现在预注册(写进 ADR),数值标注"可在开闸裁定时用当时分布复核",两全。





D4 · needs_space_review 闸的方向(阶段 2 的前置)


A. 做好 Space bootstrap:让 candidate 自然命中已有 Space,candidate_status 自然到 ready_for_review——不动权限面,治本但依赖使用习惯(space_create 一次性引导);B. 有限放宽:推荐为已有 Space 时 needs_space_review 可过、proposed_new_space_intent 不可过。建议 A 先行(阶段 1 期间观察 Space 命中率),B 留作阶段 2 裁定时的备选——放宽 Space 判断是把治理决策交给自动化,与噪声原则相抵。





D5 · tool description 是否同步(203 字节内)


三档定义唯一在 §7,Skill 加载已在两个 host 实证。建议本轮不动 description,把预算留给未来必须走 host 必达通道的措辞;若你不放心 Skill 送达的覆盖率,再做等量删减方案。





实证与代码事实来源:crates/search/src/candidate.rs(relation 梯子)、crates/mcp/src/lib.rs(权限面/compact 行/预算)、crates/task-runtime(stats/withdraw)、runtime.sqlite 中 7d8cbab1 会话 7 条 candidate 与 102 个 assessment 的原始记录。相关先例:ADR-0004(先校准再定数)、ADR-0005(处置三档)、ADR-0006(通道与收件人)。
