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

然后用 `sctx repository add` 登记希望启用 Shared Context 的本机代码仓库，再重启已经打开的 Cursor 或 Codex。只有从已登记 checkout 内启动，或从一个下面放着已登记 checkout 的父目录启动时，SessionStart 才会向 Agent 注入简短授权 marker（其中带有本会话的 `external_session_id`，供 Agent 原样回传）；未登记目录保持 neutral，不进入 Hook 记录流程。进入授权范围后，Agent 才按已安装的 Shared Context Skill、MCP 工具和生命周期 Hook 维护任务意图、取回相关历史、记录检查点，并把值得长期保存的结论整理成待审核 Candidate。

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

### 3.3 方式二：内网 NPM 或源码开发安装

正式版本从内网 Registry 安装；包不会发布到公共 npm：

```bash
npm install -g @bytedance-dev/shared-context \
  --registry=https://bnpm.byted.org
```

安装待发布代码或做本地开发验证时，可从当前仓库一键构建并安装：

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

需要验证与正式流水线完全一致的三个待发布 tarball 时，运行：

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
npm run release:version -- 0.2.0
npm run release:test
```

`release:version` 会先同步 Cargo、Cargo.lock、三个发布包及 optionalDependencies 的版本；
`release:test` 随后重新编译并确认安装后的 `sctx --version` 与该版本完全一致。

产物保存在 `target/npm-release`，隔离安装前缀为 `target/npm-release-test`；该命令不会上传
package，也不会修改 Cursor/Codex 配置。

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
- 安装用户级最小 activation Skill 与 installer-owned 完整 workflow reference 到 `~/.agents/skills/shared-context`。该 workflow 规定知识库正文默认用中文书写（intent 的 goal/current_direction/in_scope，claim 的 statement/rationale/conditions，evidence.summary），代码标识符、路径与命令保持原文；这只约束沉淀内容，不改变 Agent 与用户交流使用的语言。
- 写安装清单，用于后续升级、诊断和精确卸载。

安装器会记录每次写入并支持失败回滚。已有配置会合并，不会把整个配置文件直接覆盖成模板。

安装后建议重启 Cursor/Codex，让 MCP、Hook 和 Skill 配置重新加载。

### 3.6 升级、卸载和彻底删除

升级已安装运行时：

```bash
sctx upgrade --agents cursor,codex
```

项目尚未上线，因此 Setup/Upgrade 不维护旧 Task Runtime 兼容层：检测到已知 schema 11 或 12 时，会备份并丢弃 `runtime.sqlite` 及 sidecars，再初始化 schema 13；未确认的本地 Task、Checkpoint 和 Candidate Review 会随之清空。未知或未来 schema 会 fail closed，不做猜测性迁移；若后续安装步骤失败，事务回滚会恢复原文件和权限。SQLite 索引 `index.sqlite` 走独立的 schema 版本（当前 15）：升级检测到旧版本时会整表重建索引，不需要用户手动干预，也不影响上面的 `runtime.sqlite`（仍是 schema 13）；索引本身可以随时删掉重建，Git 知识库不受影响。

普通卸载：

```bash
sctx uninstall
```

只清空 Shared Context 活动数据、保留安装和 Agent 接入时，先查看计划，再显式确认：

```bash
sctx data reset --dry-run
sctx data reset --yes
```

Reset 会把知识 Git、Repository Catalog、四个 SQLite 和 pending/session 临时状态重建为空，同时清理旧版本遗留的 Capture 文件，并保留 `bin`、安装清单、Cursor/Codex 配置、Skill、日志和已有 backups。每次实际 reset 默认在 `backups/reset-<ID>` 保留可恢复旧数据；若旧知识仓配置了 Git remote，只解除新活动仓的本地绑定，绝不修改远端 ref。

卸载只移除安装器能够确认由自己拥有、且未被用户改写的配置项、运行时、日志、旧版临时文件和可重建索引。**知识仓库 `~/.shared-context/repository` 默认保留**。用户修改过的配置或 Skill 也会保留，并在报告中给出警告。

彻底删除知识仓库是不可恢复操作，必须同时提供精确绝对路径和固定确认词：

```text
sctx knowledge delete --confirm-path <知识仓库的绝对路径> \
  --confirm DELETE-SHARED-CONTEXT-KNOWLEDGE
