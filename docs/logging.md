# 日志采集、同步与排障

日志由独立采集进程记录，通过独立 Git 仓库上传。整个过程不调用模型。采集器不可用、队列满或磁盘不可写时允许丢日志，CLI、Hook 和 MCP 不等待日志落盘或上传。

## 开启与日常使用

使用包含日志功能的 `sctx` 安装或升级后，配置专用日志仓库：

```bash
sctx logs init --remote <GIT_URL> --email alice@example.com
sctx logs status --json
```

`--email` 可省略，此时初始化从全局 Git 配置读取完整 `user.email`。身份只在初始化时读取，不在业务热路径调用 Git。也可以先执行 `sctx logs init`，仅在本机采集，之后再显式配置邮箱和远端。

macOS 初始化会安装独立的用户 LaunchAgent；服务安装失败会返回说明，已工作的业务安装不受影响。前台排查时可以手动执行 `sctx logs collect`，同一个日志根仅允许一个采集器运行。

```bash
# 主动封口并上传当前日志
sctx logs sync --json

# 查看采集心跳、积压、空间压力和同步状态
sctx logs status --json
sctx logs doctor --probe --json

# 回收可重建的本地 Git 缓存
sctx logs prune --cache --json

# 暂停或恢复采集及自动同步，保留现有日志
sctx logs disable
sctx logs enable
```

独立的 `logs sync` 会返回真实失败退出码，适合加入外部定时任务。已有 `sctx maintain run` 也会在配置启用时执行日志同步；其日志步骤失败只进入维护摘要，不回滚其他步骤。

默认维护频率仍为每天一次，SessionStart 可按已有陈旧阈值补跑。若需要更及时上报，可在现有调度器中增加 `sctx logs sync --json`。手动同步与定时同步通过日志专用锁避免重复执行。

## 同步结果的含义

同步先请求采集器封口，最多等待 2 秒，然后处理已封口批次。默认一次最多上传约 20 MiB 新日志，总时限 60 秒。

| 结果 | 含义 |
| --- | --- |
| `uploaded` | 本次选中的批次已确认在远端；结合 `remaining_batches` 查看积压。 |
| `partial` | 达到单次处理预算，仍有批次等待下次同步。 |
| `no_ready` | 没有可上传的已封口批次。 |
| `skipped_busy` | 另一个同步正在运行，本次立即退出。 |

`active_included: true` 表示采集器确认封口；为 `false` 时，查看 `seal_status`，本次不承诺包含仍在写入的日志。采集器不存在或暂停时，已封口批次仍可上传。

断网、凭证失效或推送结果未知时保留待上传文件，下次重试使用同一批次身份。不会因本地已经 commit 就删除源文件，也不会 force push。

## 存储位置与空间策略

默认日志根为 `~/.shared-context-logs`，与业务安装和知识库分离：

```text
config.toml             独立日志配置
state/                  诊断视图、上传状态、回执和锁
spool/active/           正在写入的批次
spool/ready/            已封口、尚未确认上传的批次
repository/             当前远端分片的可重建 Git 缓存
```

采集通道位于固定系统临时目录中的私有 FIFO，按有效用户 ID 和日志根摘要隔离，不依赖不同进程的 `TMPDIR` 设置。业务进程无需写入日志根。`doctor --probe` 提供端点检查，`status` 提供心跳和观测状态；真实宿主的 FIFO 写入权限仍需在部署时验证。

每条命令支持绝对路径 `--logs-root`，业务进程可通过绝对路径环境变量 `SCTX_LOGS_ROOT` 使用同一个日志根。不要让采集器与业务进程指向不同的日志根。

远端使用一个专用 Git 仓库，分支结构为：

```text
logs/<installation-id>/<stream-id>/<UTC年月>/<分片序号>
```

分支内按完整邮箱组织文件：

```text
users/alice@example.com/<installation-id>/<UTC日期>/<batch-id>.jsonl
users/alice@example.com/<installation-id>/<UTC日期>/<batch-id>.manifest.json
```

月份变化、分片达到约 128 MiB 或文件数上限时开启独立新分支。查看历史需要选择对应分支；默认分支不聚合全部用户日志。邮箱目录用于组织，访问权限由整个 GitLab 仓库的权限控制。

空间回收分两层：

- 确认远端内容并持久化回执后，立即删除对应的本地待上传批次。
- 分片轮换或缓存达到默认 7 天创建年龄时，回收整个受管 Git 缓存，包括 `.git`。也可以主动运行 `logs prune --cache`。

清理缓存不会向远端提交删除操作。下次同步只拉取当前设备的当前分片，不下载团队全部日志。分片 128 MiB 是内容预算，实际缓存还包含工作区、Git 对象和临时文件。

默认待上传空间预算为 256 MiB，日志根总预算为 1 GiB，并保留磁盘余量。若待上传日志已满，暂停接收新日志，保留未上传批次；上传或清理释放空间后恢复。正在恢复的上传 journal 会阻止手动清理缓存，先执行 `logs sync` 核实状态。

变更邮箱或远端会开启新 stream，已经归属的批次仍使用原身份和原目标。卸载业务程序会停用受管日志服务，但保留日志数据。

## 查看行为与错误

采集内容包括操作类型、耗时、成功/失败/降级、稳定错误码，以及可获取的会话摘要和业务关联 ID。默认不记录完整对话、Prompt、代码、工具输入输出、凭证或绝对路径。

Hook 本地排障入口保持为：

```bash
sctx doctor --hooks --json
```

新诊断视图独立于业务 SQLite，上传后删除日志原文也不会立即失去近期 Hook 诊断。计数按小时聚合，近 24 小时窗口的边界可能多包含不足一小时的数据。未开启采集、采集停机或存在观测缺口时，不能将“没有错误记录”当作“没有发生错误”。

对已导出的指定范围 JSONL 进行确定性分析：

```bash
sctx logs report --input <JSONL目录> --json
sctx logs trace --input <JSONL目录> --invocation <调用ID> --json
```

这些命令不会隐式下载整个远端仓库，也不调用模型。报表按已完成操作统计明确的 `failure` 比例；开始事件不进入完成次数的分母。Hook 的 `fail_open`、`degraded` 等降级结果可通过诊断计数或 trace 查看，不计作业务失败。一次调用缺少结束记录只表示未观察到结束，不直接判定为崩溃。
