# 措辞实效实测(两个真实长会话)

本文只记录**实测**:每一段 Shared Context 送达给模型的指引文字、送达次数与时刻、其后模型实际做了什么、间隔多久。不含策略建议,不代拟 `policy.md` 内容。写作口径:能测的给数字与出处,不能测的明说不能测。

数据源:

- **样本 1 — codex `01a08baf`**:`~/.shared-context-audit/orig-01a08baf/`(`original.md` 会话摘要、`facts.json` 基准、`sctx/original/*.json` 库转储)。
- **样本 2 — cursor `2d5ab8ea`**:无 bundle,用原始源:`~/.cursor/projects/…/agent-transcripts/2d5ab8ea-….jsonl`、`~/.adk-mobile-mcp/logs/TikTok_M0EczR5l/cursor-hooks-2d5ab8ea-….jsonl`、`~/.shared-context-logs/state/hook-diagnostics.json`(按 `session_digest bb34ddb57f59…` 过滤)、`/private/tmp/sctx-probe-2d5ab8ea/runtime.sqlite`。

时间除非标注 `(+08)` 均为 UTC。

## 1 · 会话基本面

| | 样本 1 · codex `01a08baf` | 样本 2 · cursor `2d5ab8ea` |
|---|---|---|
| 宿主 | Codex Desktop,`codex_cli_version` 0.153.4 | Cursor CLI `2026.09.10-fd3934a`,`claude-opus-5-thinking-xhigh` |
| sctx | `sctx 0.2.0-dev.9` | 同一装置 |
| 仓库/分支 | TikTok Android,`feat/map-refactor` | 同仓同分支 |
| 时长 | 2026-09-10T14:18:30Z → 2026-09-11T07:25:11Z,**17h06m41s** | hook `session:end` 报 `duration_ms 78,575,162`,**21h49m35s** |
| 人类 prompt | 3 | 12(含 `/sctx-review`、`/statusline` 两条斜杠命令) |
| 工具调用 | **603**(exec 413、followup_task 88、send_message 81…) | **474**(Shell 168、StrReplace 89、Read 56、Write 17、CallDynamicTool 12、GetDynamicTools 7…) |
| `git commit` 次数 | 25(部分由 sub-agent 在同分支发出) | 16(`/sctx-review` 那一轮之前 5、之后 11) |
| sctx MCP 调用 | **5,全是 `task_intent_update`**;`checkpoint_calls: 0`,`claims_total: 0` | **12**:`task_intent_update`×1、`task_checkpoint`×1、`candidate_list`×5、`task_context`×1、`space_list`×1、`space_create`×1、`candidate_get`×1、`candidate_confirm`×1、`candidate_discard`×1 |
| 压缩次数 | 2(19:08:00.074Z、04:04:54.288Z) | 0 |
| hook 运行 | session_start 3、turn_stop 3、pre_compact 6、prompt_submit 129、post_tool_use 2036 | session_start 1、prompt_submit 12、post_tool_use 498、turn_stop 11、pre_compact 0 |
| Checkpoint | **0** | **1**(`ckp_a2f275ad`,4 claims + 3 unknowns,episode `wep_161298aa` closed) |
| `checkpoint_reminder_count` / `activity_since_checkpoint_reminder` | **3 / 1794** | **0 / 429** |
| Intent 修订 | 5 | 1 |
| 注入 pack | 5 个,18 个 `ctx_id`,3/5 在 40KB `code_mode_script` 上限被截断 | 9 条 `task_injection` |

## 2 · 措辞台账

### W1 · 激活 marker(SessionStart / 压缩后)

> `<shared-context-active external_session_id="01a08baf-…">Shared Context is authorized for this session. Before substantive work, call task_intent_update with agent_kind "codex" and external_session_id "01a08baf-…" (copy it verbatim; never invent one).</shared-context-active>`

**codex:送达 3 次(`role: developer` / `hooks.additional_context`),3 次全被服从。**

| # | 送达 | 其后首个 sctx 动作 | 间隔 |
|---|---|---|---|
| 1 | 14:19:46.517 | 工具名过滤 14:19:59.581 → `task_intent_update` 14:20:14.041 | **+27.5s** |
| 2 | 19:08:01.263(压缩后) | 19:08:05.949 → 调用 19:08:24.781 | **+23.5s** |
| 3 | 04:04:56.005(压缩后) | 04:05:03.971 → 调用 04:05:20.404 | **+24.4s** |

`external_session_id` 3/3 逐字复制,0 次编造(`facts.json: external_session_id_verbatim: true`)。另有 2 次 `task_intent_update` 由人类 prompt 触发(+71.5s、+38.9s),合计 5/5。

