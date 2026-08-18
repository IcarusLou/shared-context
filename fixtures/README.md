# Fixtures

此目录保留跨 crate 的契约测试输入。后续事件、Cursor Hook 和 Codex Hook fixture 应按来源建立子目录，并作为只读输入提交；测试不得修改 fixture 原文件。

当前阶段不添加领域事件或真实 Agent payload，以免在对应 schema/adapter 设计完成前固化错误格式。
