# Shared Context 新手使用指南

> 适用版本：`sctx 0.1.0`
>
> 适用平台：macOS（Apple Silicon/arm64 与 Intel/x86_64）
>
> 面向读者：第一次接触本项目，希望先用起来，再逐步理解高级能力的用户

## 1. 这个项目是做什么的

Shared Context 是一个给 Cursor、Codex 等编程 Agent 使用的“工程记忆库”。

它解决的问题可以用一个例子说明：同事 A 和 Agent 已经查清楚“某字段必须由服务端下发，不能由客户端计算”，几天后同事 B 或另一个 Agent 接手相关代码时，不应该再从头读需求、搜代码、做同一轮验证。Shared Context 会把已经确认且有证据的工程结论保存下来，并在后续任务真正相关时提供给 Agent。

它保存的不是整段聊天记录，而是经过整理的工程认知，例如：

- 决策（Decision）：为什么选方案 A，不选方案 B。
- 契约（Contract）：接口、Schema 或跨端约束是什么。
- 问题（Issue）和风险（Risk）：哪里可能出错，适用范围是什么。
- 验证（Validation）：运行了什么验证，结论和局限是什么。
- 发现（Discovery）和进度（Progress）：已经查清或完成了什么。

当前版本是本机优先的实现：知识保存在本机 Git 仓库中，Cursor/Codex 通过 MCP 和 Hook 使用它。首次安装可以从团队已有的非空远端 Git Knowledge Store 克隆，并通过显式 `sctx knowledge sync` 收取共享知识、发布本机工作分支；后台自动同步和自动创建 Pull Request 尚未实现。

## 2. 三分钟快速开始

如果你已经拿到了与本机 CPU 匹配的离线安装包，执行：

```bash
tar -xzf shared-context-0.1.0-darwin-arm64-offline.tar.gz
./shared-context-0.1.0-darwin-arm64-offline/install --agents cursor,codex --yes
```

Intel Mac 应使用文件名中带 `darwin-x64` 的安装包。

安装后检查：

```bash
sctx --version
sctx doctor
sctx demo
```

`sctx demo` 会在本机创建一组固定的演示数据，完成“创建 Space → 写入 Context → 审核 → 发布 → CLI/MCP 搜索”的闭环。重复执行不会反复创建相同数据。

然后用 `sctx repository add` 登记希望启用 Shared Context 的本机代码仓库，再重启已经打开的 Cursor 或 Codex。只有从已登记 checkout 内启动，或从显式登记的 Repository Group 精确根目录启动时，SessionStart 才会向 Agent 注入简短授权 marker；未登记目录保持 neutral，不进入 Hook 记录流程。进入授权范围后，Agent 才按已安装的 Shared Context Skill、MCP 工具和生命周期 Hook 维护任务意图、取回相关历史、记录检查点，并把值得长期保存的结论整理成待审核 Candidate。

## 3. 安装步骤

### 3.1 安装前检查

普通使用需要：

- macOS，CPU 为 Apple Silicon（arm64）或 Intel（x86_64）。
- Git 可正常运行。
- 至少约 64 MiB 可用空间，外加 `sctx` 二进制本身所需空间。
- 已安装 Cursor、Codex 中至少一个；两个都安装也可以。

从源码安装还需要：

- Rust stable，最低版本 1.85。
- Node.js 18 或更高版本，以及 npm。
- macOS 命令行工具提供的 `codesign`。

可以先检查常用依赖：

```bash
git --version
node --version
npm --version
rustc --version
cargo --version
```

### 3.2 方式一：使用离线安装包（普通用户推荐）

先确认机器架构：

```bash
uname -m
```

- 输出 `arm64`：选择 `darwin-arm64` 安装包。
- 输出 `x86_64`：选择 `darwin-x64` 安装包。

解压并安装：

```bash
tar -xzf shared-context-0.1.0-darwin-arm64-offline.tar.gz
./shared-context-0.1.0-darwin-arm64-offline/install --agents cursor,codex --yes
```

只接入一个 Agent 时，可以改为：

```bash
./shared-context-0.1.0-darwin-arm64-offline/install --agents codex --yes
```

或者：

```bash
./shared-context-0.1.0-darwin-arm64-offline/install --agents cursor --yes
```

离线安装器会校验文件 SHA-256，使用 npm 的离线模式安装平台包，并自动执行 `sctx setup`。不要在 `install` 后面额外添加一个 `setup` 单词。

### 3.3 方式二：从当前仓库构建并安装（开发者）

当前仓库中的 npm 包是未公开发布的内部包，不能假设 `npm install -g @company/shared-context` 可以从公共 Registry 获取。仓库提供的可用方式是本地构建安装：

```bash
cd npm
npm run install:local
```

该命令会：

1. 用 Cargo 构建当前 CPU 架构的 `sctx`。
2. 对本地二进制做 ad-hoc 签名并校验。
3. 打包主 launcher 和当前平台包。
4. 安装到仓库的 `target/npm-local` 目录并验证版本、签名和校验和。

命令结束时会打印实际的 `sctx` 路径和一条 `PATH` 设置命令。默认路径可这样加入当前终端：

```bash
export PATH="$(pwd)/../target/npm-local/bin:$PATH"
```

也可以指定专用安装目录或使用 debug 构建：

```bash
npm run install:local -- --prefix /absolute/path/to/prefix
npm run install:local -- --profile debug --prefix /absolute/path/to/prefix
```

