# 01 · 可引入业界成熟方案的改动点

> 状态：进行中。由领域分析子任务产出、经编排者 review 后录入；每条含现状、候选方案（2~3 个）、取舍与迁移风险。
> 更新日志见文末。

## 测试与验收基础设施（来源：领域 F，已 review）

### 1. scenario-runner 故障调度叠加 property-based 测试

- **现状**：`crates/scenario-runner/src/schedule.rs`（`compile_schedule`）+ `crates/scenario-contract/src/model.rs` 的 `FaultKind`（Drop/Repeat/Reorder/Crash/Concurrent）是自研确定性故障注入原语，目前只有 6 个手写场景做阻断回归（`docs/dynamic-replay-phase-one.md` 将更大故障空间列为 deferred）。
- **可替代性**：`ScenarioDefinition` 已是强类型、可序列化、校验与执行解耦的结构，天然适合作为生成式测试的目标，无需新造执行引擎。
- **候选方案**：
  1. **proptest**（推荐）：为 `ScenarioDefinition`/`FaultKind` 实现 `Arbitrary`，用随机 `after` 依赖图 + `FaultPlan` 序列探索交错空间，shrinking 定位最小复现；对现有执行器零侵入，6 个手写场景仍保留为黄金路径。
  2. **madsim / turmoil**：面向网络分布式仿真；本系统是单进程+子进程+SQLite，拓扑单一，收益有限，仅备选。
  3. **Jepsen**：不变量思想与 `InvariantKind` 闭集同源，但面向多节点网络分区，引入 JVM/SSH 编排成本远超收益，不建议。
- **风险/成本**：低到中。主要成本在生成策略要满足身份引用图合法性（`SUPPORTED_ACTION_TYPES` 等约束）。

### 2. 联想探针评估口径对齐标准 IR 指标

- **现状**：`crates/cli/tests/association_probe_harness/mod.rs` 只统计 top-1 命中的聚合计数（en ≥18/22，zh ≥19/24，已核实 `association_probe_workflow.rs:51-55`），无 Recall@k/MRR/nDCG，不区分漏检与错检。
- **可替代性**：标准 IR 评估问题，业界指标能在不动检索算法的前提下让回归信号更早、更可定位；且为将来 embedding 通道（已确认缺口、方案搁置）预备同一套 apples-to-apples 评估管线。
- **候选方案**：
  1. **trec_eval / ir_measures**：把 `expected_context_indexes` 转 qrels、排序结果转 run 文件，得到 nDCG@5/MRR/Recall@3；`tests/scripts/` 已有 Python 先例，接入自然。
  2. **BEIR 风格语料/查询/qrels 三分离格式**：把 probe JSON 结构化为可扩展基准集。
- **风险/成本**：低，只动评估/报告层（`emit`），不触及检索算法与 MCP/CLI 契约。

### 明确不建议替换的点（记录在案）

- scenario-runner 手写的 MCP 帧协议读写（`crates/scenario-runner/src/process.rs` 的 `McpChild`）：故障注入需要精确控制部分写入、崩溃时机、畸形响应分类，换标准 MCP SDK 反而削弱这条能力。

## CLI 与安装/发布链路（来源：领域 E，已 review）

### 3. CLI 参数解析器换用 clap（优先级不高）

- **现状**：`crates/cli/src/args.rs`（88 行）自研 `--flag value` 解析器，无短选项聚合、无 `--flag=value`、无子命令自动路由；`main.rs` 散落 ≥6 处手写 usage 常量，每个子命令重复 `is_help` 分支。
- **候选方案**：① **clap (derive)**：生态标准，自动 help/错误建议/shell 补全，代价是二进制体积与编译时间略增；② **argh**：更轻量，生态弱于 clap；③ **保持现状**：若命令面主要由 Agent/脚本消费，help 体验边际价值低，且无第三方行为漂移。
- **风险/成本**：中。契约测试 ≥17 处直接断言错误消息文本（对应 `args.rs:47,55,62`），迁移需同步重写断言；JSON envelope 层不受影响。收益是可维护性而非正确性。

### 弱候选（记录）

