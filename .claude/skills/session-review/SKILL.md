---
name: session-review
description: 分析 tests/scripts/session_replay 生成的会话回放审计 bundle（参数为 bundle id，如 orig-01a08017），产出 review.md 供人工裁定
---

# /session-review $ARGUMENTS

对 `~/.shared-context-audit/$ARGUMENTS/` 下的一个真实（及可选回放）Codex/Cursor 会话 bundle 做证据审计，写出
`~/.shared-context-audit/$ARGUMENTS/review.md`。你是开发者 agent，拥有本仓库全部上下文，但本技能产出的每条结论都是
**建议**，由人类最终裁定接受或拒绝——不要在 review.md 里下命令式结论，不要改代码，不要修任何东西。

核心纪律：**every 结论必须能从 bundle 里逐字引用出处**。任何你无法用一段 ≤200 字符的原文（来自
`original.md` / `replay.md` / `facts.json` / `sctx/*.json`）支撑的判断，一律不写进 review.md，宁可放进「未能判断」一节。
不要转述、不要意译引用；转述不算引用。

## 0. 边界

- 不要读 `~/.codex` 下的原始 rollout；bundle 就是唯一数据源。
- 不要在仓库下写任何文件；只写 `~/.shared-context-audit/$ARGUMENTS/review.md`。
- 不要编造 `facts.json` 里没有的数字。
- 不要把同一个根因在两个「方向」下各报一次——用交叉引用代替重复。

## 1. 读取顺序

1. `~/.shared-context-audit/$ARGUMENTS/facts.json`——先读这个，建立数字基线（injections/pack_deliveries/sctx_calls/pack_usage/sctx_db/sctx_logs/lease 等字段）。
2. `original.md`——**逐轮读全**，不要跳轮。如果文件很大（14 轮的样本约 783 KB），用 `Read` 的 `offset`/`limit`
   分段读，每一段都要覆盖到下一轮的 `## Turn N` 标题，不要因为文件大就只抽样。可以先用 grep 定位
   `^## Turn|^### hook injections|^### pack deliveries|^### sctx calls|^### tools|^## other injections` 这些锚点再分段读，但每一段实际内容都要读完，不能只看锚点。
3. `replay.md`（如果存在）。
4. `sctx/original/candidate_review.json`、`sctx/original/task_injection.json`（以及 `sctx/replay/...` 对应文件，如果存在）；`hook_event.json` 是死表，跳过。
   其余 `sctx/*/*.json` 按需抽查（本技能验证时发现 `checkpoint_operation.json` / `work_episode.json` /
   `agent_checkpoint.json` / `context_usage.json` 在 claims=0 的 no_op 场景下应为空数组——空是预期行为，不是异常）。

## 2. 先读评判依据（Rubric）

在下结论之前，必须先读一遍这些文件，让判断对齐项目自己的定义，而不是你自己的常识：

- **策略层**（什么值得记、正文风格、哪类候选该确认或丢弃）：`crates/local-state/src/default_policy.md` 的四个小节
  （`## session` 中文与绝对日期、`## checkpoint` 值得记/不值得记与 `progress` 节奏、`## stop` 完成标准、`## triage` 处置依据）；
  如果被审计的机器装了自己的策略，以 `~/.shared-context/policy.md` 为准。2026-09-10 起这些规则由运行时下发（marker /
  `task_checkpoint` 描述 / 边界提醒 / 候选处置文本），不再写在 Skill 里，所以评判「该不该记这条」只看策略文件。
- **协议层**：`skills/shared-context/references/workflow.md` 第 1 节（空 claims 是 `no_op`、「宣布不等于做了」）、
  第 3 节（字段集与落库字段规则：`path:line`、标识符原文、Hook 文本/工具输出永远不能变成 Evidence）、
  第 4 节（Session/Intent 设置）、第 5 节（如何使用 Context）、第 6 节（Checkpoint 契约）、第 7 节（候选处置三档的机制）。
- `skills/shared-context/SKILL.md` 的激活规则（哪种 marker 才可信）。
- `docs/adr/0005-policy-gated-agent-disposition.md`、`docs/adr/0006-hook-reminders-need-a-channel-and-an-actionable-recipient.md`
  ——各读一句话结论即可，但要记住 ADR-0006 明确点名了 01a08017 这个会话作为证据来源之一；如果本次审计的 bundle
  正是它，相关发现应标注「与 ADR-0006 已记录的动机一致」而不是当作新发现上报。
