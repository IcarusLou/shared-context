# 双径检索重建 · 实施方案(Step 2)

权威依据:ADR-0007(两径两 floor)、检索准入重构 v3.1(会话 artifact,其结论已折入本文)、Step 0a/0b 实测(scratchpad/step0,可复算)、R1–R4 审计的测试面盘点(候删/保留/改写清单折入 S2-4)。分支:`feat/dual-lane-retrieval`。执行模式:每包一个实现 agent,主会话逐 diff review、独立复跑后接受;分包提交;不 push。

## 目标语义(一句话版)

自动注入 = 径 A(触碰文件 → 锚定 Context,零模糊)产出种子与直接命中 + 径 B(种子 Context ↔ 候选 Context 文档-文档余弦 ≥ `SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS`)产出跨仓/跨主题联想。词法重叠永不独立授予注入资格。budget 是 cap 不是 target;空 pack 一等公民。显式 `context_search` 不动。candidate 分析侧检索栈(crates/search/src/candidate.rs)不动。

## 分包

### S2-1 · 径 A:锚点供给与文件→Context 反查

- **锚点集合**(三源并集,按 Task/Session 聚合):
  1. 本 Session 全部 workspace/diff task_signal 的文件——**含 superseded**(签名语义本就是 "superseded signals remain historical local records but leave retrieval";径 A 的锚点是历史足迹,不是检索状态。实测 17h 会话 145 个触碰文件仅 16 个 active,修真输入面积 ×9)。只读既有表,不加新表、不改 signal 生命周期。
  2. 当前 Task Intent 的 `artifact_hints`(已有字段)。
  3. `task_artifact_focus` 解析成功的 Artifact(提为径 A 一等入口:focus 命中的 Context 进入种子集合)。
