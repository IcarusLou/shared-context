# ADR-0004: Embedding 召回通道

状态：Accepted（2026-09-02 用户批准）

## 背景

R1–R5 之后，词法召回已到达天花板。R3 的实测结论：剩余探针失败全部是纯同义 / tf 问题——「砍掉 / 排除」「参数拼装 / addParamsForLiveAnchor」之间没有词法桥；zh-01/zh-02 的近重复并列中 matched token 完全相同，任何字段权重与 IDF 次级键都无法翻转（IDF 假设已被实测证伪）。真实会话同样暴露跨语言缺口（英文 Intent / 查询对中文知识库，词法通道 0/4）与跨仓语义区分需求（决策 5 否决仓库加权后，语义区分是 Q1 的唯一长期方案）。

2026-09-01 的原型（本机 9 条真实 accepted + 两套探针，bge-m3 vs multilingual-e5-small）：

| 语料 | 词法/sctx | bge-m3 | RRF(词法+bge-m3) |
|---|---|---|---|
| 真实语料 17 正例 | 12/17 | **16/17**（4 条跨语言全对） | 15/17，hit@3 17/17 |
| probe-v1 | 20/22 | 17/21（丢标识符） | **20/21** |
| probe-zh | 20/24 | **20/22** | 19/22 |

关键定性：e5-small 噪声不可分（正例最低 0.788 vs 噪声最高 0.86），不可用；bge-m3 阈值 0.50 可分（拒噪 7/9、零正例丢失）；embedding 单通道丢标识符查询、对近重复干扰与否定极性无效——**只能作为 fusion 的一路，不能替代**。

## 决策

1. **通道形态**：embedding 作为 RRF fusion 中新的一路 `SemanticSimilarity`，通道权重 3（与 hint 通道同级），**不参与任何门槛判定**（AutomaticTextEligibility、覆盖率、answerable 守卫均不变）、不产生事实、不进 Hook 热路径。相似度低于 0.52 的候选不进入该通道（T5a 扩展集重标定定稿：噪声上界 0.5095、跨语言正例下界 0.5640，0.52 高于噪声 105bp；0.50 会放进一条噪声查询并经资格路径注入无关 Context）。
补充（实装批准时明确）：语义命中 ≥ 阈值构成一条独立的注入资格路径（与 exact EngineeringGraph / ContextRelation 同级的替代条件），但不放宽任何文本门槛本身；语料向量为可丢弃本地缓存（独立 sqlite 文件，键含模型指纹与 SEARCH_RANKING_VERSION），由后台线程回填。

2. **模型**：bge-m3 级别的多语模型；分发形态为「不随包分发」——`[retrieval] embedding_model_path` 指向用户显式下载的本地模型，未配置时通道整体关闭且零成本；`sctx doctor` 提示获取方式。获取方式已收敛为一条命令 `sctx embedding install`（T5c）：下载模型与 ONNX Runtime、按内置 SHA-256 逐文件校验、加载模型编码一句话自检通过后才写 `[retrieval]`、再回填向量缓存；`--model-url` 指向团队内网镜像时仍按同一组内置摘要校验（摘要是文件的属性，不是站点的属性）。这不改变「不随包分发」的决策——二进制里只有摘要，没有权重。磁盘 ~2.1GB、RSS ~1.2GB、加载 9–12s（一次性，常驻 MCP 进程）、查询编码 p95 30–85ms。
3. **推理**：MCP 进程内 ort（ONNX Runtime，`load-dynamic`：运行时加载 `[retrieval]` 配置指向的 onnxruntime 动态库，构建期零网络依赖）；模型在 serve 启动时后台线程加载（避免首查付 9–12s），就绪前查询按第 4 条降级；索引侧在投影更新时为 accepted revision 计算并缓存向量（`SEARCH_RANKING_VERSION` bump 触发重建）；向量存 index.sqlite 新表，按 revision_id 键控。
4. **降级语义**：模型缺失 / 加载失败 / 单次编码超时（预算 200ms）→ 该路静默为空 + `omitted.reason = "embedding_unavailable"`（沿用 R1 的可解释 omission 机制），词法结果不受影响。
5. **不做**：不用 embedding 做去重（statement bigram Jaccard 分离度更好，R5 已用）；不做「embedding 单通道模式」；不引入远程 embedding 服务（隐私边界：知识正文不出本机）。

## 验收

