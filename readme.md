# 团队共享 Context：预期效果

> 第一次接触项目？请先阅读[《Shared Context 新手使用指南》](./docs/user-guide.md)，其中包含安装步骤、基础原理、日常用法和完整功能清单。

日志采集、Git 上报和本地空间清理见[《日志采集、同步与排障》](./docs/logging.md)。

## 一句话定义

> **让一个人在某个 Agent 里已经完成的理解、判断和验证，能够低成本地变成团队下一位成员和下一个 Agent 可以直接继承的有效工程 Context，而不是重新读 PRD、重新搜代码、重新问一遍。**

核心不是“共享聊天记录”，而是建立：

```text
个人工作过程
    ↓
可复用团队 Context
    ↓
在正确时机重新注入工作流
```

当前实现不存在 Hook 记录列表或人工筛选步骤。工作 Agent 直接用 `task_checkpoint` 提交聚焦的 Claims、Unknowns 和自包含 Evidence 摘要；服务端负责解析当前 Task/Intent、关闭 Work Episode、生成稳定重试身份并持久化 Candidate Build 队列。Checkpoint ACK 与同内容重放不写 Git，`candidate_list/get` 可恢复不可信 Candidate 提案，只有用户显式 `candidate_confirm` 才生成 accepted Context 事实。

---

## 第一性原理：让有效认知随工作自然传承

协作中最大的浪费，不是信息从未出现，而是已经完成的理解、判断和验证，随着人员、时间、工具或工作环节的切换而失效，后来者不得不重新支付同样的认知成本。

理想状态下，人只需要专注于当前想完成的事情。系统理解他的意图和处境，主动找到过去相关的认知，解释当时为什么这样判断，并帮助他在已有理解之上继续决策；本次工作产生的新认知，也应在过程中自然沉淀，供后来者继续继承。

```text
理解当前意图
    ↓
恢复相关历史认知及其因果
    ↓
辅助当下判断与协作
    ↓
沉淀本次新增认知
    ↓
成为未来工作的起点
```

这意味着：

- 人不需要学习或维护一套额外的知识组织方式。
- 有价值的认知能够跨越人员、时间、工具和职责边界持续流动。
- 系统主动判断什么与当前工作相关，而不是把信息堆给使用者筛选。
- 人主要负责确认正确性、适用边界和冲突，而不是重复整理已经完成的思考。

> **最终要消除的不是信息查找本身，而是团队对同一问题反复理解、反复判断、反复验证的认知浪费。**

---

## 1. 降低 Context Cold Start

当前一个需求切换到另一个端、另一个开发者或另一个 Agent 时，经常需要重新经历：

```text
PRD
 ↓
找代码
 ↓
理解现状
 ↓
了解历史决策
 ↓
寻找相关改动
 ↓
判断已有结论是否有效
 ↓
开始真正工作
```

共享 Context 希望显著压缩这部分时间。

例如 iOS 已经验证：

> 某字段不是客户端计算，而是服务端下发；为了兼容旧版本，目前不能修改 schema。

Android Agent 接手同一需求时，不应该重新探索一遍，而应该能够直接获取这条已经验证过的结论及其证据。

因此，第一个核心效果是：

> **减少 Context Cold Start，尤其是跨 FE / iOS / Android、跨 Agent、跨 Session 的冷启动成本。**

---



## 2. 从“共享信息”升级为“共享工程认知”

真正需要共享的不是：

> 某次 Agent 曾经说过什么。

而是经过提炼后的工程认知，例如：

```yaml
type: decision
decision: 采用方案 A，不采用方案 B
reason: 方案 B 会破坏旧版本兼容
scope:
  - Search Result
  - iOS
  - Android
evidence:
  - code
  - PR
  - experiment
status: accepted
supersedes: decision-123
```

因此需要完成从：

```text
Conversation Sharing
```

向：

```text
Engineering Context Sharing
```

的转变。

原始 Agent Conversation / Trace 不作为产品输入。工作 Agent 直接把已经形成的结论、理由、适用条件、证据摘要与局限写成 Checkpoint；这些内容在一次明确处置（人工确认，或服务端校验通过的 Agent 自动确认，ADR-0005）之前仍是不可信 Candidate。

---



## 3. Context 围绕 Requirement 持续演化

目标不是建立几个互相独立的信息系统：

```text
PRD
IDE
Agent
Git
Issue
Wiki
```

而是围绕一个 Requirement 形成统一的 Context Space：

```text
                    Requirement
                         │
          ┌──────────────┼──────────────┐
          ↓              ↓              ↓
         FE             iOS          Android
          │              │              │
        Agent          Agent          Agent
          │              │              │
          └─────── Context Layer ───────┘
                         │
                Decision / Contract
                Issue / Risk
                Validation / Progress
                         │
                         ↓
                     后续工作
```

因此可以把 Requirement 理解为：

> **一个持续演化的 Context 容器，而不仅仅是一张需求卡片。**

需求生命周期中产生的内容都可以挂载到它下面，例如：

- Requirement clarification
- Contract
- Design decision
- Implementation discovery
- Issue
- Risk
- Validation result
- Progress
- Cross-platform constraint

---



## 4. 在正确时机注入正确的 Context

共享 Context 最大的价值不是“记住更多”，而是：

> **Right Context at the Right Time**

例如 Android Agent 当前正在修改：

```text
Requirement: Hide General Tab
Platform: Android
Module: Search Result
File: SearchResultFragment.kt
```

系统应该根据当前工作状态获取相关内容：

```text
Relevant Decisions
Relevant Contracts
Known Risks
Previous Validation
Cross-platform Constraints
```

而不是直接塞给 Agent：

