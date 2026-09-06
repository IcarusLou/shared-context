Shared Context 日志追踪执行方案

本方案基于 2026-09-05 的实际代码，核查基线为 `9174478ca3f18a71c351cba496fcef3f116d8677`。三个 subagent 分别检查了采集入口、Git 存储和安装调度；结论未以仓库既有文档作为实现依据。对应代码已开发，日常使用见[日志采集、同步与排障](./logging.md)。本地已用真实 `sctx` 子进程和临时 bare Git 远端验证采集、封口、上传、缓存删除与重建；公司 GitLab 和真实宿主回调环境仍需在部署前验证，没有接入生产远端。

**1. 决策与交付边界**

采用「业务入口非阻塞投递 → 独立采集进程 → 本地不可变批次 → 独立 Git 同步命令」的结构。日志的生成、过滤、归档、检索和同步均由确定性程序完成，不调用模型，不加载 embedding，不新增需要 Agent 执行的日志工具。

稳定性优先于日志完整性：采集器缺失、拥塞、权限不足或磁盘故障时允许丢日志；任何新增日志故障不得改变业务结果、协议输出或业务退出码。非阻塞指不等待采集器、队列空间或网络，不承诺操作系统调度和路径查询的硬实时上限。

远端采用一个专用 GitLab 仓库。公司能承载 40–50 GB monorepo 是本方案的环境输入，不能据此假定该日志仓库的推送权限、单次推送限额和分支策略已配置。远端容量不作为当前否决项；客户端不下载远端全量日志。

第一版覆盖 CLI、已启用的 Hook、MCP 工具及协议错误、维护步骤；能够查询行为结果、耗时、错误类别和现有业务关联 ID。缺少结束事件只能标记为“未观察到结束”，不能断言进程崩溃或用户放弃。

**2. 实际代码发现与对应改动**

| 已核实的代码 | 实施决定 |
| --- | --- |
| [CLI main/run](../crates/cli/src/main.rs) 的总入口和 `requires_shared_maintenance_guard` 白名单 | 在总入口外层添加命令边界事件；`logs` 不进入业务锁白名单。只记录命令枚举，不记录 argv。Hook 使用专用入口，logs 自身不递归采集。 |
| `HookEventRecorder::flush` 当前同步调用 [TaskRuntime::record_hook_event_at](../crates/task-runtime/src/lib.rs)，写业务 `runtime.sqlite`；专用 busy timeout 为 3 ms，每 64 次插入剪枝，约保留 5000 行 | 保留现有 recorder 调用点，替换其后端为非阻塞投递，停止新版本的诊断 SQLite 写入。不能以长期双写声称隔离完成。 |
| `run_doctor_hooks` 是旧 Hook 诊断的唯一生产读取入口 | 同一交付中迁移 doctor 读取，保持现有 counts/recent_events 字段含义；旧表不删除，供明确标注的旧版本只读兼容。 |
| 正常 Disabled Hook 不记录完成事件 | 保留现有语义。不能用 CLI 通用记录器覆盖全部 Hook，导致未启用工作区开始产生行为遥测。 |
| [MCP tools_call](../crates/mcp/src/lib.rs) 将业务失败包装为 `Ok(Value)`，协议值带 `isError: true` | 在内部 `ToolResult` 转换之前记录成功/失败，读取 `ToolFailure.code/family`；不能用外层 Result 是否为 Err 判断业务成功率。 |
| MCP `serve/dispatch/serve_stdio` 对传输、协议、业务错误有不同包装 | 在对应边界分别记录，避免把 InvalidFrame、UnexpectedEof、FrameTooLarge 统一丢成 external。 |
| [主配置 ConfigDocument](../crates/local-state/src/config.rs) 使用 `deny_unknown_fields` | 日志配置独立文件，不往主配置增加 `[logs]`，避免旧版本回滚或日志配置损坏影响业务。 |
| [sync_knowledge](../crates/installer/src/lib.rs) 持业务独占 lease、setup.lock，且做领域校验和索引操作 | 新建日志 Git 同步实现，不调用 knowledge sync，不使用 GitStore 的业务流程。 |
| [GitStore Git runner](../crates/git-store/src/git.rs) 无超时；installer 的 timeout runner 仅杀直接子进程，管道输出无界 | 新增有整体 deadline、进程组终止、限量输出的日志专用 runner。 |
| maintain 前三步释放共享锁，随后 knowledge sync；步骤失败写摘要而不使 CLI 返回失败 | 在业务锁释放后添加日志子进程调度，日志结果单独进入 MaintainStep；独立 logs sync 保持真实失败退出码。 |
| 安装中的现有 maintenance plist 写入在主事务内，失败会传播 | 日志安装使用独立小事务，放在业务安装完成且主锁释放之后，失败只增加 notice。 |
| manifest.author 只保存全局 Git 邮箱的 @ 前部分，知识库本地邮箱固定为 shared-context@localhost | 初始化独立读取完整全局 user.email，显式 --email 优先；不要复用 manifest.author 或知识库 local Git 邮箱。 |