- task-runtime schema 丢弃白名单（`installer/src/lib.rs:3782-3789` 硬编码 11/12）可考虑 `rusqlite_migration`/`refinery` 表达为通用规则（详见 04 文档 E-6）。
- `npm/scripts/lib/release.js` 手写 SemVer 正则可换 `semver` npm 包，收益小。

### 明确不建议替换的点（记录在案，避免重复调查）

- **JSON/TOML 配置合并 + ownership 追踪**（`merge_json_hooks` 等，installer lib.rs:2655-2891）：核心是"记住自己上次写过什么、用户改过则保留+notice"的三态语义，通用 merge/patch 库不覆盖。
- **Transaction/Journal 文件系统事务**（installer lib.rs:1440-1575）：写前快照+回滚，无成熟通用 Rust 库覆盖此语义，且有逐崩溃点测试。
- **npm 平台包分发模式**：scoped 平台包 + optionalDependencies + launcher 是 esbuild/swc 标准做法，已是成熟模式；其 SHA-256+codesign 校验是合理加固。
- **确定性 tar.gz 打包**（自实现 ustar 头）：为可复现构建刻意自研，`tar-stream` 等不保证确定性输出。

## 存储与事件层（来源：领域 A，已 review，关键机制声明已逐条核实）

### 4. Git 访问全走 `git` 子进程 —— 投影与同步的规模天花板（高优先）

- **现状**：每个 Git 操作一次 fork/exec（`git-store/src/git.rs:18-45`）；全量重建每个 blob 一次 `git cat-file`（`index/src/git_tree.rs:139-156`，N 事件 = N+1 进程）；`validate_committed_events` 每事件 2 个进程，且 knowledge sync 每次 merge 前后各跑一遍。
- **候选方案**：① **`git cat-file --batch` 长驻管道**（最小改动，O(N) 进程降 O(1)，先做）；② **gix（gitoxide）**：纯 Rust、读侧成熟、写侧只需"造 tree+commit+更新 ref"正好是稳定部分；③ **git2（libgit2）**：写 API 最成熟，代价是 C 依赖影响交叉编译与 npm 分发体积。
- **风险/成本**：中。换对象库后追加协议不再经过 index，实际会简化（不用防 foreign staged），但 `git_writer.rs` 大量以 index 为断言对象的测试要重写。仓库仍是标准 Git 仓库，用户可用原生 git 检查。

### 5. 捆绑 JSON Schema 是第二份"事实"，无一致性保证

- **现状**：880 行 `event-v1.schema.json` 被写进 Git 供他人解读，但**从不参与运行时校验**（校验走 serde+手写 `validate()`），Rust 类型与 schema 之间零一致性测试——团队共享场景（G3/G5）下别的工具按这份 schema 读，可能已漂移。
- **候选方案**：① **`jsonschema` crate 双向契约测试**（保持手写 schema，CI 里用 fixtures 的 valid/invalid 样本双向校验；几十行、零风险，**建议优先**）；② **schemars 从 Rust 类型生成并断言与提交文件相等**（更彻底；`tag`+`flatten` 组合支持有限需手写部分 impl）；③ typify 反向生成（与手写 validate 业务不变量冲突，不推荐）。

### 6. 手写隐私扫描器：算法与规则库均可替换

- **现状**：`local-state/src/privacy.rs` 手写 O(n·m) 多次全文扫描，每次扫描还复制整份输入做小写化。
- **候选方案**：① **aho-corasick**（一次线性扫描匹配全部模式 + 内置 case-insensitive，标准做法，几乎无代价）；② **regex/RegexSet**（规则可外置成配置，Rust regex 无回溯安全）；③ **接入 gitleaks/detect-secrets 规则集**（工业级覆盖面与已调优的误报率，需映射到 `PrivacyFindingKind` 并核许可证）。
- **备注**：误报后果（见 04 文档 A-2）比性能更值得先处理；`privacy_contract.rs` + fixtures 已固化行为，替换有回归网。

### 7. SubmissionId 幂等编排可简化

- **现状**：幂等靠"专用锁→同步索引→lookup→比 hash→写→再同步→回读"编排，占 `store.rs` 约 40%，且 submit 与 confirm 是两份同构的 ~350 行代码。
- **候选方案**：① **先做纯重构**：抽泛型 `IdempotentAppend<K,H>` 让两份同构代码合一（低风险纯收益）；② 幂等收据落 `runtime.sqlite` UNIQUE 约束、现有编排降级为恢复路径（需重新论证 ADR-0003"删 Runtime 不改变既有事实"不变量，成本高）；③ index 侧 UNIQUE 仅用于写入侧探测（投影必须能表达重复这一合法坏状态，不能改表结构）。

