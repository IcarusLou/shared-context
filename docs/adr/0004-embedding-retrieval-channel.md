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

1. **通道形态**：embedding 作为 RRF fusion 中新的一路 `SemanticSimilarity`，通道权重 3（与 hint 通道同级），**不参与任何门槛判定**（AutomaticTextEligibility、覆盖率、answerable 守卫均不变）、不产生事实、不进 Hook 热路径。相似度低于 0.50 的候选不进入该通道（阈值以 T5a 扩展探针集 ≥50 条重新标定后定稿）。
补充（实装批准时明确）：语义命中 ≥ 阈值构成一条独立的注入资格路径（与 exact EngineeringGraph / ContextRelation 同级的替代条件），但不放宽任何文本门槛本身；语料向量为可丢弃本地缓存（独立 sqlite 文件，键含模型指纹与 SEARCH_RANKING_VERSION），由后台线程回填。

2. **模型**：bge-m3 级别的多语模型；分发形态为「不随包分发」——`[retrieval] embedding_model_path` 指向用户显式下载的本地模型，未配置时通道整体关闭且零成本；`sctx doctor` 提示获取方式。磁盘 ~2.1GB、RSS ~1.2GB、加载 9–12s（一次性，常驻 MCP 进程）、查询编码 p95 30–85ms。
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