旧 Hook detail 可能含绝对路径，不能原样上报。已有 [PrivacyScanner](../crates/local-state/src/privacy.rs) 是离线确定性实现，可在采集器内复用；它不替代路径策略。CLI 现有 error_code 对 StaleState、PrivacyRejected 的映射不完整，新日志使用完整的 typed ErrorKind 映射。

**3. 采集通道：先验证运行权限，再实现存储**

初稿的 Unix Datagram Socket 不能直接定案。本机实验结果如下，属于可行性实验，不是产品性能基准：

| 实验 | 观察 |
| --- | --- |
| 当前 macOS 沙箱内 bind/sendto Unix socket，包括向沙箱外接收器发送 | EPERM，不能假定该通道在所有业务宿主中可用。 |
| 沙箱外 Datagram 发送 | 512/1024/2048 字节可发送；4096/8192 字节返回 EMSGSIZE。 |
| 沙箱外接收器暂停读取 | 1 KiB 消息排入 3 条后返回 ENOBUFS；该次循环约 0.032 ms。 |
| 同一沙箱内非阻塞 FIFO | 可创建、打开、读写；PIPE_BUF 实测 512 字节。 |
| FIFO 无 reader、队列满、reader 关闭 | 分别返回 ENXIO、EAGAIN、EPIPE；满队列实验写入 16 个 512 字节帧后返回，循环约 0.067 ms。 |