### 8. 全文检索通道（谨慎，语义方案定型前不动）

- **现状**：自写 tokenizer（NFKC/case-fold/标识符切分/Han 二元组）把预切分文本拼空格喂默认 FTS5。
- **候选方案**：① **FTS5 自定义 tokenizer**（索引存原文，`snippet()/highlight()` 可用；需 FFI，工程量中）；② **tantivy**（自带 CJK/BM25，为 embedding 混合检索留空间；但破坏"一个事务切换投影"的 generation 原子性，**风险最高，embedding ADR 定型前不建议动**）；③ 维持现状（tokenizer 版本已正确参与重建触发）。

### 明确不建议替换（记录在案）

- **Batch Journal + create_new + fsync + phase 标记的 crash-safe 追加协议**（含 13 个 CrashSeam 注入点全覆盖测试）：现成 WAL 库都假定拥有存储，这里目标存储是 Git 工作树+index，无成熟件可复用，自研合理。

## 协议与宿主接入层（来源：领域 D，已 review）

### 9. MCP 传输/协议层：**rmcp 不适合**，可做局部替换

- **结论先行**：官方 Rust SDK rmcp **不建议**替代自研 MCP 层。两个硬阻力：① rmcp 强绑 tokio，而本仓全同步（rusqlite/Git 子进程/文件锁 + `authorize_and_call` 的严格授权线性化顺序）；② rmcp/schemars 生成的 schema 带 `oneOf/anyOf`——正是 f15f7c3 用真实 Codex 会话证伪并刻意移除的形态（Codex 会渲染成 unknown 联合并丢弃全部 properties）。
- **可做的局部替代**：`lsp-server`（rust-analyzer 的同步 crate）只换传输+JSON-RPC 分帧约 200 行，约 1 天量级、可逆；顺带消除手写分帧的边界问题（newline 分支先整行读进内存再检查 8MB 上限）。低优先级。
- **顺手记录的协议偏离**：`initialize` 原样回显客户端声明的 protocolVersion（规范要求回自己支持的版本）；`notifications/cancelled` 静默丢弃（长耗时 `repository_scan`/`association_rebuild` 无法取消）；`tools/list` 无分页。

### 10. 手写 JSON Schema 与 Rust 结构双份真相

- **背景**：双份是被真实故障逼出来的有据设计（f15f7c3），不是疏忽；可替代的是**漂移风险**，不是"下沉校验"这个决定。
- **候选方案**（建议顺序）：① **先做**：把现有"schema properties ↔ Rust 字段集相等"契约测试从 `task_checkpoint` 一个工具泛化到全部 17 个（`mcp_contract.rs:4332` 已有范本，几乎零风险）；② `jsonschema` crate 让服务端直接按发布的 schema 校验（声明与执行同一份字节，还免费获得 pattern/maxItems 等现在只对宿主生效的约束）；③ schemars 派生+手写展平器（**不建议**——展平器成为新 bug 源，且对外形态被真实宿主行为约束死，不能交给上游）。

### 明确不建议替换（记录在案）

- **宿主 payload 解码器**：Cursor CLI 与桌面版形状不一致且无官方类型定义，"严格 serde + 不加 deny_unknown_fields + 只严校验身份字段 + fixture 契约"是此问题空间的正解。
- **`is_strict_test_runner_command` 的保守拒绝**：换 shell-words 词法切分反而扩大接受面。

## 领域模型与生命周期（来源：领域 C，已 review）

### 11. 自研标识符/路径抽取器（hints.rs）→ 成熟代码索引方案（G4/G5 关键）

