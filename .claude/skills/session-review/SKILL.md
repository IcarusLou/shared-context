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

1. `~/.shared-context-audit/$ARGUMENTS/facts.json`——先读这个，建立数字基线（injections/sctx_calls/pack_usage/sctx_db 等字段）。
2. `original.md`——**逐轮读全**，不要跳轮。如果文件很大（14 轮的样本约 783 KB），用 `Read` 的 `offset`/`limit`
   分段读，每一段都要覆盖到下一轮的 `## Turn N` 标题，不要因为文件大就只抽样。可以先用 grep 定位
   `^## Turn|^### injections|^### sctx calls|^### tools|^## other injections` 这些锚点再分段读，但每一段实际内容都要读完，不能只看锚点。
3. `replay.md`（如果存在）。
4. `sctx/original/hook_event.json`、`sctx/original/candidate_review.json`（以及 `sctx/replay/...` 对应文件，如果存在）。
   其余 `sctx/*/*.json` 按需抽查（本技能验证时发现 `checkpoint_operation.json` / `work_episode.json` /
   `agent_checkpoint.json` / `context_usage.json` 在 claims=0 的 no_op 场景下应为空数组——空是预期行为，不是异常）。

## 2. 先读评判依据（Rubric）

在下结论之前，必须先读一遍这些文件，让判断对齐项目自己的定义，而不是你自己的常识：

- `skills/shared-context/references/workflow.md` 第 1 节（值得记什么，含排除项、「空调用不算合规」「宣布不等于做了」）、
  第 3 节（落库文本规则：中文、绝对日期、`path:line`、Hook 文本/工具输出永远不能变成 Evidence）、
  第 5 节（如何使用 Context）、第 6 节（Checkpoint 契约）、第 7 节（候选处置三档）。
- `skills/shared-context/SKILL.md` 的激活规则（哪种 marker 才可信）。
- `docs/adr/0005-policy-gated-agent-disposition.md`、`docs/adr/0006-hook-reminders-need-a-channel-and-an-actionable-recipient.md`
  ——各读一句话结论即可，但要记住 ADR-0006 明确点名了 01a08017 这个会话作为证据来源之一；如果本次审计的 bundle
  正是它，相关发现应标注「与 ADR-0006 已记录的动机一致」而不是当作新发现上报。
- `docs/deferred-issues.md`——通读一遍条目编号和现象摘要。任何发现如果和某个编号的现象吻合，在结论表「已知项」列写编号；
  确实找不到匹配再写「新」。

## 3. 七个分析方向

对每个方向产出零到多条发现，每条都要能回指第 4 节表格里的一行引用。

**送达**：marker/pack 是否真的到达模型？compaction 后是否重新注入
（`facts.json` 的 `injections.reinjected_after_compaction`）？是被宿主截断
（`injections.truncated_count`，`original.md` 里对应轮次 `### injections` 行的 `truncated=YES`）还是被模型自己丢弃
（`wire_bytes` 很小但 `truncated=no`——这是和截断不同的失败模式，两者都要在 `### injections` 摘要行里找到字面依据）？
`tools` 列表里 exec 脚本的 `args_head` 只有前 160 字符，`.then(r=>({isError:...}))` 这类丢弃包装脚本的完整文本
**通常不在 digest 里**（被截断的 head 里可能看不到），这种情况下只能用 `wire_bytes`/`truncated` 的数字组合做间接证据，
在结论里如实说明「基于 wire_bytes 与 truncated 标志推断，未见丢弃脚本原文」，不要假装看到了原文。
同一轮是否还有其他注入在抢占同一个 slot（`## other injections`）？

**激活与依从**：边界处（轮次开始/结束、compaction 前）是否有值得留的 checkpoint？是否出现空 claims 或单次调用
claims>5？assistant 文本里是否有「已记录/已保存」这类措辞但同一轮没有 sctx 调用（宣布但未做）？
`sctx_db.checkpoint_reminder_count` 是否 >0 而同一轮/同一 session 之后没有对应的 `task_checkpoint` 调用？