- T5a 扩展探针集（≥50 正例 + ≥7 噪声，含 category 标注）作为对照基线：`paraphrase` 与 `cross_lingual` 两类必须有净提升，`identifier` 与 `noise` 不得回退；两套既有 blocking 探针不回退。
- `task_context` p95 增量 ≤ 100ms（模型已加载态）。
- 关闭配置时全部行为与无此通道逐字节一致。

## 备选与否决

- **e5-small / 更小模型**：噪声不可分，否决（原型实测）。
- **仓库先验加权替代语义区分**：压制跨端联想，用户于 2026-09-02 否决（诊断页决策 5）。
- **jieba 分词**：逐探针零变化，2026-08-30 已否决（DEVELOPMENT.md）。

## 2026-09-03 修订：编码预算按真实 Intent 长度重标定

原决策文字不变，本节只补录一条被实测推翻的参数及其后果。

### 事实

决策 2 记的「查询编码 p95 30–85ms」来自 2026-09-01 的 torch 原型，不是本仓最终采用的 ort 栈；决策 4 的 200ms 预算据此设定，并在 T5b 用 15–40 字符的探针查询验收通过。

真实自动注入的查询不是这个形态。`semantic_query_text` 把 Working Intent 拍平成 `goal + current_direction + in_scope`：真实 Codex 会话 01a06646 的查询实测 **283 字符**，该会话 3 次自动检索 100% 报 `embedding_unavailable`；受控实验中模型加载完成后连发 5 次调用仍 5/5 不可用。

按查询长度实测（2026-09-03，Apple Silicon / macOS 24.6.0 / ONNX Runtime 1.28.1 / bge-m3 ONNX export / release / warm，每档 24 次，`crates/search/tests/embedding_encode_latency.rs`）：

| 字符数 | p50 | p95 | max |
|---:|---:|---:|---:|
| 20 | 28 | 33 | 37 |
| 40 | 33 | 37 | 43 |
| 100 | 74 | 78 | 81 |
| 200 | 139 | 153 | 164 |
| **283** | **197** | **235** | **243** |
| 400 | 274 | 298 | 301 |
| 700 | 458 | 539 | 633 |
| 1400 | 757 | 816 | 828 |

编码耗时随查询长度近似线性。200ms 预算只覆盖到约 250 字符——恰好落在探针（通过）与真实 Intent（超时）之间。这不是模型慢，是预算用错了长度标定。

### 修订

1. **预算默认值 200ms → 1200ms**，并新增可选配置 `[retrieval] embedding_encode_budget_ms`（接受 50–30000）。1200ms 是 512-token 截断上限处实测 p95（816ms）之上留余量的取值；它是上限而非典型开销：283 字符查询实际约 200ms 返回，且命中查询向量缓存时为零。余量的意义是让比本机慢约 5 倍的机器仍能答出真实 Intent，而不是静默降级。

2. **验收条款「`task_context` p95 增量 ≤ 100ms（模型已加载态）」按调用形态拆分**——原条款在 ort 栈上对真实长度查询不可达，继续挂着它只会让下一次验收再次用短查询绕过：
   - **同一 Intent 的重复检索：仍 ≤ 100ms。** 这是自动注入的主导形态（task_context 重读、artifact_focus 同一 Task 反复触发），由新增的进程内查询向量 LRU 保证；原条款在这一形态上原样保留。
   - **某个 Intent 的首次检索：≤ 400ms。** 这是通道第一次看到一个新 Intent 的诚实代价（283 字符实测 235ms + 融合开销 + 余量）。原条款把它记成 100ms，代价是从未真正支付过——通道在真实会话里恒不可用。

3. **超时不再无痕迹。** 编码耗时与超时计数记入 `semantic.sqlite` 的 `encode_sample` 表（可丢弃缓存，保留最近 64 条），`sctx embedding status` 输出分布，`sctx doctor` 在超时占多数时给出调 `embedding_encode_budget_ms` 的提示。原降级语义不变（仍是 `embedding_unavailable` + 词法不受影响），改的是它是否可被发现。

4. **超时的编码不再作废**：后台 worker 完成后仍把向量写入查询缓存，因此同一 Intent 的下一次调用直接命中。系统性超时的形态从「永久静默失效」变成「首次慢、后续快」。

### 教训

「静默降级 + fail-open」把一个参数标定错误放大成了全局静默失效，且无任何可查痕迹——这是本项目反复出现的形态。可观测性不是附加项：一条只在 Pack 里写 `embedding_unavailable`、不区分「模型没装」与「预算不够」的降级，等价于没有报告。