本地 npm 安装**只安装命令，不会修改 Cursor/Codex 配置**。接着必须显式执行：

```bash
sctx setup --agents cursor,codex
```

如果团队已经有一个非空的 Shared Context Git 仓库，可以在首次安装时只提供 Git 地址：

```bash
sctx setup --agents cursor,codex \
  --knowledge-store-url git@github.example.com:team/shared-context.git
```

Setup 使用系统 Git 的 credential helper 或 SSH Agent 完成认证；URL 中不能嵌入 token、密码、查询参数或 fragment。克隆会先进入事务临时目录，只有 HEAD、工作树、Event、对象哈希、Reducer 和 Index 全部通过校验后才原子安装。空远端不受支持。

安装器会从远端默认分支创建稳定的 `shared-context/<installation-id>` 本机工作分支。默认分支只作为只读基线，Setup 不向任何远端分支 push。需要同步时显式执行：

```bash
sctx knowledge sync
```

该命令 fetch 远端默认分支和本机工作分支，合并到当前 InstallationWorkBranch，并只 push `HEAD:refs/heads/shared-context/<installation-id>`。输出中的 `needs_merge=true` 表示需要由用户在 Git 托管平台上把该工作分支合入默认分支；命令不识别 GitHub/GitLab，也不自动创建 Pull Request。

`setup --demo` 目前是非核心已知限制（Mew #195）：MCP Session guard 启用后，offline setup 没有 Agent Session lease，演示内的 public MCP search 会被正确拒绝。请不要把它用于安装验收；普通 `setup`、Hook、Skill 与授权后的 MCP 流程不受影响。

### 3.4 验证安装

```bash
sctx --version
sctx doctor
```

健康检查会检查：

- 安装目录和当前版本运行时。
- 安装级 maintenance 门禁是否可用；排他维护进行中时只报告 `maintenance` 忙，不并发读取 Git/SQLite。
- 本机知识 Git 仓库与 SQLite 索引。
- Repository Catalog。
- Cursor/Codex 配置和 Shared Context Skill。
- 两种 MCP 客户端能否完成初始化和工具列表请求。
- 当前 Agent 版本是否支持并信任生命周期 Hook。

如果检查发现可安全修复的问题：

```bash
sctx doctor --fix
```

`doctor --fix` 会重新执行可逆的注册与索引设置；它要求已有完整安装清单。

### 3.5 `setup` 会改哪些地方

默认安装根目录是 `~/.shared-context`。`setup` 会：

- 安装稳定运行时到 `~/.shared-context/bin/<版本>/<架构>/sctx`，并维护 `bin/current`。
- 初始化本地知识 Git 仓库 `~/.shared-context/repository`，或通过 `--knowledge-store-url` 原子安装已存在的非空远端 Store。
- 初始化本地索引和运行时状态。
- 按选择写入 `~/.cursor/mcp.json`、`~/.cursor/hooks.json`。
- 按选择写入 `~/.codex/config.toml`、`~/.codex/hooks.json`。
- 安装用户级最小 activation Skill 与 installer-owned 完整 workflow reference 到 `~/.agents/skills/shared-context`。
- 写安装清单，用于后续升级、诊断和精确卸载。

安装器会记录每次写入并支持失败回滚。已有配置会合并，不会把整个配置文件直接覆盖成模板。

安装后建议重启 Cursor/Codex，让 MCP、Hook 和 Skill 配置重新加载。

### 3.6 升级、卸载和彻底删除

升级已安装运行时：

```bash
sctx upgrade --agents cursor,codex
```

普通卸载：

```bash
sctx uninstall
```

只清空 Shared Context 活动数据、保留安装和 Agent 接入时，先查看计划，再显式确认：

```bash
sctx data reset --dry-run
sctx data reset --yes
```

Reset 会把知识 Git、Repository Catalog、四个 SQLite 和 pending/capture/session 临时状态重建为空，同时保留 `bin`、安装清单、Cursor/Codex 配置、Skill、日志和已有 backups。每次实际 reset 默认在 `backups/reset-<ID>` 保留可恢复旧数据；若旧知识仓配置了 Git remote，只解除新活动仓的本地绑定，绝不修改远端 ref。

卸载只移除安装器能够确认由自己拥有、且未被用户改写的配置项、运行时、日志、临时 Capture 和可重建索引。**知识仓库 `~/.shared-context/repository` 默认保留**。用户修改过的配置或 Skill 也会保留，并在报告中给出警告。

彻底删除知识仓库是不可恢复操作，必须同时提供精确绝对路径和固定确认词：

```text
sctx knowledge delete --confirm-path <知识仓库的绝对路径> \
  --confirm DELETE-SHARED-CONTEXT-KNOWLEDGE
```

先运行 `sctx uninstall` 并确认输出中的 `repository` 路径，再决定是否执行知识删除。不要猜路径，也不要把父目录或通配符作为目标。

## 4. 基础原理

### 4.1 一条知识是怎样产生的

完整流程是：

```text
当前任务目标
    ↓
Working Intent（Agent 当前理解自己在做什么）
    ↓
工作过程中的检查点、结论、未知项和有限证据
    ↓
Work Episode（一次连续工作片段）
    ↓ 关闭后自动整理
Candidate（待审核草稿，不可信、不会自动发布）
    ↓ 用户明确确认
Context（长期工程知识）
    ↓
写入本机 Git 事实库，并建立可重建索引
    ↓
未来相关任务按意图、范围和工程对象检索
```