```

先运行 `sctx uninstall` 并确认输出中的 `repository` 路径，再决定是否执行知识删除。不要猜路径，也不要把父目录或通配符作为目标。

### 3.7 启用语义召回（可选）

默认检索是纯词法的。词法通道找不到「砍掉 / 排除」「参数拼装 / addParamsForLiveAnchor」这类**只有同义关系、没有共同词**的联系，也搜不动跨语言（英文 Intent 对中文知识库）。语义召回（ADR-0004）补的就是这一路。它是**可选增强**：不装就完全不存在，检索行为与加这个通道之前逐字节一致，零磁盘、零内存、零延迟开销。

一条命令启用：

```bash
sctx embedding install
```

它会依次做完过去要手工做的六步：下载 bge-m3 的 ONNX 导出（`model.onnx`、`model.onnx_data`、`tokenizer.json`）和 ONNX Runtime 1.28.1 的动态库，逐个文件按内置 SHA-256 校验，解包取出 `libonnxruntime.dylib`，**真的加载模型编码一句话**证明它能跑，然后才写 `config.toml` 的 `[retrieval]`，最后把已接受 Context 的向量回填进 `state/semantic.sqlite` 并打印条数。

安装过程逐步打印在 stderr，stdout 仍然只有一个 JSON 信封，所以 `sctx --json embedding install` 可以直接被脚本消费。

**开销预期**（bge-m3，实测量级）：

| 项目 | 预期 |
|---|---|
| 下载体积 | 约 2.3 GB（其中 `model.onnx_data` 2.1 GB） |
| 安装后磁盘 | `~/.shared-context/embedding/` 约 2.3 GB，另加 `state/semantic.sqlite`（每条 revision 约 4 KB） |
| 常驻内存 | 模型加载后 RSS 约 1.2 GB，只在 `sctx mcp serve` 进程里 |
| 模型加载耗时 | 9–12 秒，每个 serve 进程一次，后台线程完成，加载期间检索照常走词法通道 |
| 查询编码 | p95 30–85 ms，超过 200ms 预算的单次编码静默降级 |
| `install` 总耗时 | 下载时间 + 约 15 秒（校验 + 自检 + 回填） |

命令是**幂等**的：已经存在且校验通过的文件不会重新下载，中断的下载会从断点续传，所以一次失败的 2.1 GB 传输重跑就好，不用从头再来。

**团队内网源**。`--model-url` 指向一个按原名提供那三个文件的目录，公司内网镜像最常见：

```bash
sctx embedding install --model-url https://mirror.example.internal/models/bge-m3-onnx
```

内置的 SHA-256 是**文件的属性、不是站点的属性**：镜像同样要过一模一样的校验，字节不对就直接失败，什么都不装、也不写配置。这正是 `--model-url` 敢存在的原因。指定 `--model-url` 后不再回退到公网（否则「指定内网源」这件事就白做了）。

`--runtime-url` 同理指向一个 ONNX Runtime release tarball。

**平台支持**。macOS arm64 开箱即用。macOS x86_64 上游 ONNX Runtime 1.28.1 **没有发布**对应产物，所以必须自己提供并为其背书：

```bash
sctx embedding install --runtime-url <URL> --expected-sha256 <shasum -a 256 的输出>
```

其他平台会直接报 unsupported，并给出手工配置 `[retrieval]` 的指引——通道本身不限平台，只有这条便捷命令限。

**查看与关闭**：

```bash
sctx embedding status            # 配置、文件、向量条数、模型指纹（不加载模型）
sctx embedding status --verify   # 额外真加载一次，确认能跑（9–12 秒）
sctx embedding remove --yes      # 删配置节 + embedding/ 目录 + semantic.sqlite
```

`status` 默认只查文件字节数不重算 SHA-256——对 2.1 GB 重算一次要十几秒，而它要抓的问题（文件被删或写了一半）字节数就能看出来；想确认「字节是对的」而不只是「文件在」，用 `--verify`。

也可以在首次安装时顺带启用：

```bash
sctx setup --embedding
```

这一步失败**不会**让 `setup` 失败——语义召回是可选增强，用一个能工作的安装去换一个装不上的安装并不划算；失败只在报告的 `notices` 里留一条，之后随时可以重跑 `sctx embedding install`。`sctx upgrade` 不会自动安装。

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
| Context Relation | 两条 Context 之间由用户确认的稳定知识关系，例如 implements 或 validated_by；它不同于文本相关性。 |
| Related Space | Context 的辅助组织和召回角色；Context 仍由一个 Primary Space 拥有，不会复制到 Related Space。 |
| Projection / Index | 从 Git 事实重建出的 SQLite 查询视图。损坏时可以重建。 |

### 4.3 为什么同时使用 Git 和 SQLite

Shared Context 把数据分成两类：

- 长期事实进入 `~/.shared-context/repository` Git 仓库。事件只追加，不原地改写，便于审计和重建。
- 当前任务、Checkpoint operation receipt、Candidate Build outbox、Candidate 审核状态、搜索索引和工程图放在本机 `state` 目录中。

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

自动任务检索还会过滤 stopword、过短词、通用工程词和大语料中的高频词。剩余文本至少满足“完整短语、高覆盖率、两个独立文本通道、Context 文本加精确适用范围”之一，或拥有精确 Graph/ContextRelation 路径，才会进入自动 Context Pack；同一 Space 中的弱文本命中不会把无关 Context 一起带入。显式搜索/Explicit 模式不受这个自动门槛限制，仍可用于排查。

### 4.5 隐私和信任边界

- SessionStart 在模型推理前用本机 Repository Catalog 推导范围，不读取 Prompt，也不调用模型。启动目录在某个已登记 checkout 内 → 为该仓库 `Enabled`（最深的 checkout 说了算）；否则启动目录下面有几个已登记 checkout，就为那几个仓库 `Enabled`；两者都不满足则 `Disabled`。文件系统根、你的 HOME 目录本身以及 HOME 的上一级永远不会用第二条规则启用——从那里推导会把整机所有已登记仓库一次性拉进来。未登记 sibling、下面没有任何已登记仓库的目录同样是 `Disabled`。
- Enabled 只返回一个形状固定、短小且不含路径/Repository/Prompt 身份的 marker，例如：`<shared-context-active external_session_id="…">Shared Context is authorized for this session. Before substantive work, call task_intent_update with agent_kind "codex" and external_session_id "…" (copy it verbatim; never invent one).</shared-context-active>`（硬上限 512 bytes）；marker 里唯一随会话变化的是 host session id 与 `agent_kind`，Agent 必须把这个 id 原样当作 `external_session_id` 回传（Codex 也可用 `printenv CODEX_SESSION_ID` 核对，Cursor 即 conversation id），不得自行编造。host session id 本身不合法（为空、超长或含非常见字符）时，marker 会退化成不带 `external_session_id` 属性的版本，改用一句话说明去哪里找这个 id，同样不会让 Agent 编造。PromptSubmit 不重复 marker。Disabled 的 Prompt、Tool、压缩、停止和结束 Hook 不打开业务 Runtime，也不写 Report 或知识 Git。
- 同一 Agent Session 的准入 lease 与该会话**永久绑定、不过期**：会话再长也不会中途变成"未授权"。第一次成功决定记录的是会话启动目录，后续 resume/compact 或 cwd 变化都不会改写它。
- lease 会跟随 Repository 登记变化：每次 Hook 事件与 MCP 调用都用记录的启动目录对当前 Catalog 重新判定一次（纯内存，不跑 Git 也不扫描仓库）。所以 `sctx repository add` 之后正在运行的会话下一次调用就生效，取消登记之后立刻变回 Disabled，不必重开会话。
- Catalog/lease 锁忙、记录损坏或异常会立即按 Disabled 处理，但不会阻断正常编程；损坏或旧版本的 lease 记录一律当作不存在，由下一次 SessionStart 重写。
- lease 由 SessionEnd 删除。部分宿主（例如 Codex 桌面版）不发送 SessionEnd，留下的 lease 由回收兜底：`sctx doctor` 会报告可回收数量，`sctx doctor --fix`、`setup` 与 `upgrade` 会删除超过 30 天以及永远无法再使用的 lease 记录。记录目录仍有 4096 条 / 8MB 上限，写满时淘汰最旧的 lease，而不是拒绝新会话。
- PostToolUse 会在记录前检查 `absolute_file_path`、`file_path`、`filepath`、`path`、`cwd`、`workdir`、`working_directory` 等已知结构化路径。Enabled 表示整个 Session 已准入，不把调查目标限制在启动时推导出的那几个 Repo：其他已登记 Repo 会按真实 Catalog identity 记录；安全的未登记路径、registered/unregistered mixed 或无法用一个显式 workspace 安全表示的多 Repo 事件只保留无路径、无 Repository 猜测的非定位工程含义；相对、缺失、symlink、歧义或特殊文件仍让整条事件被丢弃。
- Adapter 只保留 `FileOperation`、`TestRunner`、`Shell`、`SharedContext` 或 `Other`。Hook 由这些机械分类产生的内容只能成为非事实 TaskSignal；原始命令、输出和 vendor tool name 不保存，Shared Context 自身工具也不会回流。
- Hook 采用 fail-open：Shared Context 暂时不可用或本地锁忙时，正常编程仍可继续。Hook 不排队 Claim，也不会在返回后补写工程事实。
- Claim Evidence 必须由工作 Agent 根据直接检查或验证自行聚焦产出；Hook 提示、TaskSignal、Prompt 和工具状态不能冒充 Evidence。
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

开始一项新需求时，建议先显式建立归属的 Space。Agent 在会话内用 MCP `space_create` 工具（输入 `agent_kind`、`external_session_id` 和一个完整的 `intent` 对象，校验与 CLI `sctx space create` 完全一致，返回 `space_id` 与 `intent_revision_id`，`provisional` 恒为 `false`）；`sctx space create` 保留给终端前的 operator，因为沙箱会话里执行 shell 需要额外提权。如果跳过这一步，`candidate_confirm` 时系统仍会按 Task 自动生成一个 `provisional` Space 兜底（同一个 Task 的所有 Candidate 都落在这一个 Space 上），但显式建 Space 能让后续 Candidate 归类更准确。

Agent 在合适时机会：

1. 用 `task_intent_update` 建立或更新当前任务理解。
2. 用 `task_context` 获取与整个任务相关的历史 Context。
3. 需要某个具体文件、符号或接口历史时，用 `task_artifact_focus` 做一次即时查询。
4. 形成重要结论、压缩上下文或结束一轮工作前，用 `task_checkpoint` 直接提交聚焦的 Claims、Unknowns 和自包含 Evidence 摘要。
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

`task checkpoint` 不要求调用者管理 Task、Intent、Episode、boundary 或重试键；服务端根据当前 Session locator 解析这些状态。下面示例用一条自包含验证证据关闭 Episode：

```json
{
  "agent_kind": "codex",
  "external_session_id": "manual-demo-session",
  "claims": [
    {
      "context_kind": "validation",
      "statement": "旧客户端可以继续解析当前搜索响应",
      "rationale": "兼容性测试覆盖了旧版解析路径",
      "conditions": ["search-v2 response"],
      "evidence": [
        {
          "evidence_type": "experiment_record",
          "summary": "legacy_search_contract 使用当前响应夹具执行并通过",
          "limitations": ["只覆盖当前测试夹具，未验证未来 Schema"]
        }
      ]
    }
  ],
  "unknowns": []
}
```

```bash
sctx --json task checkpoint --input checkpoint.json
```

非空响应中的 `status=accepted` 表示 Checkpoint receipt 和 Candidate Build outbox 已持久化排队；它不表示 Candidate 已构建完成。若 ACK 丢失或超时，用完全相同的 Claims/Unknowns 重试；同一 Task/Intent scope 下会返回相同 operation、Checkpoint、Episode、Build 和 Submission 身份。Checkpoint ACK 和 same-content replay 都不写 Git。

列出待审核 Candidate：

```bash
sctx candidate list \
  --agent-kind codex \
  --external-session-id manual-demo-session