- `docs/deferred-issues.md`——通读一遍条目编号和现象摘要。任何发现如果和某个编号的现象吻合，在结论表「已知项」列写编号；
  确实找不到匹配再写「新」。

## 3. 七个分析方向

对每个方向产出零到多条发现，每条都要能回指第 4 节表格里的一行引用。

**送达**：bundle 把两条通道分开记，不要混：`### hook injections`（hook 的 additionalContext 通道，只承载
marker / 提醒 / systemMessage，从不承载 pack；facts 里是 `injections`）和 `### pack deliveries`（Context Pack 以 sctx MCP
工具结果送达；facts 里是 `pack_deliveries`）。先看 marker 是否到达、compaction 后是否重新注入
（`injections.reinjected_after_compaction`）。再看每条 pack delivery 的 `channel`：`native_mcp` 是原生 MCP 调用；
`code_mode_script` 是模型把 MCP 调用包进 exec 脚本再回显，这条通道有宿主上限（`channel_cap_bytes`，Codex 约 40,000 字节 /
10,000 近似 token），`delivered_bytes` 超过它就是被宿主截断（`truncated=true`，`original_token_count` 是原文大小）；
`delivered_bytes` 极小而 `truncated=false` 则是模型自己用 `.then(r=>({isError:...}))` 之类的包装把结果丢掉了，这是另一种失效模式，
digest 的 `### tools` 下会引用 `discard_wrapper` 命中的脚本片段，引用它而不是推断。`pack_deliveries.over_channel_cap_count`
是全会话越界次数。写结论时明确说是「pack 在脚本通道被截断」还是「hook 注入被截断」，后者在现有数据里从未发生过。
同一轮是否还有其他注入在抢占同一个 slot（`## other injections`）？

**激活与依从**：边界处（轮次开始/结束、compaction 前）是否有值得留的 checkpoint？是否出现空 claims 或单次调用
claims>5？assistant 文本里是否有「已记录/已保存」这类措辞但同一轮没有 sctx 调用（宣布但未做）？
`sctx_db.checkpoint_reminder_count` 是否 >0 而同一轮/同一 session 之后没有对应的 `task_checkpoint` 调用？

**结论合规**：对每个已提交的 claim（`task_checkpoint` 调用的 `arguments.claims`，digest 里是 FULL 的，不是摘要），
按 `default_policy.md` 的 `## checkpoint` 判断它属于「值得记」的类型还是排除项（过程性理解、代码/git 已有、重复）；
再按 `## session` 的正文风格（中文、绝对日期）与 workflow.md 第 3 节的字段规则（`path:line`、标识符原文、Evidence 来源）
检查 `statement`/`rationale`/`conditions`/`evidence`。如果本次会话所有
`task_checkpoint` 调用的 `claims` 都是空数组，直接说「本次会话没有产出可评判结论合规性的 claim」，不要编造。

**召回相关性**：对每个被注入的 pack（`text` 字段是 FULL 的），判断其中哪些 item 和该轮用户 prompt 相关
（同路径/同符号/同主题）；最相关的那条有没有被引用或据此行动（`facts.json.pack_usage.ctx_ids_cited_in_assistant`
和 `ctx_ids_cited_in_sctx_calls`，以及 assistant 文本里手动核对是否出现 `ctx_` id 或明显对应内容）？有没有你根据
项目知识认为语料库里明显存在、但没被召回的东西？**这是唯一允许你使用自己项目知识的方向**，凡是这类判断，必须在
句首标注「基于项目知识」，和纯粹从 bundle 摘取的证据区分开。

**候选处置**：`candidate_list`/`candidate_discard`/`candidate_confirm` 调用（在 `### sctx calls` 里）对照
workflow.md 第 7 节的三档机制与 `default_policy.md` 的 `## triage` 处置依据；`sctx/original/candidate_review.json` 每行的
`top_relation` 分布、`decision_source`。
如果 `candidate_list` 全程返回空 `reviews` 且 `candidate_review.json` 是空数组，说明本 session 范围内没有可处置的候选，
如实报告「无候选可处置」而不是缺失项。