这里最重要的安全门槛是：**自动生成的 Candidate 不会自动变成可信知识**。用户需要先查看完整内容、证据、冲突分析和推荐 Space，然后明确选择确认或丢弃。

### 4.2 常见名词

| 名词 | 通俗解释 |
|---|---|
| Context Space | 围绕一个需求或长期目标组织知识的“文件夹”，但不是搜索分区，也不绑定某个工作目录。 |
| Working Intent | Agent 对当前任务目标、范围、约束和验收条件的临时理解。只用于当前任务，不是已确认事实。 |
| Task / Task Session | 一次明确的 Agent 工程任务。一个外部 Agent 会话可保留多个历史 Task，但只有一个 Active Task。 |
| Signal | Prompt、Workspace、Diff、测试结果等工作线索。它能帮助检索，但不能单独证明一个工程事实。 |
| Work Episode | 当前 Task 中一段连续的探索、实现和验证过程。 |
| Checkpoint | Agent 明确写下的结论（Claims）和未知项（Unknowns）。 |
| Candidate | 从已关闭 Episode 生成的待审核知识草稿。默认是不可信数据。 |
| Context | 用户确认后进入长期事实层的工程知识。 |
| Revision | Intent 或 Context 的不可变版本。修改不会覆盖旧版本，而是新增 Revision。 |
| Evidence | 能独立阅读的证据快照，例如源码快照、实验记录或工程对象快照。 |
| Engineering Reference | 一条 Context 与文件、模块、符号、API、Schema 或测试之间的已验证关系。 |
| Projection / Index | 从 Git 事实重建出的 SQLite 查询视图。损坏时可以重建。 |

### 4.3 为什么同时使用 Git 和 SQLite

Shared Context 把数据分成两类：

- 长期事实进入 `~/.shared-context/repository` Git 仓库。事件只追加，不原地改写，便于审计和重建。
- 当前任务、Candidate 审核状态、搜索索引、工程图和短期 Capture 放在本机 `state` 目录中的 SQLite 或有 TTL 的文件里。

因此，SQLite 索引可以删掉重建，但 Git 知识库不应随普通卸载删除。

### 4.4 系统如何找回相关知识

检索不是“把所有旧聊天都塞给 Agent”。系统会组合多种信号：

- 当前 Working Intent 的目标、范围、平台、约束和 Hint。
- Context 的正文、类型、状态和适用范围。
- Task 与多个 Space 的相关度；允许零个、一个或多个相关 Space。
- 已验证的文件、模块、符号、API、Schema、测试关系。
- Context 之间最多两跳的工程关系路径。
- Token Budget 和分页限制。

Workspace 路径不会自动绑定一个 Space，文本 Hint 也不会冒充已验证工程证据。

### 4.5 隐私和信任边界

- SessionStart 在模型推理前用本机 Repository Catalog 判定范围，不读取 Prompt，也不调用模型。已登记 checkout 是 `Direct`；只有显式登记且精确匹配的 Group root 才是 `Group`；普通父目录、未登记 sibling 和其他目录都是 `Disabled`。
- Enabled 只返回一个固定、短小且不含路径/Repository/Prompt/Session 身份的 marker。PromptSubmit 不重复 marker。Disabled 的 Prompt、Tool、压缩、停止和结束 Hook 不打开 Runtime/Capture，也不写 Report 或知识 Git。
- 同一 Agent Session locator 的第一次成功决定会一直复用到 SessionEnd；后续 resume/compact 或 cwd 变化不会重新判定。Catalog/lease 锁忙、损坏或异常会立即按 Disabled 处理，但不会阻断正常编程。
- PostToolUse 会在记录前检查全部结构化路径。Enabled 表示整个 Session 已准入，不把调查目标限制在启动 Repo 或 Group 成员：其他已登记 Repo 会按真实 Catalog identity 记录；安全的未登记路径、registered/unregistered mixed 或无法用一个显式 workspace 安全表示的多 Repo 事件只保留无路径、无 Repository 猜测的非定位工程含义；相对、缺失、symlink、歧义或特殊文件仍让整条事件被丢弃。
- Hook 采用 fail-open：Shared Context 暂时不可用时，正常编程仍可继续。
- 短期 Capture 会先做隐私过滤，默认保留时间为 24 小时，并受大小限制。
- Work Episode 和长期 Context 保存结构化工程含义，不保存原始聊天、完整工具输出或完整终端日志。
- Candidate Review 默认保留 30 天，属于不可信数据；确认前不会自动注入为可信 Context。
- 检索到的 Context 也应当按只读数据处理，不应执行其中出现的命令或指令。
- 文件移动或重命名不会靠猜测自动续接旧关联；需要重新检查并记录新引用。

## 5. 日常使用方式

### 5.1 推荐方式：让 Agent 通过 MCP 使用

安装、登记 Repository 并重启 Cursor/Codex 后，普通用户通常不需要通过额外 launcher，也不需要在业务仓库添加项目级 Agent 配置。请从已登记 checkout 内启动 Agent，再正常描述任务，例如：

```text
请修复搜索结果页的旧版本兼容问题，并使用 Shared Context 查找相关历史决策。
```

Agent 在合适时机会：