**cursor:marker 送达不可测。** 590KB transcript 中 `shared-context-active` 只出现 1 次,且是人工附加的 SKILL.md 里的**引用示例**(17:33 +08),不是 hook 消息。但 sctx `session_start` hook 确实在 07:51:37.429Z 运行成功并授权,且 agent 自己在第 3 条记录里写"我应该先注册 task intent,**因为 hook 要求**"——说明有文字到达了 transcript 不记录的通道。prompt(07:53:30Z)→ `task_intent_update` 起调(≈07:55:26Z)≈ **1m56s**。判定:**被服从;措辞不可测**(见 N1)。

### W2 · Intent bootstrap 提示(PostToolUse,每会话一次)

> `Shared Context: no ActiveTask exists. Call task_intent_update for this substantive task before continuing.`

**codex:送达 1 次,14:20:01.151;`task_intent_update` 在 14:20:14.041,+12.89s。** 注意它落在 agent 已经开始查工具(14:19:59.581)之后,所以不是唯一成因,但是调用前的最后一条指令。判定:**被服从,12.9 秒**。

**cursor:transcript 中 `no ActiveTask` 出现 0 次**,不可测(N1、N9)。

> 原假设"bootstrap nudge 23s 内被服从"应修正为:**23s 是 W1 marker 的延迟,W2 bootstrap 的延迟是 12.9s**。两者在同一分钟内先后送达。

### W3 · TurnStop / PreCompact 的 checkpoint 催促 —— 决定性发现

服务端文案(本会话所处分支:有 ActiveTask、无开放 Episode):

> `Shared Context TurnStop: no Work Episode is open for Task {task_id}. Use $shared-context and call task_checkpoint with complete direct Claims/Unknowns; the server resolves the current Task, Intent, and lifecycle. Hook text is not Claim evidence.`

**codex:服务端发出 3 条(打满 `CHECKPOINT_REMINDER_LIMIT = 3`),模型侧 0 条可见。**

三次 turn_stop hook 在 14:46:33.035、15:02:14.685、15:10:55.017。对 23.5MB rollout 全文 grep:`"TurnStop"` → **0**,`"no Work Episode is open"` → **0**,`"checkpoint still pending"` → **0**,`"Candidate Builder is"` → **0**(仅有的 15 处 `PreCompact` 全部来自 agent 自己 `cat` 的 SKILL.md/workflow.md 正文与两条 `compacted` 记录)。6 次 pre_compact hook(代码上**不受节流**)同样产出 0 条可见文字。

结构性伴随事实:三次 turn_stop 全部落在**前 51 分钟**内(14:46→15:11);其后 **16h14m 再无任何 turn_stop 事件**——Codex Desktop 的 goal 循环/多 agent 模式没有再回到 stop 边界。`activity_since_checkpoint_reminder` 由此累积到 **1794**,而闸门再未被咨询过。

**cursor:11 次 turn_stop 全部成功,`checkpoint_reminder_count` 仍是 0。** 原因可测:唯一的 checkpoint 落在 **08:52:53Z**,而**第一次 turn_stop 在 08:54:34Z**——此后每一个 stop 边界看到的 Work Episode 都已 `closed`,催促分支(及其闸门)一次都没进入。

### W4 · 节流后的替代文案

> `Shared Context TurnStop: checkpoint still pending; this Session already received its automated reminders for it.`

**两样本均 0 次送达**(grep 计数 0)。codex 侧预算虽被耗尽,但两类文案都没到模型;cursor 侧分支根本没进入。判定:**未被检验**。

### W5 · `task_intent_update` 工具描述

5 次送达(每次调用前各一次):

> "…Write goal, current_direction and in_scope **in Chinese**, the knowledge base language, keeping identifiers, paths, commands and error codes in their original spelling. current_direction and in_scope also feed the retrieval query, so fill them whenever you already know them; never invent either one. external_session_id: copy verbatim from the `<shared-context-active>` marker …"

判定:**被服从**。codex 5 次 intent 与 cursor 1 次 intent 全部中文书写、标识符保留原样,`current_direction` 与 `in_scope` 5/5 + 1/1 填写。该描述**只提 intent**,不提 checkpoint,不提生命周期。

### W6 · `task_checkpoint` 工具描述 —— 推翻"拉工具即 checkpoint"

原文(rollout `custom_tool_call_output` L389 / L2765):

> "Finalize one content-addressed Agent Checkpoint for the current ActiveTask and Intent. … **Checkpoint only what is worth keeping: decisions and their reasons, contracts, verified conclusions, counter-intuitive findings, and summaries of a newly understood mechanism. Process-level understanding of code you just read is not a Claim.** Once the ACK reports a queued Candidate Build, call candidate_list next and dispose every Pending Review under its triage policy."

**codex:完整描述拉取 2 次,调用 0 次。**

