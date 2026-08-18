# Fixtures

此目录保留跨 crate 的只读契约测试输入；测试不得修改原文件。

- `events/v1/valid/`：V1 七类事件各一个合法、可 round-trip 的样例。
- `events/v1/invalid/`：缺失必填字段、未知权威字段、非法 ID 与 payload 局部不变量。
- `events/unknown/`：Reader 必须原样保留并隔离的未知 Schema 输入。

后续协议演进应新增版本目录，不得改写已经发布的 fixture 合同。