1. 用 `task_intent_update` 建立或更新当前任务理解。
2. 用 `task_context` 获取与整个任务相关的历史 Context。
3. 需要某个具体文件、符号或接口历史时，用 `task_artifact_focus` 做一次即时查询。
4. 形成重要结论、压缩上下文或结束一轮工作前，用 `task_checkpoint` 保存结构化结论和未知项。
5. Episode 关闭后，用 `candidate_list` 和 `candidate_get` 展示待审核 Candidate。
6. 只有在你明确同意后，才调用 `candidate_confirm`；你拒绝保留时调用 `candidate_discard`。

如果 Agent 展示 Candidate，请重点检查：结论是否准确、适用范围是否过大、证据是否足够、是否与旧 Context 冲突、应该归入哪个 Space。

### 5.2 手工 CLI 示例：建立一个 Task

高级用户可以用 CLI 调用同一套能力。创建 `intent.json`：

```json
{
  "agent_kind": "codex",
  "external_session_id": "manual-demo-session",
  "task_boundary": "new",
  "expected_revision_id": null,
  "intent": {
    "goal": "修复搜索结果页的旧版本兼容问题",
    "current_direction": "先复用现有接口契约",
    "in_scope": ["搜索结果渲染", "旧版本兼容"],
    "out_of_scope": ["排序模型"],
    "domains": ["search"],
    "platforms": ["web"],
    "constraints": ["不能破坏旧客户端"],
    "acceptance_conditions": ["新旧客户端都通过兼容性测试"],
    "artifact_hints": ["SearchResult"],
    "interface_hints": ["search-v2"]
  }
}
```

执行：

```bash
sctx --json task intent update --input intent.json
```

保存返回的 `task_id` 和 `intent_revision_id`。后续同一任务的更新使用 `task_boundary: "continue"`，并把最新 `intent_revision_id` 放进 `expected_revision_id`。只有切换到无关目标时才使用 `new`。

读取当前任务 Context：

```bash
sctx task context \
  --agent-kind codex \
  --external-session-id manual-demo-session \
  --token-budget 2000 \
  --max-spaces 8
```

### 5.3 手工 CLI 示例：关闭 Episode 并审核 Candidate

`task checkpoint` 使用版本比较（CAS）避免并发覆盖。第一次写 Checkpoint 时 `expected_episode_version` 为 `0`；之后必须使用上次响应中的最新版本。

下面示例用一条自包含验证证据关闭 Episode。请把示例 ID 替换为真实返回值：

```json
{
  "agent_kind": "codex",
  "external_session_id": "manual-demo-session",
  "expected_task_id": "tsk_替换为真实值",
  "expected_intent_revision_id": "tir_替换为真实值",
  "expected_episode_version": 0,
  "boundary": "close",
  "claims": [
    {
      "context_kind_hint": "validation",
      "topic_key_hint": "search/legacy-compatibility",
      "statement": "旧客户端可以继续解析当前搜索响应",
      "rationale": "兼容性测试覆盖了旧版解析路径",
      "applicability": {
        "domains": ["search"],
        "platforms": ["web"],
        "conditions": ["search-v2 response"]
      },
      "assumptions": ["测试夹具与线上旧版 Schema 一致"],
      "recheck_when": ["响应 Schema 发生变化"],
      "evidence": [
        {
          "kind": "inline_validation",
          "evidence": {
            "kind": "experiment_record",
            "supports": "旧版解析器兼容当前响应",
            "content": {"test": "legacy_search_contract", "result": "passed"},
            "interpretation": "固定兼容性用例通过",
            "limitations": ["只覆盖当前测试夹具"]
          }
        }
      ],
      "artifact_refs": [],
      "related_contexts": []
    }
  ],
  "unknowns": []
}
```

```bash
sctx --json task checkpoint --input checkpoint.json
```

列出待审核 Candidate：

```bash
sctx candidate list \
  --agent-kind codex \
  --external-session-id manual-demo-session
```

查看完整 Candidate：

```bash
sctx candidate get \
  --agent-kind codex \
  --external-session-id manual-demo-session \
  --candidate-id <CANDIDATE_ID>
```

确认时，使用 `candidate get` 返回的最新 Task、Intent、Review 版本和 Space 推荐创建 `confirm.json`：

```json
{
  "agent_kind": "codex",
  "external_session_id": "manual-demo-session",
  "expected_task_id": "tsk_替换为真实值",
  "expected_intent_revision_id": "tir_替换为真实值",
  "candidate_id": "cnd_替换为真实值",
  "expected_review_version": 1,
  "primary": {"existing_space_id": "spc_替换为真实值"},
  "related_space_ids": [],
  "edits": {}
}
```

```bash
sctx --json candidate confirm --input confirm.json
```

如果使用系统推荐的新 Space，把 `primary` 改为：

```json
{"new_space_recommendation_id": "rec_替换为真实值"}
```

不保留时执行：

```bash
sctx candidate discard \
  --agent-kind codex \
  --external-session-id manual-demo-session \
  --expected-task-id <TASK_ID> \
  --expected-intent-revision-id <INTENT_REVISION_ID> \
  --candidate-id <CANDIDATE_ID> \
  --expected-review-version <REVIEW_VERSION> \
  --reason "证据不足，暂不沉淀"
```

## 6. 所有功能与命令

本节覆盖当前 `sctx --help` 中暴露的全部功能。命令默认输出便于人阅读的 JSON；在任意命令中加入全局参数 `--json` 可获得稳定的单行 JSON 信封，适合脚本处理。`sctx --help` 或 `sctx -h` 查看总帮助，`sctx --version` 或 `sctx -V` 查看版本。

### 6.1 安装与维护