第一版选择非阻塞 FIFO 作为待宿主验证的传输方式。POSIX 的小帧原子写语义可参见 [pipe 手册](https://man7.org/linux/man-pages/man7/pipe.7.html)。P0 必须在真实 Cursor/Codex Hook、MCP 和 CLI 启动方式下验证写权限与丢弃行为；本机实验不代表所有安装环境。若某入口被宿主禁止 IPC，显示该入口未接通，不能静默改成同步文件写入或扩大主流程权限。

客户端约束：

- FIFO 位于每用户独立、由采集器管理的本机运行时目录；父目录 0700、FIFO 0600。实现固定使用 macOS 的 `/private/tmp`（其他 Unix 为 `/tmp`），按有效用户 ID 和日志根摘要隔离，不依赖 `TMPDIR`，生产者和采集器均验证 FIFO ownership 和私有权限。部署时验证宿主对该端点的访问权限，不使用网络挂载目录。
- 客户端不创建目录、不读取完整日志配置、不启动采集器，不调用 Git；缺少端点即丢弃。关闭采集时由管理命令关闭接收端。
- 使用 safe `OpenOptionsExt`，`O_NONBLOCK | O_NOFOLLOW`，不开 create/truncate；打开后确认对象是 FIFO，拒绝普通文件。通过窄依赖获取常量/系统安全封装，保持 workspace 的 `unsafe_code = forbid`。
- 每个事件一个不超过 512 字节的帧，包含 magic、协议版本和长度；采用紧凑的有界编码。必须一次 `write`，禁止 `write_all`、阻塞重试、BufWriter 或分开写头部和正文。
- 主路径只发送枚举、小数字、固定长度标识和少量限长字段。放不下时先去掉可选摘要，不能截断成坏结构。复杂格式化和 JSONL 构造在采集器完成。
- 一次 emit 执行 open/write/close，避免长期 MCP 缓存 FD 连接到已退出采集器的旧 FIFO。失败在当前调用结束，不同步恢复。
- 验证 sctx 二进制在 reader 退出时不会因 SIGPIPE 结束。Rust 当前默认忽略 SIGPIPE，但必须做实际子进程测试，不能仅靠语言惯例；不为日志随意修改全局信号行为。[Rust 官方说明](https://doc.rust-lang.org/nightly/unstable-book/compiler-flags/on-broken-pipe.html)
- 不改变 stdout/stderr；日志发送错误不再次进入日志链路。发送成功只代表入内核缓冲，不代表持久化。

采集器是独立的 `sctx logs collect` 进程。用独立 LaunchAgent 托管，限频重启；不绑定某个 MCP 连接。读取 FIFO 时处理半帧/多帧，限制解析缓冲，坏帧丢弃并重新寻找帧头。非阻塞读端加独立 dummy 写端避免短命 Hook 结束引发 EOF 忙循环，不依赖 FIFO O_RDWR 的跨平台行为。

采集器内部使用有界缓冲；磁盘慢时可使缓冲及 FIFO 满，从而使生产端丢弃，而不是反压业务。采集器不得持有业务 SQLite 或 Git 锁。资源限制用于限制影响范围，不承诺在整个操作系统失效、全机 OOM 等情形下仍能保证业务运行。

**4. 日志格式和可用性分析**

落盘使用 JSONL，封口批次附带 manifest：schema_version、batch_id、事件时间范围、事件数、内容字节数、SHA256、采集器版本。批次身份产生后固定，重试不能生成新身份。

统一字段按来源分为：

- 采集器补充：完整邮箱、installation_id、stream_id、平台、采集器版本、接收时间。
- 发送端提供：程序版本、入口类型、event_kind、invocation_id、进程内 sequence、发生时间、duration、结果枚举、错误码及少量业务计数。
- 按需关联：宿主 session 的稳定摘要、task_id、task_session_id、episode_id、checkpoint_id，以及已有业务 operation_id。

`invocation_id` 是本次 CLI/Hook/MCP 调用身份，不能占用已有业务 operation_id 的名称。业务 ID 缺失时留空，不增加数据库查询，也不修改业务函数签名去强行传递。宿主 session 可能包含多个并发任务，不能把它当唯一业务链。未经授权的请求标识与已验证标识要区分。

首批事件清单：

| 事件 | 必需结果 |
| --- | --- |
| CLI operation started/finished | 命令枚举、耗时、typed outcome/error；跳过 help/version 和 logs 自身。 |
| Hook decision | 复用已有 enabled/fail_open/degraded、reason 和耗时；正常 disabled 不新增事件。 |
| MCP tool started/finished | tool 枚举、授权状态、业务成功/失败、code/family、耗时。 |
| MCP protocol/transport failure | 结构化类别，不带原始报文。 |
| 维护步骤 finished | 步骤、结果、重试次数、耗时。日志同步本身写自己的有界状态。 |
| 结果摘要 | 仅从已有响应取检索结果数、replayed、candidate_build.status 等，不额外执行业务查询。 |

邮箱仅作为配置中的明确身份字段使用；其他自由文本仍经过过滤。默认不上报完整 Prompt、对话、代码、工具参数/输出、环境变量、凭证或绝对路径。未知错误保留 code/stage，摘要处理失败则舍弃摘要。主流程不执行全量文本正则扫描。

本地 `logs status --json` 报告接收和落盘计数、可观察的丢弃计数、最近心跳、积压量、最旧批次时间、上次上传时间和错误码。采集器停机期间所有短进程丢失的事件无法完整计数，状态必须区分“0 次失败”和“无观测”。

doctor --hooks 改读独立、可重建的有界诊断视图：24 小时按 reason 的计数、最近 5000 条 Hook 事件。该视图在日志根的 state 内，周期性原子落盘，有容量上限，不依赖保留已上传日志原文。旧表只在显式兼容路径下读取，标明 source 和时间，不将旧数据伪装成当前状态。

为支持无需模型的分析，后续同一执行计划内提供只读 `logs report --input <导出的JSONL目录> --json`：按版本/入口/错误码统计次数、失败率、空结果率和耗时分位数；`logs trace --input <目录> --invocation <ID>` 输出关联事件。两者不隐式下载整个 GitLab 仓库；导出端明确选择设备、分支和时间范围。第一版不做 dashboard、告警平台或自动用户满意度判定。

**5. Git 布局与本地空间回收**

本地结构：

```text
~/.shared-context-logs/
  config.toml
  state/
    collector-status.json
    hook-diagnostics.json
    upload-journal.json
    receipts/
    sync.lock
    service-ownership.json
  spool/
    active/
    ready/
  repository/                  # 当前分片缓存；包含 .git，可整体重建
```

远端分支示例：

```text
logs/<installation-id>/<stream-id>/2026-09/0001
logs/<installation-id>/<stream-id>/2026-09/0002
```

每个分支的目录树：

```text
users/alice@example.com/<installation-id>/2026-09-05/<batch-id>.jsonl
users/alice@example.com/<installation-id>/2026-09-05/<batch-id>.manifest.json
```

完整邮箱采用可逆的安全路径编码，禁止路径分隔符、`.`/`..` 等解释；域名规范化，但不擅自把本地部分合并成同一个用户。每次 init 固定邮箱，变更邮箱/remote 时开启新 stream，已有批次仍绑定原身份和原目标，不能重新归属后上传。每个 stream 在 state 中保存不可变的目标配置（可解析的 remote URL/引用、邮箱和身份，不内联凭据），不能只存摘要；旧目标配置保留到待传批次清空。完全未配置身份的本地批次标为 unassigned，只允许首次显式 init 绑定并报告归属，后台 sync 不猜测身份或目标。

优先复用安装 manifest 的稳定 installation_id，复制到日志配置后运行时不读主 manifest。没有安装 manifest 时 logs init 自行生成。stream_id 用于本地上传状态重建；状态丢失后新建 stream，避免错误复用旧分片序号。备份克隆的两个 writer 若共享身份，按非快进冲突检测，不覆盖对方；修复时给克隆实例新 stream。

每个分支累计只追加，达到以下任一条件即封存：跨 UTC 月、累计未压缩日志及 manifest 达到 128 MiB，或批次文件数达到 4096。追加前计算“已有分片 + 本次新文件”的字节和文件数，放不下整个批次就先轮换；非快进 fetch 后也重新计算。下一个分支是独立 orphan 空根，不能从旧分支带入旧 tree/history。UTC 月份使用同步分片时间，文件日期用事件时间，离线跨月批次无需改写身份。

本地只克隆当前设备当前分片：`--depth=1 --single-branch --no-tags --branch <精确分支>`；以后 fetch 也使用 `--depth=1 --no-tags` 和单个精确 ref，不 unshallow、不拉取默认 main 或其他用户 refs。未推送 batch 以 spool/journal 为恢复依据，更新远端基线前不能丢掉这两者。新分片直接 git init 并创建新的根提交。该设计不依赖公司 GitLab 的 partial clone 开关。[Git clone 参数语义](https://git-scm.com/docs/git-clone)

这个布局意味着：GitLab 上看历史需要切换设备/月份分支；默认 main 不展示所有人的全部日志。main 只保存静态说明，不让所有客户端并发更新全局索引。邮箱目录用于组织，不提供目录级访问隔离。

已在本地 bare remote 上验证：另一个用户分支含 1 MiB 随机 blob；单分支浅克隆设备 A 后追加并 push，重建空缓存后再追加并 push。远端保留三次提交、三批文件均在最新 tree；缓存没有其他用户 blob；旧 writer 的分叉推送被非快进拒绝。尚未验证公司的 GitLab 分支策略、认证和吞吐。

清理规则：

1. 远端包含批次且本地 durable receipt 写入成功后，立即删除对应 ready 原始文件，不必等到定期清理。
2. 当前 Git 缓存仍含工作区副本和 Git objects，因此删除 spool 不等于释放全部空间。Git 历史对象不会因删除当前文件自动消失。[GitLab 对仓库体积的说明](https://docs.gitlab.com/user/project/repository/repository_size/)
3. 分片轮换时回收旧缓存；当前缓存自创建起最多保留 7 天，在成功同步后的清理步骤整体删除 repository（工作区和 .git）。这是“创建年龄”，不是每次使用都续期，避免每天同步导致永不清理。
4. `logs prune --cache` 允许用户立即回收已确认状态的缓存。不得执行 git rm/commit 把本地清理传播为远端删日志。
5. 缓存删除不触碰 state、配置或未上传 spool。删除前核实受管 ownership、规范化日志根和实际 Git 目录；拒绝符号链接、外部 gitdir 和非受管用户仓库。存在尚未核实的上传 journal 时，先恢复上传状态，普通 prune 不做破坏性处理。
6. 下次同步只重新下载当前分片，内容预算约 128 MiB，另有 Git 元数据、工作区及临时 pack。完整空间回收的代价是再次下载这一分片，不是全公司日志。

默认空间预算建议：ready/active 合计 256 MiB，分片 128 MiB，单批 5 MiB，独立诊断视图 16 MiB，日志根总目标 1 GiB，磁盘剩余低于 1 GiB 时停写/停扩容。128 MiB 不是本地总量的硬上限；同步预留工作区、objects、pack 和临时缓存的空间，并在运行中监测预算、超限终止 Git。不能用一次事前检查承诺严格全机磁盘配额。

达到预算时先回收可重建的已确认缓存。未上传 spool 满后默认停止接收新日志，保留已有未上传批次，状态显示 dropped/storage_pressure；不悄悄删除未上传数据。空间恢复后自动恢复采集。只有对应 spool 已不存在且无 journal 引用，receipt 才能压缩为有界历史摘要；源文件删除失败时保留其回执。不无限保留已完成批次的回执或 daemon 自身 stderr。

**6. 同步状态机与异常恢复**

`sctx logs sync` 只依赖日志配置、spool、日志锁和 Git，可在业务配置或 SQLite 损坏、业务独占锁被占用时执行。

状态机：`active → ready → prepared → committed → remote_confirmed → receipt_durable → local_pruned`。

1. 尝试获取 sync.lock，忙则立即 `skipped_busy`。该锁不被采集器用于日常写入；采集器和 sync 只通过已封口文件交接。
2. 采集器按 5 MiB 或 60 秒封口，将 JSONL 和 manifest 写入同一个批次目录；完整 flush/fsync 文件和目录后，整体同文件系统 rename 到 ready，再 fsync 父目录，不能分别发布两个文件。手动 sync 可发 seal 请求并有界等待最多 2 秒；响应失败则同步已有 ready，报告 active 尚未包含，不并发读取正在追加的文件。
3. 选取本次预算内的 batch，校验完整性；在 Git 缓存外原子写 journal，包含 remote 身份摘要、stream、分支、批次 ID、路径、字节数和 SHA256。
4. 初始化或恢复当前分片 cache。只追加受管唯一文件路径，显式 git add -- <路径>，不运行 git add .。同路径同 SHA 视为已存在；同路径不同 SHA 报冲突，不能覆盖。
5. 创建 commit，持久化其 OID，再普通 push 到精确 ref；禁 force、mirror 和自动合并到知识库。非快进时仅 fetch 当前 ref，在最新树上重新应用未确认 batch，最多重试一次。
6. push 后核实远端 ref。若与本次 OID 相同，确认成立；若已前进，则获取对应树，校验实际 JSONL blob 的 SHA256 及 manifest 一致性，不能仅信 manifest 声明的 hash。只凭本地 commit 或 journal phase 不认定已上传。
7. 确认后在 cache 外持久化 receipt，再删除该批 spool，最后清理 journal。写回执失败时保留原始批次；远端成功但网络断开/本地崩溃属于结果未知，下次核实固定 batch，不另造文件名重复上传。
8. 全部状态清楚后执行本地 cache/receipt 清理。历史已确认分支突然不存在，报告 remote_history_missing，不静默重建为少了旧日志的新分支。

采集器恢复 active 时只保留完整 JSONL 行，截去未完成尾部，再生成可验证批次；不能把半批文件上传。Git 工作区是可重建缓存，未上传源文件和 journal 才是恢复输入。

并发边界：采集器拥有 active，sync 拥有选中的 ready；prune 与 sync 共用日志锁。上传中的 pinned batch 不被任何清理策略移除。磁盘压力下采集器停接新日志，不竞争删除 sync 正在处理的数据。

同步默认总时限 60 秒、每次最多处理 20 MiB 新日志，时限包含准备、认证、fetch、push、核实和清理。积压超过一批时返回 `partial` 和剩余数量；手动命令可重复执行，定时任务处理下一批。日志初次 fetch 当前分片的下载量不等于 20 MiB 新日志预算，仍受总时限与本地空间预算约束。

Git/launchctl runner 使用独立进程组、非交互凭证方式、stdin null、限长 stdout/stderr；网络或 credential helper 挂住时终止同组派生进程并回收。避免无界 read_to_end 和超时后 join 永不结束的 reader。不能借自动同步打开密码弹窗；认证失败由 status 提示用户修复已有 Git 凭证。超时、权限和坏远端只改变日志状态。

**7. 命令、配置与生命周期**

拟新增命令，当前均未实现：

```bash
sctx logs init --remote <GIT_URL> --email alice@example.com
sctx logs sync --json
sctx logs status --json
sctx logs prune --cache --json
sctx logs doctor --probe --json
sctx logs enable
sctx logs disable

# 由服务管理器调用的内部入口
sctx logs collect

# 离线确定性分析，不触发模型或全仓库下载
sctx logs report --input <JSONL目录> --json
sctx logs trace --input <JSONL目录> --invocation <ID> --json
```

统一支持 `--logs-root`，供自定义安装和测试使用。未配置 remote/邮箱时可本地采集，但主动 sync 返回配置错误；维护任务报告 unconfigured/skipped。日志根不存在时普通业务命令不得自动创建。

示意配置位于 `~/.shared-context-logs/config.toml`：

```toml
schema_version = 1
enabled = true
email = "alice@example.com"
remote = "<GIT_URL>"
installation_id = "<stable-installation-id>"
stream_id = "<generated-stream-id>"

[sync]
on_maintain = true
timeout_seconds = 60
max_new_payload_mib = 20
max_retry_count = 1

[storage]
batch_max_mib = 5
seal_interval_seconds = 60
shard_max_mib = 128
shard_max_files = 4096
spool_max_mib = 256
local_budget_mib = 1024
min_free_disk_mib = 1024
cache_max_age_days = 7
```

显式 logs init 对自身失败返回错误，不掩盖配置问题；它不回滚或修改已工作的业务安装。setup/upgrade 在业务事务完成、setup.lock 和 maintenance lease 释放后，对已配置日志做独立 best-effort 安装/更新，所有日志故障进入 notices。无日志配置的老安装不凭空猜测远端。

独立服务使用稳定 `<root>/bin/current/sctx logs collect`；ownership 保存在日志根，不混入主 manifest。先完成独立配置事务再激活，激活有超时。更新或卸载只操作仍匹配安装记录的 plist，保留用户修改过的文件。升级不成功启动日志服务不改变业务升级结果。

maintain 增加 logs_sync 子进程：前三步释放业务 lease 后，由独立的有界 supervisor 启动日志子进程，随后主维护流程照常执行 knowledge sync，最后收取日志结果。日志子进程及其 Git 有自己的总 deadline，不能让日志的超时控制依赖 knowledge sync 返回；启动失败直接形成日志步骤失败。业务安装完全不可用导致 maintain 提前返回时，由独立 logs sync 支持排障，不承诺 maintain 必然执行。日志步骤失败进入摘要，不跳过/回滚其他步骤，也不修改现有 maintain 对步骤失败仍返回成功的语义。

已有定时任务是每日 06:00，SessionStart 机会补跑默认 24 小时陈旧阈值。第一版复用这一频率，不声称日志自动每分钟上报。希望更及时的团队可把 `sctx logs sync --json` 加入现有外部调度；单实例锁兼容用户手动触发。

uninstall 有界停日志服务、移除受管 plist，但保留日志根和未上传数据；日志清理命令不删除业务根。logs disable 关闭采集与自动同步，保留现存批次，显式 logs sync 仍可上传这些历史批次。日志 warning 不把 doctor 的业务状态改成不可用。

**8. 实施拆分与依赖顺序**

建议分 6 个可单独评审的提交组，按依赖顺序实施，不为赶进度跳过 P0。

| 阶段 | 具体产物 | 完成条件 |
| --- | --- | --- |
| P0：运行环境和传输验证 | 用实际 sctx 子进程实现最小 FIFO 探针；在真实 Hook/MCP/CLI 宿主验证；专用 GitLab 测试分支验证浅克隆、创建分片和再次 push | 无 reader/满管/reader 退出均不改业务结果；实际宿主可投递；公司 GitLab 允许所需 ref 策略。受限入口明确报告，不用同步落盘兜底。 |
| P1：独立日志基础设施 | 新增 `sctx-telemetry`（wire/client/event）、`sctx-log-service`（config/collector/spool/diagnostics）；独立 init/collect/status/doctor/enable/disable | 主配置坏了仍能操作日志；有界缓冲、封口恢复、字段白名单和损坏数据处理通过。 |
| P2：产品接入与旧诊断迁移 | CLI、Hook、MCP 和维护步骤入口接入；doctor --hooks 迁移；旧 SQLite 表保留但停止新增诊断写入 | isError 统计正确；原有启用范围与业务输出不变；Hook 不再为诊断访问 runtime.sqlite；无需修改业务函数签名。 |
| P3：Git 上传与清理 | 独立 runner、journal/receipt、单 ref 浅克隆、分片轮换、sync/prune 命令 | 每个崩溃断点可恢复；远端确认前不删源文件；整体缓存回收及恢复通过；不拉其他设备 blob。 |
| P4：安装与调度 | installer 独立日志生命周期、maintain 子进程步骤、服务 ownership/升级/卸载 | 日志目录或 launchctl 故障不改变 setup/upgrade 结果；同步时业务独占 lease 可获取；卸载保留未上传日志。 |
| P5：查询、端到端验收和小范围验证 | 离线 report/trace；完整故障矩阵；内部若干设备连续运行并检查日志与空间 | 能复现一条实际调用的结果/错误链；缓存清理后空间下降；主流程性能与输出验收通过；实际 GitLab 下身份、时间范围、权限和同步状态可查。 |

`sctx-telemetry` 不能依赖 task-runtime/git-store/installer/MCP，客户端只依赖窄的事件编码和平台设施。`sctx-log-service` 可以依赖 telemetry、确定性 PrivacyScanner 及 Git runner；CLI 同时依赖两者，MCP 只依赖 telemetry。installer 调用稳定命令或日志生命周期适配器，禁止形成反向业务依赖循环。

修改的现有位置：workspace Cargo.toml；CLI main.rs/Cargo.toml 及新增 logs.rs；MCP lib.rs/Cargo.toml；installer lib.rs/maintain.rs/Cargo.toml 及独立 logs_launchd.rs。实现拆成 `sctx-telemetry`、`sctx-log-service`、`sctx-log-sync` 三个 crate，分别负责轻量投递、本地采集和 Git 同步，并补充对应测试。task-runtime 的旧表和 API 保留兼容，未做破坏性 schema migration；知识库 Git 流程保持独立。

**9. 验收矩阵**

| 类别 | 必测场景与断言 |
| --- | --- |
| 业务隔离 | collector 不存在/暂停/崩溃、FIFO 无权限/普通文件替代、队列满、超过帧长、磁盘满/只读、坏日志配置；同输入业务 stdout/stderr、退出码和业务状态一致。 |
| IPC 正确性 | 32 路进程并发无混帧；半帧读取/坏帧重同步；关闭 reader 得到 EPIPE 且 sctx 正常完成；不启动后台采集器、不等待 flush。 |
| 热路径性能 | 复用 hook_hot_path 的 32 路并发 p99 < 500 ms；checkpoint_ack_performance 的 p95 < 250 ms/p99 < 500 ms；candidate_list_performance 的现有 p95 <= 800 ms。对比启用与禁用，新增 emit 的初始 p99 目标 < 1 ms，在同机基线下验证，不把 Python 可行性实验当达标证据。 |
| 采集完整性与隐私 | MCP isError 正确计失败；Disabled 不新增完成记录；typed errors 不落 unknown；不出现原始参数、工具输出、绝对路径或凭证；清理已上传原文后 doctor 仍能显示独立有界诊断。 |
| Git 幂等恢复 | push 未发生/已发生但响应丢失/receipt 写前崩溃/receipt 后删除前崩溃；重试保持 batch ID；远端同路径不同 SHA 拒绝；非快进重试有界且不 force。 |
| 本地空间 | 删除已确认 spool；删除整个 cache 后工作区和 .git 实际回收；重新同步只取当前分片；换月/容量轮换的新分支没有旧根 tree；总预算计入 pack、objects、临时文件和诊断视图。 |
| 生命周期 | 坏 plist/ownership/日志配置/launchctl 超时不回滚业务安装；用户修改服务配置被保留；升级身份稳定；卸载保留未上传批次。 |
| 调度与进程 | logs sync/status 可在业务锁被占、主配置损坏和 DB 不存在时运行；maintain 日志失败不跳过其他步骤；大量输出和派生子进程挂起仍按总 deadline 退出；手动与定时同时运行有一个 skipped_busy。 |

相关现有测试：`crates/cli/tests/hook_hot_path.rs`、`hook_fail_open.rs`、`doctor_hooks_workflow.rs`、`maintain_scheduling.rs`、`repository_scoped_context_acceptance.rs`、`repository_scoped_activation_acceptance.rs`；`crates/mcp/tests/checkpoint_ack_performance.rs`、`candidate_list_performance.rs`；`crates/installer/tests/installer_matrix.rs`。部分旧测试直接断言 SQLite 新诊断行，应改为新观察边界，并保留旧表兼容测试。

开发阶段已增加并运行产品层测试，覆盖真实 CLI/MCP、独立采集进程、Git 恢复、空间回收和服务生命周期故障。部署阶段需要实际的专用日志 GitLab URL 与测试权限，验证宿主回调的 FIFO 权限、GitLab 分支策略、认证和吞吐后再配置生产 remote。首版仅采集结构化元数据，自由文本摘要直接舍弃，不以正则黑名单保证任意原文安全。