- **现状**：手写字符扫描 + 19 个扩展名白名单 + 魔数长度阈值；派生器能力上限钉死在"File + 唯一 basename"——`ArtifactLocator` 六种 kind 一种符号级坐标也产不出（代码注释自认），扩展名白名单还排除了 .c/.cpp/.dart/.rb/.sql 等。
- **候选方案**：① **universal-ctags**（子进程 JSON 输出，安装成本最低，直接填 Symbol 坐标；正则驱动精度参差、引入外部二进制需 doctor 探测+降级）；② **tree-sitter**（纯 Rust 精度最高，用法是先建仓库符号表再拿 Claim token 查表；每语言一个 grammar，体积/编译时间显著增加）；③ **SCIP/LSIF**（跨仓符号定位的工业标准、天然支撑 G5；每语言一个 indexer，对"零配置本地安装"形态过重）。
- **风险**：hints 的两个消费者（Reference 派生与 FTS 投影）必须同一份读数；换实现会改变已 accepted Context 的 hint 集合，需全量 rebuild + 探针基准重算。**最小步**：resolver 抽 trait、hints.rs 留作 fallback。

### 12. 五处独立的手写稳定哈希 + 手工拼 UUID 位

- **现状**：三处直接把 `serde_json::to_vec` 字节当 canonical form（依赖字段顺序永不变这一未断言前提——`ContextRevisionDraft` 新增一个可选字段就会改变全部历史 Candidate 的 content hash）；`from_stable_seed` 两份逐字重复代码手工把 SHA-256 改写成"假装是 v4 的确定性 UUID"（这正是 UUIDv5 的定义）。
- **候选方案**：① **RFC 8785 JCS**（`serde_jcs`，标准答案、改动面小；一次性哈希变更走已有的 schema 丢弃重建先例）；② 确定性 CBOR（引入第二套格式，收益不如①）；③ **`Uuid::new_v5`** 替换两处 from_stable_seed（零新依赖；需放宽 `parse_uuid` 的强制 v4 校验）。
- **注意**：`candidate_submission_content_hash` 会进 Git 事件，**这一处不改**，只改其余四处本地 runtime 哈希。

### 13. 手写 DAG 校验/定点迭代 → petgraph（优先级最低，仅为完整记录）

五处复用的自研图算法（无环检测、head 计算、O(n²) 定点迭代级联失效）可换 `petgraph` 的一次反向遍历；但当前实现正确、事件规模是本地安装级，**不建议现在做**。转换层需保持 BTreeSet 排序稳定（投影确定性有强断言）。

### 明确不建议替换（记录在案）

- **candidate_build outbox**：receipt/Episode 关闭/outbox/SubmissionId 同一 SQLite 事务的 transactional outbox 标准模式正确实现，引入外部消息队列会破坏单文件本地安装形态。

## 检索与联想层（来源：领域 B，已 review）

### 14. 代码 Artifact 抽取：行前缀启发式 → tree-sitter / SCIP（与条目 11 同主题，本层视角）

- **现状**：`scanner.rs` 逐行 `starts_with("struct ")` 式前缀匹配表，只覆盖 4 种代码语言+3 种数据格式（Java/Go/ObjC/C++ 缺席，直接限 G5）；Symbol 的 qualified 坐标是"文件内名字"非真限定符；完全产不出 calls/implements 边。
- **候选方案**：① **tree-sitter**（成熟 binding、query DSL 把抽取规则写成 .scm、纯函数符合确定性约束；grammar 版本必须并入 `RESOLVER_POLICY_VERSION` 与 projection_generation）；② ast-grep（面向搜索/改写非符号索引，收益不大）；③ **SCIP/LSIF**（唯一能同时给限定符号+引用关系的方案，但需在被扫仓库跑构建工具链，与 TD"有界只读扫描"约束正面冲突，不建议近期）。
- **风险**：`EngineeringReference` 是持久事件事实，抽取器换代会让一批历史 Reference 从 resolved 变 missing 且系统明确不做 relocation——**必须新旧双跑出差异报告、人工确认，不能静默切换**；建议先在 Symbol/Test 两类 kind 试点。

### 15. 中文分词：Han bigram → 词典分词器

- **候选方案**：① **jieba-rs**（纯 Rust、`cut_for_search` 双粒度与现有 identifier_split 思路一致；词典版本必须并入 projection_generation 否则违反跨安装确定性契约）；② lindera（中日韩一套、用户词典可直接喂现有 domain_term；词典体积更大）；③ **保留 bigram、只把 answerable 分母下沉到自动路径**（零依赖、改动三处；不解决噪声只解决召回被闸卡死——**投入产出比最高，建议作为前置步骤**，详见 04 文档 B-4）。
- **风险**：换分词器 = 全量 rebuild + token_alias 重算 + 中文探针集与阈值全部重新校准（"必须连带调参"级）。备注：L7 曾评估 jieba 零收益不引入——当时的结论是在 bigram+闸门现状下测的，若先修分母问题再评估，结论可能不同。