| 命令 | 功能 |
|---|---|
| `sctx setup [--demo] [--agents cursor,codex] [--knowledge-store-url GIT_URL]` | 首次安装运行时、知识库、索引、MCP、Hook 和 Skill；可从已有非空远端 Store 克隆并幂等执行。`--demo` 是 #195 已接受的非核心已知限制，不作为安装验收。 |
| `sctx demo` | 建立并验证固定演示闭环；重复执行可复用已有演示数据。 |
| `sctx doctor` | 只读检查安装、索引、配置、MCP 和 Agent 能力。 |
| `sctx doctor --fix` | 重做安全、可逆的注册和索引设置后再次检查。 |
| `sctx upgrade [--agents cursor,codex]` | 安装新版本并原子切换 `bin/current`。 |
| `sctx data reset --dry-run` / `--yes` | 预览或确认事务式清空活动数据；保留安装结构、Agent 接入和默认恢复备份，不修改远端 Git。 |
| `sctx uninstall` | 精确移除安装器拥有的运行时和接入配置，保留知识库。 |
| `sctx knowledge sync` | 显式收取远端默认/工作分支，验证并合并到本机 InstallationWorkBranch，只发布该工作分支。 |
| `sctx knowledge delete ...` | 双重确认后永久删除知识 Git 仓库。 |

`--root`、`--runtime-source`、`--runtime-version` 主要用于安装包、测试和受控部署。普通命令固定读取 `~/.shared-context`，日常用户应使用默认根目录，避免“setup 到自定义目录、运行时却读取默认目录”的混淆。

### 6.2 Space 管理

| 命令 | 功能与关键参数 |
|---|---|
| `sctx space create --input <INTENT.json>` | 从 JSON 创建一个 Context Space。 |
| `sctx space create --title ... --problem ... --desired-outcome ...` | 用命令行字段创建 Space；`--in-scope` 和 `--acceptance-condition` 至少各提供一项，其他列表参数可重复。 |
| `sctx space intent revise --space-id ... --parent-revision-id ... <Intent 字段>` | 新增 Intent Revision。必须提供当前全部 Head；并发分支存在时用多个 `--parent-revision-id` 收敛。 |
| `sctx space list` | 列出所有 Space、Intent Head、标题和 Context 数量。 |
| `sctx space get --space-id <ID>` | 获取一个 Space 的完整投影视图。 |

Intent JSON 的字段是：

```json
{
  "title": "需求标题",
  "problem": "要解决的问题",
  "desired_outcome": "希望得到的结果",
  "in_scope": ["范围内事项"],
  "out_of_scope": ["范围外事项"],
  "acceptance_conditions": ["验收条件"],
  "domain_terms": ["领域术语"]
}
```

### 6.3 Task、Intent、Signal、Artifact Focus 和 Checkpoint

| 命令 | 功能 |
|---|---|
| `sctx task intent update --input <JSON>` | 创建新 Task 或以 CAS 更新当前 Working Intent，并立即返回 Task Context Pack。 |
| `sctx task context --agent-kind ... --external-session-id ... [--token-budget 2000] [--max-spaces 8]` | 只读获取现有 Active Task 的相关 Context，不修改状态。Token Budget 最低 256，最多返回 32 个 Space。 |
| `sctx task artifact-focus --input <JSON>` | 针对一个文件、模块、符号、API、Schema 或测试做一次即时历史查询。Focus 不持久化，也不会成为证据。 |
| `sctx task checkpoint --input <JSON>` | 在 Task、Intent、Episode 三重版本保护下保存 Claims/Unknowns；`close` 会触发 Candidate Builder。 |
| `sctx task signal supersede --input <JSON>` | 把已经不再相关的活动 Signal 标记为 superseded；保留历史，不执行删除。 |

`task_boundary` 的规则：

- `continue`：仍是同一工程目标，包括修复、测试、范围调整和新增约束。
- `new`：用户明确切换到无关目标或新交付物。
- 拿不准时保留连续性，用 `continue`。

Artifact Focus 的 `locator_kind` 支持：`file`、`module`、`symbol`、`api`、`schema`、`test`。请求提供绝对文件路径；Repository ID 和仓库相对路径由本机 Catalog 解析，调用者不要猜。

### 6.4 Candidate 审核

| 命令 | 功能 |
|---|---|
| `sctx candidate list --agent-kind ... --external-session-id ...` | 分页列出当前 Task 的 Candidate Review；默认只列 `pending`。可用 `--status`、`--limit`、`--cursor`、`--token-budget`。 |
| `sctx candidate get ... --candidate-id <ID>` | 获取完整草稿、证据、来源、冲突分析、置信度、未知项和 Space 推荐。 |
| `sctx candidate analyze --candidate-id <ID> [--token-budget 4096] [--top-k 16]` | 重新计算与已有 Context 的重复、支持、修订、潜在冲突和相关性分析；不写入 Git。 |
| `sctx candidate confirm --input <JSON>` | 用户明确确认后，把 Candidate、Primary/Related Space 和可选编辑作为一个原子事实批次写入。 |
| `sctx candidate discard ... --reason <TEXT>` | 用户明确拒绝保留时丢弃 Candidate Review；不会发布任何 Context。 |
| `sctx candidate build-closed-episode --episode-id <ID>` | 在 Episode 已关闭但 Builder 响应丢失或待恢复时重建；属于恢复命令。 |