**留痕**：真正的留痕在 facts 的 `sctx_logs` 块（来自 `~/.shared-context-logs/state/hook-diagnostics.json`，已按
session_digest 过滤到本会话）：看事件序列是否完整（session_start → prompt_submit → post_tool_use… → turn_stop，
会话结束应有 session_end），有没有 fail_open / undecodable / reason ≠ ok 的事件，`logs_sync_failure` 与
`spool_ready_batch_count` 是否指示遥测积压。`lease` 块给出租约文件（`state/authorized-session-scopes/scope-<digest>.json`）
是否仍存在：会话早已结束而 `lease_file_exists=true` 且 sctx_logs 里没有 session_end，就是 SessionEnd 未清理的直接证据。
`sctx/original/hook_event.json` 是一张只建不写的死表（全树没有写入），为空不是异常，不要据此下结论，也不要拿它
和 `checkpoint_reminder_count` 对照。`diagnostics_file_found=false` 表示该侧根本没有诊断文件（回放侧早期就是如此），
按「未能判断」处理而不是「零事件」。同一秒内重复的 session_start、`authorization_internal` 突发在 sctx_logs 的事件序列里看。
如果一条留痕/依从发现的根因能在 `docs/adr/000X-*.md` 里找到且该 ADR 的 frontmatter `status` 是 `accepted`，说明这是
已被修复的历史行为：把「已知项」写成该 ADR 编号，并在小节正文里明说「这是对已修复问题的历史证据复现，不代表当前代码仍有此缺陷」，
严重度按它在录制当时造成的影响标注，不要因为已修复就直接标「观察」抹平它，也不要因为影响大就让人误以为现在仍会发生。

**原/回放对照**（仅当 `replay.md` 存在时做）：逐轮比较，回放是否偏离原始到让下一轮原始用户 prompt 变得不连贯
（例如用户说「这不是根因」，但回放里的 agent 从没说过那个根因）？把这类轮次标「失真」，并区分「版本变化导致的差异」
和「回放漂移导致的差异」——前者是宿主/模型版本不同造成的正常噪声，后者才是回放机制本身的缺陷。

## 4. 输出结构：`review.md`

写入 `~/.shared-context-audit/$ARGUMENTS/review.md`，固定结构如下：

1. **Header**：bundle id、thread id（原始/回放）、`facts.json`/manifest 里的 sctx 版本与 Codex/Cursor
   `cli_version`、轮数、一段话摘要（这段摘要本身也不能超出下面表格能支撑的范围）。真实会话（无 `--replay-id`）
   产出的 bundle 通常没有 `manifest.json`，`facts.json` 和 `sctx/*.json` 也可能不携带 sctx 自身版本号——找不到就
   在 Header 里写「未能判断」，只报告确实能读到的 `cli_version`，不要因为字段名相近就把 sctx 版本和宿主 CLI 版本混写成一个数字。
2. 一张表：

   `| # | 方向 | 结论 | 引用 | 严重度 | 已知项 |`

   - `引用`：来自 `original.md`/`replay.md`/`facts.json`/`sctx/*.json` 的逐字引用（≤200 字符），标明轮次
     （例如 `Turn 1`、`facts.json.original.injections`）。
   - `严重度` ∈ `阻断` / `重要` / `次要` / `观察`。
   - `已知项`：命中 `docs/deferred-issues.md` 的编号，或命中 ADR-0005/0006 的一句话引用，都填不上就写「新」。
3. 每条发现一个短小节（2–4 句）：发生了什么、依据 workflow.md（协议）或 `default_policy.md`（策略）哪条规则、哪个 ADR
   判断它重要、建议下一步核实什么。
   这一节里每句话都要能追到第 2 步的表格行,不要引入表格之外的新事实。
4. 「未能判断」一节：列出这个 bundle 结构性做不到的判断，例如 Codex 的 reasoning 是加密的、
   某一侧 `diagnostics_file_found=false` 导致留痕方向无法核实、Cursor bundle 缺少 tool 输出且 pack 送达是从 sctx 侧
   反推的（`channel=unknown`）、原会话的 sctx 版本只能取 bundle 时的安装版本等（按实际遇到的情况写，不要照抄这个例子列表）。
5. 每句结论前或段落开头用「建议：」标出，提醒这是建议不是定论；不给任何修复方案或代码改动建议，只给「建议人工核实
   XX」这类下一步。

## 5. 明确的禁止项

- 不编造 `facts.json` 里没有的数字。
- 不转述引用，只逐字摘录。
- 不把同一根因在两个方向下各报一遍，用「见方向 X 的第 N 条」交叉引用。
- 不读 `~/.codex` 下的原始 rollout。
- 不在仓库任何路径下写文件。
- 不给出修复建议或代码改动，只给人类要核实什么。