```

`candidate list` 会对当前 Task 的 pending/incomplete outbox 做有界恢复，因此可能追加供 Review 使用的、不可信 Candidate submission facts。只有后续显式 `candidate confirm` 才会创建 accepted Context revision、association、publication 和 confirmation facts。

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
| `sctx setup [--demo] [--embedding] [--agents cursor,codex] [--knowledge-store-url GIT_URL]` | 首次安装运行时、知识库、索引、MCP、Hook 和 Skill；可从已有非空远端 Store 克隆并幂等执行。`--demo` 是 #195 已接受的非核心已知限制，不作为安装验收。`--embedding` 在安装完成后顺带启用语义召回（见 3.7）；失败只记一条 notice，不会让 setup 失败。 |
| `sctx demo` | 建立并验证固定演示闭环；重复执行可复用已有演示数据。 |
| `sctx doctor` | 只读检查安装、索引、配置、MCP 和 Agent 能力。 |
| `sctx doctor --fix` | 重做安全、可逆的注册和索引设置后再次检查。 |
| `sctx doctor --recheck` | 只读评估所有 accepted Context 里结构化的 `recheck_when` 条目（见 6.5），把命中结果写入本机索引的 `stale_reason`；不能与 `--fix` 同时使用。 |
| `sctx upgrade [--agents cursor,codex]` | 安装新版本并原子切换 `bin/current`。 |
| `sctx data reset --dry-run` / `--yes` | 预览或确认事务式清空活动数据；保留安装结构、Agent 接入和默认恢复备份，不修改远端 Git。 |
| `sctx uninstall` | 精确移除安装器拥有的运行时和接入配置，保留知识库。 |
| `sctx maintain run [--opportunistic]` | 一次周期维护：重建工程图、清点待人工处置的 Candidate Review 与 provisional Space、同步知识库。每步独立容错，一步失败不阻断后续步；结果写入 `state/maintain-digest.json`，由 `sctx doctor` 读回。它只统计、不处置任何 Candidate（见 ADR-0005），也不做 `doctor --fix` 的重装和语义模型预热。`--opportunistic` 让同步遇锁即让路（只尝试一次），适合挂在有人等待的操作后面；不加则按 30/60/120 秒退避重试。 |
| `sctx maintain status` | 读回上一次维护运行的时间、每步结果和各项待处置计数。 |
| `sctx knowledge sync` | 显式收取远端默认/工作分支，验证并合并到本机 InstallationWorkBranch，只发布该工作分支。触及远端的 fetch/ls-remote/push 有 120 秒预算，超时即终止子进程并释放独占租约。 |
| `sctx knowledge delete ...` | 双重确认后永久删除知识 Git 仓库。 |
| `sctx embedding install [--model-url BASE] [--runtime-url URL] [--expected-sha256 SHA]` | 一条命令启用语义召回：下载模型与 ONNX Runtime、校验、自检、写 `[retrieval]`、回填向量缓存（见 3.7）。 |
| `sctx embedding status [--verify]` | 报告配置、文件、向量条数与模型指纹；`--verify` 才真正加载模型（9–12 秒）。 |
| `sctx embedding remove --yes` | 删除 `[retrieval]`、`~/.shared-context/embedding/` 与 `state/semantic.sqlite`。 |

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
| `sctx task context --agent-kind ... --external-session-id ... [--token-budget 2000] [--max-spaces 8] [--compact]` | 只读获取现有 Active Task 的相关 Context，不修改状态。Token Budget 最低 256，最多返回 32 个 Space。 |
| `sctx task artifact-focus --input <JSON>` | 针对一个文件、模块、符号、API、Schema 或测试做一次即时历史查询。Focus 不持久化，也不会成为证据。 |
| `sctx task checkpoint --input <JSON>` | 只提交 Session locator、完整 direct Claims/Unknowns；服务端解析 Task/Intent/lifecycle、关闭 Episode并持久化 queued Build receipt。 |
| `sctx task signal supersede --input <JSON>` | 把已经不再相关的活动 Signal 标记为 superseded；保留历史，不执行删除。 |

`task_boundary` 的规则：

- `continue`：仍是同一工程目标，包括修复、测试、范围调整和新增约束。
- `new`：用户明确切换到无关目标或新交付物。
- 拿不准时保留连续性，用 `continue`。

Artifact Focus 的 `locator_kind` 支持：`file`、`module`、`symbol`、`api`、`schema`、`test`。请求提供绝对文件路径；Repository ID 和仓库相对路径由本机 Catalog 解析，调用者不要猜。

Engineering Graph 整体不可用时，Artifact Focus 会尝试严格文本 fallback：只有安全 Context 同时包含精确 RepositoryId 与完整 kind-specific locator key 才返回，并用 `resolved_focus_text_fallback` 路径明确说明它不是 Graph 证据。它不按 basename 或相似名称猜测；Graph 已存在但结果为 missing、ambiguous 或 unreachable 时也不会启用。

Checkpoint Claim 必须严格包含 `context_kind`、`statement`、`rationale`、`conditions`、`evidence`；每条 Evidence 严格包含 `evidence_type`、`summary`、`limitations`；Unknown 严格包含 `statement`、`blocking`。不要额外提交 Task/Intent/Episode/version/boundary、transport key、关系/工程引用或调用者生成的 ID。

`task context` 和 `task_context`/`task_intent_update`/`task_artifact_focus` MCP 工具都支持 `detail_level: "compact"`（MCP 侧默认就是 `compact`；CLI 侧为保持与 Rust 直接入口一致，默认仍是 `full`，需要加 `--compact` 才切换）。`compact` 只保留可直接继承的字段：`statement`、`applicability.conditions`、截断到 200 字符的 `evidence.summary`、`relations` 和最多 3 条一句话 `why`；去掉逐项 `match_reason`、`retrieval_paths` 和 RRF 明细。需要看排序依据或调试召回时才用默认/`full`。`task_checkpoint` 本身只做一次持久 ACK（记录 Checkpoint 收据并把 Episode 排进 Candidate Build 队列），不在这一步解析文件路径。文件路径与类名到 Engineering Reference、`topic_key` 的确定性抽取发生在随后的 Candidate Build 阶段（Episode 关闭后、`candidate_list`/`candidate_get` 能看到结果前），对同一 Episode 只做一次、结果落盘复用；调用方不需要、也不应该额外提交这些字段。

### 6.4 Candidate 审核

| 命令 | 功能 |
|---|---|
| `sctx candidate list --agent-kind ... --external-session-id ... [--compact]` | 分页列出当前 Task 的 Candidate Review；默认只列 `pending`。可用 `--status`、`--limit`、`--cursor`、`--token-budget`。 |
| `sctx candidate get ... --candidate-id <ID>` | 获取完整草稿、证据、来源、冲突分析、置信度、未知项和 Space 推荐。 |
| `sctx candidate analyze --candidate-id <ID> [--token-budget 4096] [--top-k 16]` | 重新计算与已有 Context 的重复、支持、修订、潜在冲突和相关性分析；不写入 Git。 |
| `sctx candidate confirm --input <JSON>` | 用户明确确认后，把 Candidate、Primary/Related Space、最终 Context Revision、Association、Publication、Confirmation 和可选编辑作为一个原子事实批次写入。JSON 用 `candidate_id` 确认一个，或用 `candidate_ids` 数组原子确认多个（写入前对每个 Candidate 做完整校验，任何一个失败整批都不写；批量模式不支持 `edits` 和新建 Space 推荐，只能用于已有 Space）。 |
| `sctx candidate discard --candidate-id <ID> [--candidate-id <ID> ...] --reason <TEXT>` | 用户明确拒绝保留时丢弃 Candidate Review；不会发布任何 Context。重复 `--candidate-id` 原子丢弃多个自己名下的 Pending Candidate。 |
| `sctx candidate build-closed-episode --episode-id <ID>` | 在 Episode 已关闭但 Builder 响应丢失或待恢复时重建；属于恢复命令。 |

Candidate 状态支持 `pending`、`discarded`、`expired`、`confirmed`。只有完整分析且 `ready_for_review` 的 Candidate 才适合让用户决策。`potential_contradiction` 和 `unresolved_related` 是审核线索，不是已经成立的事实。

`candidate list` 默认（MCP 侧）和 `--compact`（CLI 侧）返回精简三角视图：每条只有 `candidate_id`、`kind`、`statement`、置信度最高的 `top_assessment`（`relation` + `confidence_basis_points`）、`primary_space_recommendation` 和 `ready_for_review`；不含完整证据、来源和分析明细。推荐的审核顺序是：先看这份精简列表，只对 `top_assessment.relation` 为 `potential_contradiction` 或 `revises` 的 Candidate 用 `candidate get` 展开完整 Review，再决定是否用 `candidate_ids` 批量确认或丢弃其余同批次的 `supports`/`exact_duplicate`/`novel` 项，避免逐条重复展开明显不需要人工细看的 Candidate。

同一 Task Intent Revision 产生多个 Claims 时，每个 Claim 仍是独立 Candidate，但它们共享一个 `ProposedSpaceGroup`：建议的新 Space 标题只来自 Working Intent 的 `goal`，会移除内部 `System suggestion:` 前缀、规范空白后按字符截断到 40 个字符并加省略号（不是按词边界截断，goal 为空时用固定标题 "Task intent"）。第一个 Candidate 确认创建新 Space 后，其他待审核 Candidate 会推荐该 Existing Space；新的 Intent Revision 使用新分组。这个机制不会按文本合并 Candidate，也不是全局 Active Space。这类系统生成的 Space 会带 `provisional` 标记，出现在 `space list`/`space get` 与候选的 Space 推荐里；当它积累的已接受 Context 达到一定数量，或出现跨 Space 的相关引用时，`candidate list` 顶层会给出合并/命名到人工 Space 的提示，人工执行一次 `space intent revise` 才会让它不再是 `provisional`。

确认时的 `edits` 可以只替换用户明确要求修改的字段：`kind`、`topic_key`、`problem_view`、`hints`、`statement`、`rationale`、`applicability`、`assumptions`、`recheck_when`、`relations`、`evidence`。省略字段表示保留原草稿；`topic_key`/`problem_view` 都要用 `{"action":"clear"}` 才表示显式清空，`hints` 直接给字符串数组整体替换。`problem_view` 省略且草稿本身没有值时，服务端会用来源 Task 的 Working Intent（goal/in-scope/未决问题）自动补一份摘要，不需要手工填写。

确认编辑中的 `relations` 只接受 `depends_on`、`constrains`、`implements`、`validated_by`、`contradicts`、`related_to`。Engineering Reference 在 Context 确认后通过独立的 `engineering-reference record` 记录；它必须使用已登记 RepositoryId、确定性 locator、非空 `supports` 和至少一项限制说明。图重建失败不会使已提交的确认事实重复创建。

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

`recheck_when` 里绝大多数条目仍是给人看的自由文本，但两种固定前缀会被服务端自动评估：

- `branch_advanced:<分支名>@<commit>`：分支已经不再指向该 commit 时判定过期。
- `file_changed_since:<commit>:<仓库相对路径>`：该文件在 commit 之后发生过变更时判定过期。

评估只在 `sctx doctor --recheck` 或 `sctx association rebuild` 之后执行一次，命中的 Context 会在本机索引标上 `stale_reason`，在 `search`/`task_context` 里仍然可见，但会被降权且排除出自动注入；这是纯本机派生状态，不写入知识 Git，也不会被其他机器看到，`sctx index rebuild` 之类的整表重建会把它清空，需要再跑一次 `sctx doctor --recheck` 才能恢复。

`context revise` 或 `candidate confirm` 的 `edits.relations` 里，`supersedes` 表示这条新 Revision 取代了某个已有 Context：被取代的 Revision 在 Git 里的事实不会被改写或删除，只是本机投影会把它标记为已被取代，排除出自动注入，但仍可以被显式 `search`/`context get` 查到、也能在 explain 里看到取代关系，适合"旧结论仍值得追溯，但不该再被继承"的场景。

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
| `sctx repository doctor` | 检查 checkout 是可用、缺失还是不安全，并在安全时同步 Registry；对每个早期安装留下的 legacy `rpo_<uuid>` 身份给出 typed 警告（`--json` 下稳定 `kind: "legacy_repository_id"`），并统计本机 Index 里还有多少条 Engineering Reference 引用该 legacy id。 |
| `sctx repository rename --from <OLD_ID> --to <NEW_ID>` | 只改本机 Catalog / repository-registry 里的 RepositoryId；`--to` 必须满足 ADR-0001 的 1–64 字节可读 ASCII 语法，目标已存在或来源不存在都返回 typed 错误。已经写入 Git 的 EngineeringReference 事件仍保留旧 id 原文，不会被重写——`repository doctor` 报告的引用计数就是用来衡量这批历史事件的规模。 |
| `sctx repository scan --checkout-path <路径> [--path <仓库相对路径>] [--max-artifacts 200]` | 显式扫描有界路径并返回工程对象摘要，不返回源码正文。最多请求 1000 个 Artifact。 |
| `sctx engineering-reference record --input <JSON>` | 把现有 Context Revision 与已验证的工程对象关系写入长期事实层。 |
| `sctx association explain --reference-id <ID>` | 解释一条 Reference 当前解析到了什么、依据是什么、是否存在歧义、有哪些图路径。 |
| `sctx association rebuild [--diagnose]` | 重新扫描有界计划并重建关联；`--diagnose` 只诊断，不落新的工程图快照。 |

支持的工程对象是 `module`、`file`、`symbol`、`api`、`schema`、`test`；关系是 `implements`、`defines`、`consumes`、`validates`、`constrains`、`depends_on`。

RepositoryId 是 1–64 字节、大小写敏感的可读 ASCII 名称，例如 `Android`、`iOS`、`FE`。首字符必须是字母，后续可使用字母、数字、点、下划线和连字符。团队成员应对同一逻辑源码仓库使用完全相同的 ID；本机 Catalog 会拒绝 `FE` 与 `fe` 这类仅大小写不同的重复身份，也不会从路径、basename 或 Git remote 猜测 ID。

`engineering-reference record` 只应在直接检查或验证后调用。它要求完整、确定性的 locator、非空 `supports` 和至少一条 `limitations`。不要用相似文件名猜移动或重命名关系。

例如多个 Android 仓库位于同一父目录，而你希望从父目录启动 Agent：只要每个仓库都用显式 Repository ID 登记过，从这个父目录启动就会自动为它们全部启用，不需要再登记这个目录本身。未登记的 sibling 不会因为在同一个父目录下就被猜成一个 Repository identity；Session 已准入后对安全未登记位置的调查最多形成非定位工程含义。

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

`--query` 默认按 `ranked` 模式匹配：只要命中任意 query token 就可能入选，排序综合 BM25 与 token coverage（命中 token 数 / query token 数），coverage 过低的结果会被截断；每条结果的 `match_reason` 会给出 `matched_tokens` 和 `coverage_basis_points`。`ranked` 模式下 query token 还会按已积累的别名表（来自代码标识符拆分与 Space 领域术语）做有限展开，例如查询里的一个缩写命中另一种拼法的同一标识符/术语时也能召回；被别名展开命中的 token 会在 `match_reason.matched_via_alias` 里单独列出，覆盖率仍按原始 token 计。需要旧的"全部 token 都必须命中、不做别名展开"的严格匹配时加 `--exact`（映射到 `SearchRequest.match_mode = "exact"`）。

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
| `sctx hook --agent cursor` | 从标准输入接收 Cursor Hook JSON，输出 Cursor 所需响应。通常由安装器写入且带 `--agent-version` 的 Hook 配置调用。 |
| `sctx hook --agent codex` | 处理 Codex Hook JSON。安装器会把 setup 时探测到的版本固化为 `--agent-version`，Hook 本身不再逐事件启动版本探测进程；通常不应手工调用。 |
| `sctx hook --agent <cursor 或 codex> --capabilities ...` | 报告 Agent 版本、Hook 可用性和信任状态。Codex 可用 `--trust <confirmed 或 unconfirmed>`。 |

**不做版本门控。** `--agent-version` 与 Cursor payload 里的 `cursor_version` 只作信息上报：它们原样透传到能力报告的 `detected_version`，不参与任何比较，也不会让系统降级。宿主的版本字符串本来就不统一——运行 Hook 的 Cursor CLI（`cursor-agent --version`）给的是日期形 `2026.08.25-3e8eec8`，桌面端 `cursor --version` 给的是 semver，Codex 给的是 `codex-cli 0.147.0`——安全性来自严格的 payload 解码器，不是版本比较。能力模式只由两件事决定：Hook 是否可用，以及 Codex Hook Trust 是否已确认。

| 条件 | 模式 |
|---|---|
| Codex Hook Trust 未确认 | `action_required`（Hook 全部关闭，MCP + CLI 可用） |
| Hook 可用 | `verified_hooks` |
| Hook 不可用 | `mcp_cli_fallback` |

能力报告里的 `fixture_profile_version` 是该 Adapter 的 payload 契约所对照的仓库内 fixture profile（Cursor `3.13.0`、Codex `0.147.0`），同样只是信息，不是最低版本要求。
| `sctx mcp serve --client cursor` | 通过标准输入/输出运行 Cursor MCP Server。 |
| `sctx mcp serve --client codex` | 通过标准输入/输出运行 Codex MCP Server。 |

安装后的 MCP 一共暴露 17 个工具：

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
17. `space_create`

CLI 还提供 Space/Context 写入治理、语义冲突、索引和 Pending Batch 等管理员能力；这些没有全部开放成 Agent MCP 写工具，以维持显式审核和生命周期边界。

当前 Repository 准入控制 Hook 的 Agent-visible activation 与机械 TaskSignal 路径；MCP Server 也用 current Enabled Session lease 实现授权校验，Disabled/缺失/锁忙/损坏 Session 的调用会被拒绝。已安装的全局 Skill 主入口只包含最小 activation gate：没有可信 SessionStart marker 时不读取完整 workflow reference、不产生 Shared Context MCP 调用提示；有 marker 时才完整读取一次 installer-owned reference。这个 Skill gate 是 Agent 推理前的指令准入机制，Server guard 则负责安全和不落越权数据。MCP 进程和工具 Schema 仍由用户级 Agent 配置提供，可能物理启动或可见；不要把 Disabled 理解为进程必然未启动，也不要把合约中的 reference-read/MCP-call 字节代理外推为真实计费 token 已被测量。当前还已证明 Disabled Hook 不向模型注入 Shared Context 文本，也不产生业务 Runtime/Report/知识 Git 记录。

#214 更新后的固定 bytes proxy 进一步量化该边界：Disabled 的 Agent-visible activation、完整 workflow read、Shared Context MCP call/result 与业务 residue 都是 0；Enabled 每个 SessionStart marker 的固定部分为 249 bytes，加上 `agent_kind` 与两处 host session id（36 字符的 UUID 会话约 326 bytes，硬上限 512 bytes），完整 workflow 读取一次，并在固定的单仓库/共同父目录验收链中产生 5 次真实 public MCP 调用。当前最小 gate、workflow、metadata 源文件分别为 1882、18671、263 bytes。若 Enabled Session 在没有 ActiveTask 时先发生安全 PostToolUse，Hook 只提醒一次调用 `task_intent_update`，不读取 Prompt、不自动创建 Task；PreCompact/TurnStop 只给出 bounded Checkpoint guidance并尝试恢复已有 outbox，不创作 Claim。这些值用于回归比较，不是 tokenizer 结果或供应商计费 token。

### 6.11 可选 `config.toml` 设置

安装根目录下的 `config.toml`（`~/.shared-context/config.toml`）可以手工添加以下可选表，不写就是默认行为：

```toml
[hooks]
artifact_focus_reminder = false