Candidate 状态支持 `pending`、`discarded`、`expired`、`confirmed`。只有完整分析且 `ready_for_review` 的 Candidate 才适合让用户决策。`potential_contradiction` 和 `unresolved_related` 是审核线索，不是已经成立的事实。

确认时的 `edits` 可以只替换用户明确要求修改的字段：`kind`、`topic_key`、`statement`、`rationale`、`applicability`、`assumptions`、`recheck_when`、`relations`、`evidence`。省略字段表示保留原草稿；`topic_key` 使用 `{"action":"clear"}` 才表示显式清空。

### 6.5 Context 内容与治理

Context 类型共有七种：

| 类型 | 含义 |
|---|---|
| `decision` | 已做出的方案选择及原因。 |
| `contract` | API、Schema、跨端或模块契约。 |
| `issue` | 已知问题。 |
| `risk` | 可能发生的问题及触发条件。 |
| `validation` | 测试、实验或检查结论。 |
| `discovery` | 已查明但不属于以上类别的事实。 |
| `progress` | 可复用的工作进度信息。 |

可用命令：

| 命令 | 功能 |
|---|---|
| `sctx context get --space-id ... --context-id ... [--revision-id ...]` | 获取 Context 全部状态，或一个不可变 Revision。 |
| `sctx context revise --space-id ... --context-id ... --parent-revision-id ... --input <JSON>` | 新增 Context Revision。必须引用当前全部 Revision Head。 |
| `sctx context review --space-id ... --context-id ... --revision-id ... --verdict <approve 或 reject> --reason ...` | 对一个 Revision 做明确审核。 |
| `sctx context publish ...` | 发布一个被确定性批准的 Revision。必须引用当前全部 Publication Head 和该 Revision 的全部 Review Event。 |
| `sctx context withdraw ...` | 撤回当前治理 Head 选中的 Revision，不接受 Review Event。 |

Context 内容可通过 `--input` JSON 提供：

```json
{
  "kind": "decision",
  "topic_key": "search/response-source",
  "statement": "搜索字段必须由服务端下发",
  "rationale": "客户端计算会导致跨端不一致",
  "applicability": {
    "domains": ["search"],
    "platforms": ["ios", "android"],
    "conditions": ["legacy clients supported"]
  },
  "assumptions": ["服务端契约保持兼容"],
  "recheck_when": ["旧客户端停止支持"],
  "evidence": [
    {
      "kind": "source_snapshot",
      "supports": "字段来自响应 Schema",
      "content": {"source": "search-response schema", "field": "general_tab"},
      "interpretation": "客户端只消费字段，不负责计算",
      "limitations": ["快照不代表未来版本"]
    }
  ]
}
```

证据类型支持 `source_snapshot`、`experiment_record`、`artifact_snapshot`。`content` 必须是非空 JSON 对象；证据要能独立说明它支持什么、如何解释、有什么局限。

搜索可见状态包括 `candidate`、`accepted`、`deprecated`、`superseded`、`governance_conflict`。

### 6.6 语义冲突

语义冲突用于表示多个已经发布的 Context 在同一适用范围内互相冲突，而不是简单的文本不同。

| 命令 | 功能 |
|---|---|
| `sctx semantic conflict open --space-id ... --participant CONTEXT_ID:REVISION_ID:PUBLICATION_ID ... --reason ...` | 对当前 Accepted Head 打开冲突；可重复提供 participant，并用 `--domain`、`--platform`、`--condition` 描述范围。 |
| `sctx semantic conflict resolve --space-id ... --conflict-id ... ...` | 写入冲突解决 Revision，必须覆盖每个参与 Context，并引用当前 Publication。 |

首次解决用 `--expect-no-resolution-head`；继续或合并解决分支时，用一个或多个 `--previous-resolution-id`。每个结果格式为：

```text
CONTEXT_ID:REVISION_ID:retained|revised|withdrawn|scope_split
```

### 6.7 Repository Catalog 与工程图

| 命令 | 功能 |
|---|---|
| `sctx repository add --repository-id <ID> --path <绝对仓库路径> [--path ...]` | 用团队约定的 ID 创建本机 Catalog 身份，或给已有 exact ID 增加 canonical checkout/worktree。 |
| `sctx repository list` | 查看 Repository Catalog，并同步本地 Registry。 |
| `sctx repository doctor` | 检查 checkout 是可用、缺失还是不安全，并在安全时同步 Registry。 |
| `sctx repository group add --root <绝对父目录> --member-repository-id <ID> [--member-repository-id <ID> ...]` | 显式登记一个精确父目录为 Repository Group；只有该 root 本身可以启用 Group，普通祖先目录不会自动启用。 |
| `sctx repository group update --repository-group-id <ID> [--root <路径>] [--member-repository-id <ID> ...]` | 显式修复或更新 Group root/成员。 |
| `sctx repository group remove --repository-group-id <ID>` | 移除 Group；不删除成员 Repository。 |
| `sctx repository group list` / `doctor` | 查看 Group 与成员、检查 root 漂移或不可用状态。 |
| `sctx repository scan --checkout-path <路径> [--path <仓库相对路径>] [--max-artifacts 200]` | 显式扫描有界路径并返回工程对象摘要，不返回源码正文。最多请求 1000 个 Artifact。 |
| `sctx engineering-reference record --input <JSON>` | 把现有 Context Revision 与已验证的工程对象关系写入长期事实层。 |
| `sctx association explain --reference-id <ID>` | 解释一条 Reference 当前解析到了什么、依据是什么、是否存在歧义、有哪些图路径。 |
| `sctx association rebuild [--diagnose]` | 重新扫描有界计划并重建关联；`--diagnose` 只诊断，不落新的工程图快照。 |