**结论合规**：对每个已提交的 claim（`task_checkpoint` 调用的 `arguments.claims`，digest 里是 FULL 的，不是摘要），
按 workflow.md 第 1 节判断它属于「值得记」的类型还是排除项（过程性理解、代码/git 已有、重复）；再按第 3 节检查
`statement`/`rationale`/`conditions`/`evidence` 是否遵守中文、绝对日期、`path:line` 等字段规则。如果本次会话所有
`task_checkpoint` 调用的 `claims` 都是空数组，直接说「本次会话没有产出可评判结论合规性的 claim」，不要编造。

**召回相关性**：对每个被注入的 pack（`text` 字段是 FULL 的），判断其中哪些 item 和该轮用户 prompt 相关
（同路径/同符号/同主题）；最相关的那条有没有被引用或据此行动（`facts.json.pack_usage.ctx_ids_cited_in_assistant`
和 `ctx_ids_cited_in_sctx_calls`，以及 assistant 文本里手动核对是否出现 `ctx_` id 或明显对应内容）？有没有你根据
项目知识认为语料库里明显存在、但没被召回的东西？**这是唯一允许你使用自己项目知识的方向**，凡是这类判断，必须在
句首标注「基于项目知识」，和纯粹从 bundle 摘取的证据区分开。

**候选处置**：`candidate_list`/`candidate_discard`/`candidate_confirm` 调用（在 `### sctx calls` 里）对照
workflow.md 第 7 节三档；`sctx/original/candidate_review.json` 每行的 `top_relation` 分布、`decision_source`。
如果 `candidate_list` 全程返回空 `reviews` 且 `candidate_review.json` 是空数组，说明本 session 范围内没有可处置的候选，
如实报告「无候选可处置」而不是缺失项。

**留痕**：`sctx/original/hook_event.json` 每行的 `decision`（是否 `fail_open`）、`event_kind`
（是否有无法解码的）、`reason`（是否 ≠ `ok`）；`hook_event.json` 为空数组而
`sctx_db.checkpoint_reminder_count` / `facts.json` 里的 reminder 计数 >0，是一个已知的怪现象——`sctx_facts.py`
模块 docstring 的原话是「the reminder counter and the hook_event audit log are populated by different code paths
for this host session」；`tests/scripts/session_replay/README.md` 的「Known deviations」一节进一步说明
`runtime.sqlite` 的 `hook_event` 表「on current installations no hook writes any more (hook decisions go to the
log service's `hook-diagnostics.json` aggregate instead)」——也就是说在较新的安装上 `hook_event` 为空可能根本不是
这一次会话的异常，而是这张表已经停止被写入，真正的留痕现在在 bundle 未导出的 `hook-diagnostics.json` 里。
两条说明都要据实引用，不要只挑一条、也不要替它们调和出一个更圆的解释；如果两者时间线冲突（比如 bundle 的会话
明显早于「停止写入」生效的时间），在「未能判断」里说明无法确定适用哪一条。同一秒内重复的 `session_start`、
`authorization_internal` 突发也在这里看，但如果 `hook_event.json` 本身是空数组，这些子项直接归为「未能判断」而不是「未发现」。
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
3. 每条发现一个短小节（2–4 句）：发生了什么、依据 workflow.md 哪条规则/哪个 ADR 判断它重要、建议下一步核实什么。
   这一节里每句话都要能追到第 2 步的表格行,不要引入表格之外的新事实。
4. 「未能判断」一节：列出这个 bundle 结构性做不到的判断，例如 Codex 的 reasoning 是加密的、
   `hook_event.json` 为空导致留痕方向大部分子项无法核实、SessionEnd/lease 清理状态不在
   `sctx_facts.py` 导出的表里、exec 包装脚本的完整文本被 160 字符 `args_head` 截断导致丢弃模式只能靠数字推断
   而非原文验证、Cursor bundle 缺少 tool 输出等（按实际遇到的情况写，不要照抄这个例子列表）。
5. 每句结论前或段落开头用「建议：」标出，提醒这是建议不是定论；不给任何修复方案或代码改动建议，只给「建议人工核实
   XX」这类下一步。

## 5. 明确的禁止项

- 不编造 `facts.json` 里没有的数字。
- 不转述引用，只逐字摘录。
- 不把同一根因在两个方向下各报一遍，用「见方向 X 的第 N 条」交叉引用。
- 不读 `~/.codex` 下的原始 rollout。
- 不在仓库任何路径下写文件。
- 不给出修复建议或代码改动，只给人类要核实什么。
