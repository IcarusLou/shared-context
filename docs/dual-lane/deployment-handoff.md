# 部署交接(2026-09-12 迭代收官)

本轮迭代自 0e54982(双径合并门)后共 17 个 commit,HEAD `815eae9`。全部通过:workspace 1109/0(15 ignored=真模型套件,见手动清单)、clippy `-D warnings` 零告警、真模型套件重基线三次一致、红线(candidate 语义/search_contract/显式 search)零回退。部署由操作者执行;本文件是交接面。

## 1. 本轮包含什么(按 commit 组)

- **a988142** build 指纹:`sctx --version` → `0.2.0-dev.9 (<commit>, clean|dirty)`;遥测 program_version 同源。
- **c4f1134 / c293254** 送达链:close 型 Checkpoint 后有新活动即恢复 TurnStop 提醒资格(cap=3 语义不变,计数每次 Checkpoint 重置;`## stop` 策略段通道随之复活);自愈 marker 按值确认送达后才记账。
- **a66d8cb** 观测面:上传/维护失败保留工具原话(本地诊断 3 行/240 字符)、`first_failure_unix_ms`、maintain 日志行带 reasons。
- **e59eb88** `docs/dual-lane/wording-evidence.md`:12 条指引措辞在两个真实长会话中的服从/无视实测(policy.md 内容创作的输入)。
- **dafec34 / 07c4df5 / 4d07425 / 0c21073** 供给侧:扩展名白名单补实证六种(含绝对路径守卫修复)、Claim 以类型名点名文件(符号级派生,capped)、歧义派生答案变化时重看(schema 19→20)、定义任务的首条 Prompt 暂存回补(20→21,斜杠命令不占槽)。
- **9cfc7fe / 6373f71** 候选侧(B1 冻结例外):safe 位对齐 `blocks_automatic_injection` + 按治理 revision 键控(38 份真实 analysis 重放:重复评估行 44→0、安全诊断 71→40);五集合通道按度量赋秩。
- **0376cba** `last_injected_at`(21→22),重推不再改写首次注入时间,判决不被覆盖。
- **7f3d6e4** Confirmation 后新 revision 自动入向量回填队列。
- **2f2a834** pack 顶层 `retrieval_paths` 逐字节重复删除(占 wire ~25% 且不计费,双径栈同病已除)。
- **38b2be6** 查询侧语义正式退役(ADR-0007 修正案:那条路生产从未有调用者);配置键 `embedding_encode_budget_ms` 仍解析但失效。
- **815eae9** 真模型套件重基线:F2LLM 探针臂改测文档空间分离度(worst_positive 4081 / best_noise 1980 / margin 2101,棘轮 4080/1990/5210/4090);bge 臂变为换模对比臂;`DEVELOPMENT.md` 增真模型套件手动清单。

## 2. 运维清单(部署时照单执行;A–B 与新二进制无关,可先做)

### A · 日志上传(22 连败的自愈路径)
1. `cat ~/.shared-context-logs/state/upload-status.json` — 看 `consecutive_retryable_failures` / `last_error_stage` / `next_retry_unix_ms`。
2. 手动复跑看真实 stderr:`~/.shared-context/bin/current/sctx logs sync --logs-root ~/.shared-context-logs --json`(**不要加 `--scheduled`**,会被 1h 闸拦下什么都看不到)。
3. 连通性(只读):`git -C ~/.shared-context-logs/repository ls-remote --exit-code --heads origin`(诊断时已通,根因大概率已消失)。
4. 卡住的 journal(`phase: "committed"`, `receipt_durable: false`):一次成功 verify 应自行推进;仍失败才需要人工裁定是否弃单。
5. 积压:`ls ~/.shared-context-logs/spool/ready | wc -l`(诊断时 185)。
6. 要抓守护进程 stderr:把 `~/Library/LaunchAgents/com.shared-context.logs-sync.plist` 的 StandardOut/ErrorPath 指向文件后 `launchctl kickstart -k gui/$UID/com.shared-context.logs-sync`。
7. **不要**用 `launchctl list` 判断健康——22 连败期间它始终报 exit 0。

### B · maintain
1. `cat ~/.shared-context/state/maintain-digest.json` 看两条失败 reason。
2. 手动:`sctx knowledge sync` / `sctx maintain run` 取真实原因。
3. 未推送知识 commit:`git -C ~/.shared-context/repository log --oneline origin/main..main`(诊断时 3 条,确认同步后落远端)。

### C · 新二进制安装后
- `sctx --version` 应带 commit;upload-status 出现 `last_error_diagnostic`/`first_failure_unix_ms`;maintain 失败带子进程原话;日志行带 `reasons=`。

### D · 留给操作者裁定的两个策略点(本轮刻意未改)
- 退避梯 `[1,5,15,60]min` 封顶 1h,连败不升级、不置 `needs_human`、doctor 不变红。
- maintain 调 `logs sync` 不带 `--scheduled`,绕过退避闸(每次 maintain 强制一次尝试——有恢复价值,但会抬高连败计数)。

## 3. 部署后冒烟建议(U-002 惯例:Cursor+Codex 配对)

各一条真实会话,验收点:
1. 自动注入的 why 应为 `anchored:`/`associated via`,不再有 Matched/coverage 文案;无锚点任务收到空包(`no_lane_evidence`)而非填满。
2. close 型 Checkpoint 之后继续工作 ≥1 个 TurnStop:提醒应再次出现(附 policy `## stop` 段,如已配置 policy.md)。
3. 会话结束:`context_usage` 未判条目落 `session_close`;租约无残留;上传/维护 reason 可读。
4. Confirmation 后:`sqlite3 semantic.sqlite 'select count(*) from revision_vector'` 应含新 revision(回填队列生效)。

## 4. F2LLM vs arctic 选型对比菜单(权重下载为操作者决策)

正确基准=文档空间三套件(查询空间旧数字全部作废):
- hop2 校准:`SCTX_PROBE_F2LLM_MODEL=<dir> SCTX_PROBE_EMBEDDING_RUNTIME=<dylib> cargo test --release -p sctx-search --features embedding-onnx --test embedding_hop2_admission_calibration -- --ignored`(指标:干净带宽度/AUC/13/47 保留)。
- hard-negative 棘轮:`cargo test --release -p sctx-search --lib -- --ignored hop2_ratchet`(13/18/0)。
- 探针分离度(换模臂):`SCTX_PROBE_EMBEDDING_MODEL=<dir> ... cargo test --release -p sctx-cli --test association_probe_ext_semantic -- --ignored`(打印全表,断言仅"两分布不重叠";F2LLM 臂同表可逐字节对照)。

## 5. 待决与新账(deferred #47–#55 摘要)

#47 hook_fail_open 并发 flake;#48 隐私金丝雀替换的恢复条件;#49 符号派生无真实流量基线;#50 active_signals 不计费(有实测数字,涉 token_budget 语义,留待裁定);#51 EngineeringGraphSnapshot 清理面(随 #42/B1);#52 encode+SessionGate 单调用者保留;#53(G2b 前已有);#54 真模型套件门覆盖缺口(清单为人依赖的缓解);#55 DEVELOPMENT.md §当前实现边界 五条查询通道旧文未除。另:`EmbeddingProvider::encode_bulk` trait 文档"与 encode 向量一致"的契约声明对 Qwen3 已不真——是设计声明该不该改的判断,留操作者裁定。