> 这里有这个需求过去的 300 条聊天记录，你自己看。

因此整个系统需要同时解决两个问题：

```text
Checkpoint
  ↓
Agent 认为哪些结论值得沉淀？

Retrieve
  ↓
当前应该拿哪些 Context？
```

---



## 5. 支持多人、多 Agent 并行协作

未来的工作模式可能从：

```text
一个工程师
   ↓
一个 Context
   ↓
串行完成任务
```

逐渐转变为：

```text
                    Requirement
                         │
             ┌───────────┼───────────┐
             ↓           ↓           ↓
          iOS Agent  Android Agent  FE Agent
             │           │           │
             └──── Shared Context ───┘
```

共享 Context 本质上成为一种异步同步机制。

例如 Android Agent 发现：

> API Contract 与 PRD 描述不一致。

这个发现应该快速成为：

```text
Issue / Decision Candidate
```

随后 iOS、FE 或其他 Agent 能看到，而不是继续按照旧 PRD 实现。

因此共享 Context 可以提高：

- 多端并行能力
- 多 Agent 并行能力
- 异步协作效率
- 跨端一致性

---



## 6. 将一次性的认知成本转化为团队资产

当前 Agent Coding 中存在大量重复认知成本：

```text
Agent 搜索几十个文件
        ↓
理解工程架构
        ↓
发现关键约束
        ↓
完成当前任务
        ↓
Session 结束
        ↓
大部分认知丢失
```

下一次另外一个 Agent 又重新执行：

```text
搜索
 ↓
阅读
 ↓
理解
 ↓
验证
```

共享 Context 希望将其变成：

```text
探索
 ↓
产生 Candidate Context
 ↓
验证 / Review
 ↓
沉淀 Team Context
 ↓
未来持续复用
```

也就是说：

> **工程探索成本只支付一次，但产生的工程认知可以被团队持续复用。**

长期来看形成一种：

> **Context Compound Interest**

项目做得越久，Agent 对工程的冷启动成本应该越低，而不是始终从零开始。

---



## 7. 尽可能降低 Context Tax

共享 Context 不能依赖这样的工作方式：

```text
完成开发
  ↓
再花 30 分钟整理知识
  ↓
再手动填写 Wiki
  ↓
再维护 Context 数据库
```

否则系统本身会产生巨大的：

> **Context Tax**

理想流程应该更接近：

```text
正常使用 IDE / Agent
        ↓
Agent 直接提交聚焦的 Claim / Unknown / Evidence 摘要
        ↓
服务端可靠排队生成 Candidate
        ↓
人工 Review / Validate
        ↓
形成 Team Context
```

人工主要负责：

- 判断结论是否正确
- 判断是否值得长期保存
- 处理冲突
- 确认作用范围

而不是重新把 Agent 已经探索过的内容人工写一遍。

因此一个重要设计目标是：

> **共享 Context 对工程师额外增加的操作成本应尽可能接近零。**

---



## 8. Context 必须具有生命周期和治理能力

不能简单实现：

```text
Agent Output
    ↓
Database
```

否则随着时间推移，很容易形成大量：

- 过期知识
- 错误推断
- 重复结论
- 互相冲突的 Decision
- 无法判断来源的 AI 生成内容

因此 Context 至少应该存在类似生命周期：

```text
Candidate
    ↓
Review / Validate
    ↓
Accepted
    ↓
Deprecated / Superseded
```

同时记录必要元数据：

```yaml
source:
evidence:
scope:
version:
timestamp:
owner:
status:
confidence:
acl:
```

这意味着该系统不仅是：

> Context Storage

同时也是：

> **Context Governance**

---



# 核心预期效果

最终可以归纳为五个核心目标。

## 1. 少重复理解

同一个工程事实不应该被：

- FE
- iOS
- Android
- 不同开发者
- 不同 Agent
- 不同 Session

反复探索。

---



## 2. 少信息丢失

Agent Session 结束后，有价值的：

- Decision
- Contract
- Issue
- Risk
- Validation
- Engineering Discovery

不应该一起消失。

---



## 3. 降低 Context 切换成本

在以下场景中都应该快速恢复团队当前认知：

```text
换人
换端
换 Agent
换 IDE
换 Session
隔几天重新继续
```

---



## 4. 提升并行能力

多个开发者和 Agent 可以围绕一个 Requirement 同时工作：

```text
                    Requirement
                         │
          ┌──────────────┼──────────────┐
          ↓              ↓              ↓
        Agent A        Agent B        Agent C
          │              │              │
          └────── Shared Context ───────┘
```

新的发现、风险和决策能够快速传播到其他工作流。

---



## 5. Context 越使用越有价值

每完成一个需求，团队得到的不应该只有：

```text
Code + PR
```

而应该同时增加：

```text
Code
+
Decision
+
Contract
+
Validation
+
Engineering Knowledge
```

从而让未来的 Agent 更快理解工程。

---



# North Star

不应该把下面这些指标作为最终目标：

```text
沉淀了多少 Context
记录了多少 Conversation
数据库里有多少知识条目
```

更合适的 North Star 是：

> **一个没有参与过该需求的人或 Agent，需要多久才能获得足够的 Context，并做出接近原参与者质量的工程判断？**

最终希望达到的状态是：

> **团队成员可以换，Agent 可以换，IDE 可以换，会话可以结束，但 Requirement 的工程认知不会丢。**

可以进一步抽象为：

```text
Conclude Once
    ↓
Validate Once
    ↓
Share Across Team
    ↓
Retrieve on Demand
    ↓
Reuse Repeatedly
```

这就是团队共享 Context 最核心的预期效果。