- **反查**:`(repo,path)` → engineering_reference → **仅当前 accepted revision** 的 Context(审计实测 73 行中 6 行挂在非当前 revision,必须过滤,否则从文件反查出已作废知识)。
- **产出**:`LaneAHit { context, anchors: Vec<(repo,path)> }`;why 直接渲染 `file:line` 出处。
- **明确不做**:不做任何图遍历/relation 跳数;不做相似度;不建新表;不动 signal 采集端。
- **验收**:单测(stale-revision 过滤、superseded signal 纳入);以 Step 0b 三会话数据形态构造 fixture 复现"种子 6/0→若干/1"的改善(17h 会话在全历史锚点下应产出非零种子——它触碰过的文件里有锚定文件吗?0b 实测 145 个文件命中 0,但那是 16 槽窗口下的 active 集;全历史 145 个仍命中 0(意外 #3),所以 17h 会话的种子要靠 S2-2 的非语义边,验收按此陈述,不虚构改善)。

### S2-2 · 种子扩展:非语义边

- 种子集合上追加两类零成本边(解决 38% 零锚定知识与横切知识的可达性——ADR-0007 定性为信号问题):
  1. **同 `problem_view`**:与种子同 problem_view 的 accepted Context 直接入种子(map 会话:fb1f06df→c1a d461d/2ea272dd 即由此可达)。
  2. **同 `topic_key`**(非空且相等)。
- 扩展一跳即止,不递归。产出仍是种子(可再作径 B 查询),why 注明边类型。
- **明确不做**:不引入 Space 边(Space 关联面广,首版控噪);不动 topic_key 生成(其 kind 前缀不跟随缺陷另案)。
- **验收**:单测 + 以真实装置数据形态的 fixture 断言 map 系种子从 0→≥1。

### S2-3 · 径 B:第二跳准入

- **查询**:种子 Context 的**文档向量**(semantic.sqlite 缓存即有,R0 后为原文空间)对全部 accepted 文档向量做余弦;无需新编码路径,查询侧 encode 不参与。
- **准入**:`score >= hop2_floor`,floor 默认 `SEMANTIC_HOP2_ADMISSION_FLOOR_BASIS_POINTS`(5200),按预注册开配置键 `[retrieval] hop2_admission_floor_basis_points`(范围校验 3000–9000);**每次准入判定记录分数样本**(semantic.sqlite 新小表,循 encode_sample 模式:保留最近 N 条,可丢弃缓存,记 seed/candidate/score/admitted),供真实流量复标。
- **排序**:准入集合内按 score 降序;共享标识符(语法校验后的真标识符交集)只作同分细排与 why 佐证。
- **明确不做**:意图文本不参与径 B;不做多跳;向量缺失(未回填)的候选静默跳过并计数(不阻塞、不同步编码)。
- **验收**:hard-negative-v1 fixture 上端到端 0 假阳(棘轮);Step 0a 装置矩阵抽样回放一致;分数样本表落库断言。

### S2-4 · pack 装配替换 + 融合层拆除 + 测试面执行

- **装配**:自动注入(`task_intent_update`/`task_context` 的 automatic 模式)改为:径 A 命中 + 径 B 准入 → 按 Context 所属 Space 分组渲染(**wire 形状不变**,Space→items 结构保留,association score 改由径证据派生:径 A=固定高分+出处、径 B=hop2 score);budget 变 cap(超出按 score 截断,不足不补);空结果渲染既定一行。why 重写为径语义(A: `anchored: <repo:path>`;B: `associated via <seed-id> at <score>`),删除 fused/coverage/query_token_explanation 全族。
- **拆除**(推翻清单):八通道加权 RRF(`FUSION_CHANNEL_WEIGHT` 族、`assign_channel_features`、`reciprocal_rank_micros` 的 pack 侧用途)、scope 通道召回、`AUTOMATIC_RELEVANCE_FLOOR_BASIS_POINTS`、coverage 词法准入与 `AUTOMATIC_MIN_ANSWERABLE_RATIO`、fill-to-budget、`TaskAssociationFusionExplanation`/`AutomaticQueryTokenExplanation` 载荷。**显式 `context_search` 保持现行为**(它自己的词法+语义排序不动;若与 pack 共用代码,拆分而非拆除)。
- **测试面**(按 2026-09-10 盘点,file::test 粒度清单在会话记录,此处列硬边界):候删 ≈1351 行(task_association.rs 纯融合段 ~902、mcp_contract `working_intent_hint_text_...` 整测 194、probe harness 融合诊断 108、semantic_channel 两测 81、milestone_two/working_intent 零星 66);改写 ≈254 行(含 `assert_typed_m2_path` 穷尽 match 裁剪、`retrieval_quality_workflow` L414 空断言重论证、context_usage why 断言);**不许误伤**:candidate_analysis.rs 整文件、task_association 中 wire/Space 语义 ~1760 行、embedding 四文件、semantic_channel 其余 25 测(尤其"语义命中单独构成注入资格"两测——改写为径 B 语义而非删除)、search_contract 全部、probe fixture 628 行全留;三个命中率棘轮(probe/zh/ext)**重基线化**(三次一致实测,新数字记录推导)。
- **验收**:全 workspace 测试绿;hard-negative 端到端棘轮 0 假阳;三棘轮新基线三次一致;`context_search` 行为快照不变;fmt/clippy 零告警。

### S2-5 · wire 信封与降级链

- `estimated_tokens` 按信封口径重计(含 why/evidence/omitted/缩进/MCP text 转义);pack 总 wire ≤10K 硬校验,超限降级链:full→compact→仅 statement→按 score 尾部省略(omitted 如实列出)。
- **验收**:构造超限语料断言逐级降级且始终合法 JSON;真实 26 条库全量注入场景 wire 实测 <10K。

### 收尾 · 验证与合并门

- 0c 回放(仅 Cursor 宿主):4b2e9fb5 原样 + 01a08baf 跨端时间盒强杀;对照矩阵(注入条数/跑题数/截断数/种子来源)写入 bundle 分析。
- 合并门:上述验收全绿 + 回放对照无回归 + 我方 review 记录齐全。

## 顺序与依赖

S2-1 → S2-2 → S2-3 可流水(2 依赖 1 的种子形态,3 依赖 1+2);S2-4 依赖 1–3;S2-5 可与 S2-4 并行开发、合并时串接。每包独立提交,S2-4 的拆除与装配在同一包内完成(避免双栈中间态过夜)。

## S2-1/S2-2 实施注记(review 定案,S2-3/S2-4 必读)

- 落点 `crates/search/src/lanes.rs`(私有模块,S2-4 接线前 allow(dead_code));过滤统一走 `SAFE_ACCEPTED_CONTEXT_PREDICATE`(它同时覆盖 stale revision 与 superseded Context 两种旧引用形态,实测装置的 6 行旧引用全部属于后者)。
- **17h 会话(01a08baf)在径 A+B 下确定为空包,这是设计事实而非缺陷**:145 个全历史触碰文件与 56 个锚定坐标零交集(连 basename 都不重叠),artifact_hints 是 /private/tmp 的 .md(非代码路径),无 focus 调用——种子为零,S2-2/S2-3 无从展开。按噪声公理,空包正确;S2-4 的空包渲染按此语义书写。
- superseded signal 纳入是"足迹修真"(输入面积 16→145),在三个探针会话上不改变召回条数——不得把它表述为召回提升。
- 仓限定锚点只做精确路径匹配(不做 basename 放宽);basename 仅用于无仓 artifact_hints 且带 `AnchorMatchBasis::Basename` 标记,S2-4 装配端可按噪声表现降级。
- `task_artifact_focus` 读的是 engineering.sqlite(resolved_reference/graph_context_snapshot),与径 A 反查(index)是两个投影:S2-1 取 focus 的 resolved (repo,path) 回到 index 侧反查,两侧可能不一致——S2-4 遇到不一致时以 index 侧为准并在 why 里如实呈现。

## S2-3 实施注记(review 定案,S2-4 必读)

- `lane_b_hits` 不持 provider、不做任何查询编码;样本由函数**返回**而非写入(index 与 semantic 两库分离),S2-4 装配端负责把 `admission.samples` 交给 `record_hop2_admissions`。
- 最强种子内联为 `SeedMatch`、其余在 `also_admitted_by`——无重复状态、无可 panic 的 seeds[0]。
- 标识符校验复用 `hints::normalized_identifiers`(已含语法门:CamelCase≥2 段、snake≥6 字符等,`x-ttk-map-view` 切片贡献为零)——plan 的"最小校验"应急条款未启用。
- 配置键 `[retrieval] hop2_admission_floor_basis_points` 已落 local-state(3000–9000 拒绝式校验),**engine 接线与 user-guide 条目刻意留给 S2-4**(不文档化惰性旋钮)。
- 公开面小幅移动:样本表三类型两方法经公共 `SemanticVectorCache` 导出(lane 类型仍 pub(crate))——review 裁定接受。
- 棘轮是 in-crate 单测(模块私有所致),调用形态 `cargo test --release -p sctx-search --lib -- --ignored hop2_ratchet`;三次读数 13/18/0 与校准文档一致;装置矩阵 5140 行与 seed_sweep 逐字节一致,5200 行与 ADR 的 62/66 声明一致(注意 ordered/unordered 两种计数口径,均已记录)。
- 数据佐证:5200 下 2ea272dd 仅可经 problem_view 边到达——S2-2 的必要性由 S2-3 实测反向确认。

## S2-4 中场裁定(review 定案,S2-4b 续包依据)

1. **不变量:一个 Context 是种子,当且仅当它在包里**(扩展种子经 SeedExpansion 入包,已实现)。
2. **topic_key 边删除**(不留惰性代码):fixture 实测噪声 + 真机 topic_key 前缀不跟随 kind 改写的既有缺陷,两头不可靠;problem_view 独自承担非语义边。恢复条件:topic_key 获得可靠性修复后另案评估。
3. **探针棘轮现状接受**("每个自动包为空 + 零噪声"断言 + 文档化恢复路径);**合并门新增前置**:最小 lane 探针集——从既有探针挑 ~10 条补文件足迹(engineering reference + workspace signal),恢复 lane A/B 命中率棘轮(三次一致),否则自动注入侧只有负向棘轮。
4. **改名容差与 graph 诊断退役是有意的**:图谱已裁定降级为 relocation 记账;pack 自解释实测负价值。deferred 跟进:"径 A 反查 miss 时咨询 relocation 记录"。graph_retrieval 失败据此分流:引用解析类保留修复、pack 图通道类退役。
5. **融合常量族的 pack 侧退役完成**;其余存活仅因 candidate 分析栈(B1 冻结)与不可达的 Explicit 模式——全量删除随 B1,Explicit 模式死代码一并在 B1 清点。
6. **60 个失败的分流规则**(逐文件执行):(i) 测试意图是"文本匹配让自动包含 X"→ 退役删除(旧语义);(ii) 意图是 wire 形状/Space 分组/budget/compact 字节稳定/usage 记录 → 给 fixture 补文件足迹使包非空后修复断言(mcp compact 稳定性需 ≥8 items 的 lane-fed fixture);(iii) graph_retrieval 按第 4 条分流;(iv) 严禁为了让测试通过而弱化空包语义或降低 hop2 floor。