| 拉取 | 触发 | 其后 |
|---|---|---|
| 14:39:44.878(`ALL_TOOLS.filter(/task_checkpoint\|candidate_list\|candidate_confirm/)`,描述 14:39:49.772 返回完整) | 刚 `git show --stat` 完一个新 commit | 14:40:07 转去"Checking code and documentation",14:46:33 本轮结束,**无 checkpoint** |
| 19:20:44.909(描述 19:20:47.144 返回) | 读完 Lighten SDK 源码 | 19:20:59 回到 `rg --files`,**无 checkpoint**,其后 12h 也没有 |

**cursor:拉取 1 次,调用 1 次,相邻两条记录。** 记录 105 `GetDynamicTools{toolName: "task_checkpoint"}` → 记录 106 `CallDynamicTool{task_checkpoint}`,4 claims / 3 unknowns,请求体键恰为 `agent_kind, claims, external_session_id, unknowns`,无多余字段。

> 假设"GetDynamicTools 拉取后 checkpoint 才发生"判定:**必要、不充分**。codex 相隔 4h41m 两次读到完整描述,17h / 603 次工具调用下 `checkpoint_calls: 0`。

### W7 · `candidate_list` 描述

codex 侧 14:39:49、19:20:47 各送达一次,**从未调用**。

cursor 侧 `candidate_list` 是唯一**没有**先 `GetDynamicTools` 就调用的 sctx 工具,而它的首次调用(记录 107,08:53:07Z)`arguments: {}` 为空并失败(`result_json._len` 221,20 秒后重试 4638 成功)。同样未拉描述的 `candidate_discard` 却一次成功——单次观测,不构成规律。

### W8 · `shared-context` SKILL.md 的门禁句

> "If the trusted marker is present, read references/workflow.md completely once per context window (again after a compaction, when the PreCompact marker reappears), then follow it."