### 16. Engineering 投影读取：JSON blob 全量反序列化 → SQLite 原生索引

- **现状**（已核实）：整个投影按 JSON blob 存两张表；每次 task_context 读两次全表+逐行反序列化+全量 validate；Hook 热路径用 `json_extract(...digest)=?` 在只有主键的表上全扫，150ms 预算超时**静默返回空**。
- **候选方案**：① **生成列 + 索引**（`GENERATED ALWAYS AS (json_extract(...)) STORED`，零依赖、schema 4→5 走现有 ensure_schema、O(N)→O(log N)——**优先**）；② read_snapshot 拆成按需查询（单事务内读取反而更强于现在的"读两次比对代际"）；③ 嵌入式图库 cozo/kuzu（仅当 A2A 边落地后遍历失控时再议，当前不建议）。

### 17. DF 统计：每 token 两条 FTS COUNT → `fts5vocab` 虚拟表

- **现状**：中文长 prompt 经 bigram 候选 token 轻易过百，乘 hint 通道重跑，单次 `task_context` 可打出**数百条 FTS COUNT 查询**。
- **方案**：`CREATE VIRTUAL TABLE ... USING fts5vocab(context_fts, 'row')` 一次拿全 term 的 doc 计数。零新依赖。**本层最实在、最低风险的改动点。**

### 18. 停用词表：硬编码 → 语料驱动

- **现状**（已核实）：手写表混入业务专名 `tiktok` 与结构化维度值 `android`/`ios`——同一词在结构化通道是强信号、在文本通道被当噪声，语义自相矛盾（详见 03 文档 B 领域第 4 条）。
- **候选方案**：① `stop-words` crate 换英文部分（中文的"工程语境通用词"通用表替代不了，保留自定义）；② 完全语料驱动（高 DF 门槛降 0、删硬编码表；`AUTOMATIC_MIN_RETAINED_QUERY_TOKENS=8` 已是兜底，风险可控）；③ 配置化（破坏跨安装一致性，**不建议**）。

### 明确不建议整体替换：tantivy（已按任务书要求专项评估）

四个理由：① 本层价值在 BM25 之外——8 个 RRF 通道中三个高权通道来自 Graph/关系，AutomaticTextEligibility 是治理语义非检索语义，换打分器这些全得原样保留；② 每条 FTS 查询必须 join 七条件的 `SAFE_ACCEPTED_CONTEXT_PREDICATE`，独立倒排索引只能先检索再回表过滤，破坏 `total/omitted` 的精确性；③ 多一个必须与 generation 同步的独立存储，破坏"影子表+单事务切换"的原子性；④ 段合并/多线程 indexing 引入非确定性。**窄口子**：只借鉴 tokenizer 层（FTS5 自定义 tokenizer 载体）+ fts5vocab 换 DF（条目 17）。

### 语义向量候选清单（按范围约束仅记录，不推荐）

fastembed-rs（最省事）/ ort + bge-m3/multilingual-e5（可离线打包）；存储侧 **sqlite-vec**（与现有 SQLite 同事务，与本仓形态最契合）/ usearch。核心约束：模型版本须并入 generation；向量天然无 `retrieval_path` 可解释路径，而 TD 明确"无路径解释不得自动注入"——**即便引入也只能进 Explicit 模式或作"补召回+他通道背书"，不能单独过自动注入闸**。

---

## 更新日志

- 2026-09-01 建立骨架，分析任务派发中。
- 2026-09-01 录入领域 F（测试与验收基础设施）2 条候选 + 1 条不建议替换记录；关键声明已抽查核实。
- 2026-09-01 录入领域 E（CLI/安装发布）、A（存储与事件层）、D（协议接入）、C（领域模型）、B（检索与联想）；六领域齐。共 18 个编号条目 + 多条"明确不建议替换"记录；tantivy 与 rmcp 两项按要求专项评估，结论均为不建议整体替换。