支持的工程对象是 `module`、`file`、`symbol`、`api`、`schema`、`test`；关系是 `implements`、`defines`、`consumes`、`validates`、`constrains`、`depends_on`。

RepositoryId 是 1–64 字节、大小写敏感的可读 ASCII 名称，例如 `Android`、`iOS`、`FE`。首字符必须是字母，后续可使用字母、数字、点、下划线和连字符。团队成员应对同一逻辑源码仓库使用完全相同的 ID；本机 Catalog 会拒绝 `FE` 与 `fe` 这类仅大小写不同的重复身份，也不会从路径、basename 或 Git remote 猜测 ID。

`engineering-reference record` 只应在直接检查或验证后调用。它要求完整、确定性的 locator、非空 `supports` 和至少一条 `limitations`。不要用相似文件名猜移动或重命名关系。

例如多个 Android 仓库位于同一父目录，而你希望从父目录启动 Agent，应先分别用显式 Repository ID 登记，再用 `repository list` 核对并创建 Group。仅仅把已登记仓库放在同一个父目录下不会自动产生 Group；从更高层祖先目录启动仍是 Disabled。这条规则避免系统把未登记 sibling 猜测成 Group 成员或 Repository identity；Session 已准入后对安全未登记位置的调查最多形成非定位工程含义。

### 6.8 搜索与读取

```bash
sctx search \
  --query "旧版本兼容" \
  --domain search \
  --platform ios \
  --kind decision \
  --status accepted \
  --page-size 20
```

`search` 支持以下可重复硬过滤参数：

- `--space-id`
- `--domain`
- `--platform`
- `--condition`
- `--kind`
- `--status`

还支持 `--cursor` 翻页。空 `--query` 也合法，可以只使用结构化过滤。结果会给出匹配理由、索引 Tree/Generation 和冲突信息。

### 6.9 索引、待提交批次与事件校验

这些是运维或故障恢复命令，普通用户通常只需 `doctor`。

| 命令 | 功能 |
|---|---|
| `sctx index status` | 同步索引并报告当前 Tree、Generation 和诊断。 |
| `sctx index rebuild` | 从 Git 事件完全重建 SQLite 投影。 |
| `sctx pending list` | 列出因为 Git 写入中断而留下的待处理 Batch。 |
| `sctx pending commit <BATCH_ID>` | 继续提交一个已检查的 Pending Batch。 |
| `sctx pending move-aside <BATCH_ID>` | 把异常 Batch 移到隔离位置，保留文件供人工排查。 |
| `sctx validate --staged` | 校验知识 Git 仓库中已经 staged 的事件是否符合 Schema 和隐私规则。 |

不要在不了解 Batch 内容时直接执行 `pending commit`。先查看 `pending list`、运行 `doctor`，必要时检查知识仓库状态。

### 6.10 Hook 与 MCP 服务

| 命令 | 功能 |
|---|---|
| `sctx hook --agent cursor` | 从标准输入接收 Cursor Hook JSON，输出 Cursor 所需响应。通常由安装器写入的 Hook 配置调用。 |
| `sctx hook --agent codex` | 处理 Codex Hook JSON。通常不应手工调用。 |
| `sctx hook --agent <cursor 或 codex> --capabilities ...` | 探测 Agent 版本、Hook 可用性和信任状态。Codex 可用 `--trust <confirmed 或 unconfirmed>`。 |
| `sctx mcp serve --client cursor` | 通过标准输入/输出运行 Cursor MCP Server。 |
| `sctx mcp serve --client codex` | 通过标准输入/输出运行 Codex MCP Server。 |

安装后的 MCP 一共暴露 16 个工具：

1. `task_intent_update`
2. `task_artifact_focus`
3. `task_signal_supersede`
4. `task_checkpoint`
5. `task_context`
6. `repository_scan`
7. `engineering_reference_record`
8. `association_explain`
9. `association_rebuild`
10. `context_search`
11. `context_get`
12. `candidate_list`
13. `candidate_get`
14. `candidate_discard`
15. `candidate_confirm`
16. `space_list`

CLI 还提供 Space/Context 写入治理、语义冲突、索引和 Pending Batch 等管理员能力；这些没有全部开放成 Agent MCP 写工具，以维持显式审核和生命周期边界。

当前 Repository 准入控制 Hook 的 Agent-visible activation 与生命周期记录路径；MCP Server 也用 current Enabled Session lease 实现授权校验，Disabled/Missing/Expired/Stale/busy/corrupt Session 的调用会被拒绝。已安装的全局 Skill 主入口只包含最小 activation gate：没有可信 SessionStart marker 时不读取完整 workflow reference、不产生 Shared Context MCP 调用提示；有 marker 时才完整读取一次 installer-owned reference。这个 Skill gate 是 Agent 推理前的指令准入机制，Server guard 则负责安全和不落越权数据。MCP 进程和工具 Schema 仍由用户级 Agent 配置提供，可能物理启动或可见；不要把 Disabled 理解为进程必然未启动，也不要把合约中的 reference-read/MCP-call 字节代理外推为真实计费 token 已被测量。当前还已证明 Disabled Hook 不向模型注入 Shared Context 文本，也不产生 Runtime/Capture/Report/知识 Git 记录。