[context_ttl]
validation = "30d"
progress = "14d"

[retrieval]
embedding_model_path = "/absolute/path/to/bge-m3-onnx"
embedding_runtime_path = "/absolute/path/to/libonnxruntime.dylib"
```

- `[hooks] artifact_focus_reminder`：默认 `false`（关闭）。关闭时 PostTool Hook 与该开关引入前逐字节一致。显式改成 `true` 后，仅对被识别为单个文件操作的工具事件，用本机 Catalog 把绝对路径解析到已登记 Repository（该 Repository 必须在本会话准入范围内，所以父目录会话会按文件选对仓库），再对 `engineering.sqlite` 做一次只读查询（不加锁、不跑 Git、不 scan、不 rebuild）；命中已接受且可自动注入的 Graph Context 时，追加一条不超过 800 字节的提示（最多 3 个 Context ID、每个标题截断到 60 字符，加一句固定的“可以调用 `task_artifact_focus` 查看”提示文案），不包含 statement/evidence 正文，也不写任何事实。同一 Session 对同一文件只提示一次。这个开关只影响以绝对文件路径命中的工具事件，不覆盖模块/符号/API/Schema/测试等其他定位方式。
- `[retrieval]`：可选的 embedding 召回通道（ADR-0004）。**两个键都不写就是默认：通道完全不存在，检索与加入该通道之前逐字节一致，零磁盘、零内存、零延迟开销。** 两个键必须同时写、且都必须是绝对路径；只写一个视为配置错误（`sctx doctor` 会 Warning，通道保持关闭）。**不需要手工写这一节**：`sctx embedding install`（见 3.7）会下载、校验、自检后替你写好；下面的说明是给自备模型或非 macOS 平台的用户看的。
  - `embedding_model_path`：模型目录，需包含 `model.onnx`（若是拆分导出还需同目录的 `model.onnx_data`）与 `tokenizer.json`。推荐 bge-m3 的 ONNX 导出（约 2.1GB 磁盘、约 1.2GB 常驻内存）。模型不随包分发；`sctx embedding install` 会下载到 `~/.shared-context/embedding/model/`，也可以自行下载后手工指向别处。
  - `embedding_runtime_path`：本机 ONNX Runtime 动态库（macOS `libonnxruntime.dylib`、Linux `libonnxruntime.so`）。构建期不下载任何二进制，运行时才按此路径加载。`sctx embedding install` 会解包到 `~/.shared-context/embedding/runtime/`。
  - `embedding_encode_budget_ms`（可选）：单次查询编码的时间预算，毫秒，接受 50–30000。不写就用内置默认值 1200ms。这个默认值是在 Apple Silicon 上按真实 Working Intent 长度实测标定的（283 字符查询 p95 235ms，512-token 截断上限处 p95 816ms）；明显更慢的机器需要调大，判断依据见下一条的 `sctx doctor` 提示。它是上限不是常态开销：一次典型查询远在预算内返回，重复查询直接命中进程内缓存。
  - 开启后：`sctx mcp serve` 启动时由后台线程加载模型（一次性 9–12 秒）并把已接受 Context 的向量写入 `state/semantic.sqlite`（可随时删除的本地缓存，不进 Git、不进 `index.sqlite`，按模型指纹与 ranking 版本键控）。自动注入的查询会额外走一路余弦召回（阈值 0.52、最多 16 条），与词法通道一起做 RRF 融合；语义命中本身构成一条独立的注入资格路径。查询向量另有一个进程内 LRU（64 条），所以同一个 Working Intent 被反复检索时只编码一次。模型未就绪 / 加载失败 / 单次编码超过预算时，该路静默降级为 `omitted.reason = "embedding_unavailable"`，词法结果不受影响；超时的那次编码在后台跑完后仍会写进查询缓存，所以同一个 Intent 的下一次调用会直接命中。
  - 怎么知道预算够不够：最近 64 次编码的耗时分布与超时次数记在 `semantic.sqlite` 里，`sctx embedding status` 的 `encode_latency` 字段会输出（`samples`/`timed_out`/`p50_ms`/`p95_ms`/`max_ms`）。超时占多数时 `sctx doctor` 的 `retrieval_embedding` 检查会从 Ok 变成 Warning，并直接给出调大 `embedding_encode_budget_ms` 的建议——通道「装好了但每次都超时」在 Pack 里和「没装模型」长得一模一样，这是唯一能把两者区分开的地方。
  - 显式 `context_search` 本轮不接入该通道，保持纯词法。
- `[context_ttl]`：按 Context 类型（`decision`/`contract`/`issue`/`risk`/`validation`/`discovery`/`progress`）配置一个带单位的正时长（`s`/`m`/`h`/`d`/`w`），不配置的类型没有时效。到期起点是该 Context 被接受时所在 commit 的时间，不是本机当前时间。过期后状态变为 `historical`：排除自动注入，仍可以被 `search`/`context get` 查到。

## 7. 常见问题

### 7.1 `sctx` 找不到

从源码本地安装时，确认使用了安装命令最后打印的 PATH。默认可执行文件位于仓库的 `target/npm-local/bin/sctx`。重新打开终端后，需要把该目录加入 shell 启动配置，或者使用绝对路径执行。

### 7.2 `doctor` 提示 Hook 未验证或需要信任

先重启 Cursor/Codex，再运行：

```bash
sctx doctor
```

如果报告为 `ACTION REQUIRED`，按报告中的 Agent 能力提示完成信任设置，再运行 `sctx doctor --fix`。Hook 不可用时系统会降级；正常编程不会被阻断，但 Agent 应在结束前用 flat `task_checkpoint` 直接提交完整 Claims/Unknowns。`doctor` 报告里的 `adapter_capability.*` 只反映 Hook 可用性与信任状态，附带上报检测到的版本；版本本身无论是什么形态都不会把检查降级为 Warning。

### 7.3 报 `intent_stale` 或 Review 版本过期

这是并发保护在生效，不是数据损坏。重新读取当前 Task/Candidate，使用最新的 `intent_revision_id` 或 `review_version`，核对内容后重试。`task_checkpoint` 不接受调用者提供的 Task/Intent/Episode CAS；同 scoped content 应原样重试，不要猜 ID 或添加重试键。

### 7.4 搜不到某个文件的历史 Context

依次检查：

```bash
sctx repository list
sctx repository doctor
sctx association rebuild --diagnose
```

确认仓库已经登记、路径仍存在、相关 Context 已经记录 Engineering Reference，并且工程图已经显式构建。`artifact_not_reachable_in_graph` 只表示当前历史图没有安全的精确路径，不代表当前源码文件不存在。

### 7.5 在多个仓库的父目录启动时为什么没有激活

从父目录启动会自动为它下面所有已登记仓库启用，不需要登记这个目录。没有激活通常是下面三种情况之一：

1. **成员还没登记**。用 `sctx repository list` 核对；缺哪个就 `sctx repository add --repository-id <ID> --path <绝对路径>` 补上。
2. **启动目录下面一个已登记 checkout 都没有**（例如漂移后的旧路径，或只放着未登记 sibling）。`sctx repository doctor` 会指出 checkout 是缺失还是不可用。
3. **启动目录是被保护的位置**：文件系统根 `/`、你的 HOME 目录本身，或 HOME 的上一级。从这些位置推导会把整机所有已登记仓库一次性拉进来，与"只在登记过的仓库下记录"相反，所以默认不启用。确实需要时可在 `~/.shared-context/config.toml` 写：

   ```toml
   [activation]
   allow_home = true
   ```

   它只解除两条 HOME 保护，文件系统根永远不会用这条规则启用。

登记变化不需要新开 Agent Session：正在运行的会话会用它的启动目录对新的 Catalog 重新判定，下一次 Hook 事件或 MCP 调用即生效——父目录会话会因此多出或少掉一个仓库。会话启动目录本身不会被改写，所以在未登记目录启动的会话即使后来 `cd` 进已登记 Repo 也仍然是 Disabled——那种情况才需要新开会话。

### 7.6 Candidate 没有生成

常见原因是提交为空而得到 `no_op`、Checkpoint 只有 Unknown、Build outbox 仍为 pending/incomplete，或 Candidate recovery 失败。先查看 `task checkpoint` 的 queued receipt，再调用 `candidate list` 触发有界恢复；如果已知 Episode 仍需显式恢复，可使用：

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

团队共享的固定端到端验收使用两个独立安装、同一个 `FE` RepositoryId、两个不同 checkout 和一个本地 bare remote，覆盖 A 发布工作分支、人工合入默认分支、B 同步并按 B 本地路径命中同一 Context，以及 B reset 不修改远端：

```bash
cargo test -p sctx-installer --test installer_matrix \
  fixed_two_installation_team_sharing_and_local_reset_oracle
```

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
