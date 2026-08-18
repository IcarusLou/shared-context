# Schemas

此目录保存随源码版本控制的不可变事件 Schema。`event-v1.schema.json` 使用 JSON Schema
Draft 2020-12，`$id` 与 `sctx_event_schema::V1_SCHEMA_ID` 保持一致。

Schema 发布后只读；协议升级必须新增版本文件，不能原地修改已经写入 Git 的版本契约。
`annotations`（包括 `origin_hint`）是唯一允许扩展字段的非权威边界，其他 V1
envelope 和 payload 均拒绝未知字段。