#193 的固定 bytes proxy 进一步量化该边界：Disabled 的 Agent-visible activation、完整 workflow read、Shared Context MCP call/result 与业务 residue 都是 0；Enabled 每个 SessionStart marker 为 109 bytes（上限 128），完整 workflow 读取一次，并在固定 Direct/Group 验收链中产生 5 次真实 public MCP 调用。最小 gate、workflow、metadata 源文件分别为 1472、10390、263 bytes。这些值用于回归比较，不是 tokenizer 结果或供应商计费 token。

## 7. 常见问题

### 7.1 `sctx` 找不到

从源码本地安装时，确认使用了安装命令最后打印的 PATH。默认可执行文件位于仓库的 `target/npm-local/bin/sctx`。重新打开终端后，需要把该目录加入 shell 启动配置，或者使用绝对路径执行。

### 7.2 `doctor` 提示 Hook 未验证或需要信任

先重启 Cursor/Codex，再运行：

```bash
sctx doctor
```

如果报告为 `ACTION REQUIRED`，按报告中的 Agent 能力提示完成信任设置，再运行 `sctx doctor --fix`。Hook 不可用时系统会降级；正常编程不会被阻断，但 Agent 应在结束前用显式 `task_checkpoint boundary=close` 完成 Episode。

### 7.3 报 `intent_stale`、`checkpoint_stale` 或 Review 版本过期

这是并发保护在生效，不是数据损坏。重新读取当前 Task/Candidate，使用最新的 `intent_revision_id`、`episode_version` 或 `review_version`，核对内容后重试。不要猜 ID，也不要用旧版本覆盖新状态。

### 7.4 搜不到某个文件的历史 Context

依次检查：

```bash
sctx repository list
sctx repository doctor
sctx association rebuild --diagnose
```

确认仓库已经登记、路径仍存在、相关 Context 已经记录 Engineering Reference，并且工程图已经显式构建。`artifact_not_reachable_in_graph` 只表示当前历史图没有安全的精确路径，不代表当前源码文件不存在。

### 7.5 在多个仓库的父目录启动时为什么没有激活

父目录不会因为下面恰好有多个已登记仓库而自动获得权限。先确认每个成员都已用 `sctx repository add` 登记，再显式执行：

```bash
sctx repository group add \
  --root /absolute/path/to/android-parent \
  --member-repository-id <REPOSITORY_A_ID> \
  --member-repository-id <REPOSITORY_B_ID>
```

Group 只匹配这个 canonical root 的精确路径；父目录的父目录、未登记 sibling、漂移后的旧路径都保持 Disabled。用 `sctx repository group doctor` 检查当前状态。创建 Group 后应新开一个 Agent Session；同一 Session 已经形成的 Enabled/Disabled lease 不会因为 cwd 或 Catalog 随后变化而改写。

### 7.6 Candidate 没有生成

常见原因是 Episode 未关闭、Checkpoint 只有 Unknown、Claim 没有充分 Evidence，或者 Builder 返回 `needs_evidence`。先查看 `task checkpoint` 响应；如果 Episode 已关闭但构建响应丢失，可使用：

```bash
sctx candidate build-closed-episode --episode-id <EPISODE_ID>
```

不要为了生成 Candidate 而编造 Evidence、问题或工程引用。

### 7.7 SQLite 索引异常

先执行：

```bash
sctx doctor
sctx index status
```

确认 Git 知识库正常后再执行：

```bash
sctx index rebuild
```

索引是派生数据，可以从 Git 事实重建。

### 7.8 能否团队共享或跨电脑同步

可以从同一个团队远端 Git 地址初始化多台机器，每台安装都会使用自己的 `shared-context/<installation-id>` 工作分支。各机器显式运行 `sctx knowledge sync` 后，会把远端默认分支和自己的远端工作分支合入本机，再只发布自己的工作分支。用户仍需在托管平台创建/合并 Pull Request，其他机器再显式同步；没有后台定时同步。Repository Catalog 仍是每台机器的本机显式配置，团队成员需要为同一业务源码仓约定相同的 RepositoryId。

## 8. 开发与验证

修改项目后，完整检查命令是：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
cd npm
npm test
```

本地端到端安装验证：

```bash
cd npm
npm run install:local
```

npm 测试会检查 launcher、平台包、离线 Bundle 和安装契约。Apple Silicon 主机可以执行 arm64 真实安装 smoke；没有 Intel 主机时，x64 交叉构建测试只证明包结构、Mach-O、签名和 CPU 契约，不等同于 Intel 原生运行验证。

## 9. 当前明确不支持的能力

- 后台定时同步、自动创建/合并 Pull Request 和无人值守跨电脑分发。
- 把 Workspace 自动绑定成某个 Space。
- 把文本 Hint、Prompt、文件路径或测试结果自动当成可信工程证据。
- 未经用户审核，自动确认、发布或注入 Candidate。
- 在查询时扫描整个仓库、猜测文件移动/重命名，或把它当作通用全仓代码搜索引擎。
- Windows、Linux 的正式安装与运行。

## 10. 进一步阅读

- 项目目标与术语：[`../CONTEXT.md`](../CONTEXT.md)
- 技术设计与不变量：[`../technical-design.md`](../technical-design.md)
- 开发约定：[`../DEVELOPMENT.md`](../DEVELOPMENT.md)
- npm 打包和离线安装：[`../npm/README.md`](../npm/README.md)
- 事件 Schema：[`../schemas/README.md`](../schemas/README.md)