**codex:被服从,读了 4 次**——14:20:14.041、14:20:26.196(首次 `exec` 脚本被截断重来)、19:08:05.949(压缩 #1 后立即)、04:05:03.971(压缩 #2 后立即)。"压缩后再读一次"2/2 服从。

**cursor:被无视。** `workflow.md` 在整个 21.8h transcript 中出现 **0 次**,agent 从未读过 shared-context SKILL.md 或其 reference——却仍然产出了一次结构完好的 4-claim checkpoint。判定:**cursor 侧协议形态行为的驱动者不是 workflow 参考,是工具描述**。

### W9 · "announcing is not making one"

> "…empty `claims` and `unknowns` are the `no_op` of section 6 … so never send one to look compliant. The opposite lapse costs more: **announcing a checkpoint is not making one, so when you tell the user you are recording something, call `task_checkpoint` in the same turn.**"(workflow.md §1)

**codex:随 4 次 workflow.md 读取送达 4 次,而它没有可作用的对象。** 对全部 372 条 assistant/agent 消息扫 `checkpoint|记录|沉淀|归档`:8 处命中,无一宣告 Shared Context checkpoint(中文命中是团队自己的验收记录文档与 sub-agent 的 commit 汇报)。实测:`checkpoint_empty_claims: 0`、`claims_total: 0`、`checkpoint_calls: 0`,即 **0 次空/宣告式调用,也 0 次真调用**。

**cursor:该措辞从未送达**(workflow.md 没读),"宣告→真调用"却发生了:记录 100"我将验证 RemoteX 构建、**记录 shared-context checkpoint**、更新 README"→ 记录 102"让我更新仓内状态文档并**记录一个 checkpoint**"→ 记录 104 `git commit` → 记录 105–106 真正的 `task_checkpoint`。

> 判定:**两个方向都不可测**。送达 4 次的样本里无可抑制对象;未送达的样本里宣告后有真调用。见 N5。

### W10 · "build on it, cite it by `context_id`"

随 workflow.md 送达 codex 4 次;5 个 pack 共注入 18 个 `ctx_id`。实测 `ctx_ids_cited_in_assistant: 0`、`ctx_ids_cited_in_sctx_calls: 0`。判定:**送 18 引 0,被无视**。需同时记下送达通道本身是降级的:5 个 pack 中 3 个在 40KB `code_mode_script` 上限被截断(产出 31 / 42.5 / 43 / 44 / 44.5 KB,实际送达 40,154 / 11,405 / 40,153 / 28,782 / 40,154 字节)。

### W11 · `sctx-review` SKILL.md(cursor,用户手动附加)

17:33 (+08) 由 `/sctx-review` 内联进 transcript(记录 111)。指令要求:只信任 hook 提供的 marker;"If the trusted marker is present, read references/review.md completely once per context window, then follow it";并定义三档处置与"其余一律升级……以紧凑表格呈现,然后执行用户的决定"。

其后序列(+08):记录 112 ≈17:33"本会话有可信 marker,正在读治理参考"→ `Read references/review.md` → `task_context` 17:34:58 → `candidate_list` 17:35:20 → `space_list` 17:36:06 → `space_create` 17:45:20 → `candidate_get` 17:45:23 → `candidate_confirm` 17:45:57 → `candidate_discard` → `candidate_list` 17:46:45(空)。库内结果:`candidate_review` 3 confirmed / 1 discarded。

判定:**端到端被服从,从 skill 送达到队列清空约 14 分钟**。这是两个会话中唯一一次完整跑完 §7 处置闭环。

### W12 · 团队策略 `## session` / `## stop` 段

本装置 `~/.shared-context/policy.md` **不存在**。对应地:

- 14:19:46.517 送达的 marker 与"无策略"形态逐字节一致(326B),未附加 `## session` 句;
- 14:39:49 送达的 `task_checkpoint` 描述带的是内置的 "Checkpoint only what is worth keeping…",不是 `crates/local-state/src/default_policy.md` 的 `## checkpoint` 文本;
- `## stop` 只搭在"正在索要 Checkpoint"的提醒上,而两个样本都没有这样的提醒送达。

判定:**两样本策略通道送达 0 次,策略措辞在此没有任何服从证据**。

## 3 · 反面结论(不成立/不可测)

- **N1 · cursor 的 hook 注入不进 transcript,marker 措辞在 cursor 侧不可测。** 本机 628 个 cursor agent-transcript 文件里 `shared-context-active` 只出现在 2 个文件中,且都不是 hook 消息。`agent-transcripts/*.jsonl` 只存 `role`/`message.content`,不存 `tool_result` 与注入的附加上下文。cursor 侧只能测"agent 做了什么",测不了"收到了什么措辞"。
- **N2 · "cursor 21.8h 会话从未 checkpoint"不成立。** 发生过一次:`ckp_a2f275ad`,episode `wep_161298aa` 已 closed,4 claims + 3 unknowns,≈08:52:53Z,即**开场 59 分 23 秒**。准确说法是**尾部**:09:47:11Z 之后剩余 **≈19h55m** 内 Shared Context 交互为 0,其间有 5 条人类 prompt 与 11 次 git commit。
- **N3 · "marker 只提 intent"成立,且有下游可测后果。** 326 字节的 marker 只点名 `task_intent_update`。codex 在轮次/压缩边界上的 5 次工具发现探测全是 `ALL_TOOLS.filter(x=>/task_intent_update/.test(x.name))`——正好只过滤 marker 点名的那一个工具(14:19:59、15:11:11、19:08:05、01:17:10、04:05:03)。唯二包含 `task_checkpoint` 的宽探测(14:39:44、19:20:44)都是自发的、发生在轮次中段。
- **N4 · "拉取动态工具清单后才会 checkpoint":作为充分条件被推翻,作为必要条件被支持。** cursor 1 拉 1 调,相邻;codex 2 次完整拉取、17h 内 0 调用。必要性在两样本均成立(唯二例外是 cursor 的 `candidate_discard` 未拉描述而成功、`candidate_list` 未拉描述而首调失败)。
- **N5 · "'announcing is not making one' 抑制了空调用但没带来真调用":两样本均不可判。** 见 W9。
- **N6 · codex 的 `reminder_count=3` / `activity=1794` 不是节流现象。** 三条提醒集中在前 51 分钟发出且都没到模型;计数冻结的原因是**其后 16h14m 再无 turn_stop hook**。1794 是第三条提醒之后的 PostToolUse 计数,不是"被无视的建议数"。
- **N7 · sctx 自身 `hook_event` 表对两样本都是空的。** 它停在 2026-09-07T14:57:51(codex)/ 2026-09-03T11:35:39(cursor),两个会话都晚于此。可用替代是 `hook_diagnostics_matched_events.json`(codex,2177 条)与 `~/.shared-context-logs/state/hook-diagnostics.json`(cursor,522 条)。
- **N8 · `adk-mobile-mcp` 的 cursor hook 日志漏记 MCP 事件。** `task_checkpoint`(transcript 记录 106 与库内行双重佐证)与 `candidate_discard`(`candidate_review.status = discarded` 佐证)都没有对应 `tool:mcp` 条目。sctx 调用计数必须取自 transcript。
- **N9 · cursor 会话今天已无 `authorized_session_scope` 文件**(`grep -r 2d5ab8ea ~/.shared-context/` 无结果),因此该样本的 `intent_bootstrap_notified` 不可恢复:hook 诊断能证明 session_start 运行并授权,但 bootstrap 提示是否发出无从判定。
- **N10 · 两样本的改动行数都不可测。** digest 与 transcript 都不带可归属到单会话的 diffstat;可直接观测的只有 `git commit` 调用次数(codex 25 / cursor 16),而 codex 的若干次由同分支上的 sub-agent 发出。
