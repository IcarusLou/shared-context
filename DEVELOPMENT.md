# 开发约定

本仓库当前只提供 Shared Context 的 Rust workspace、构建边界和测试骨架。领域事件、Git 写入和 SQLite 投影不属于本阶段。

## 环境与检查

需要 Rust stable（最低 Rust 1.85），并安装 `rustfmt` 与 `clippy`：

```bash
cargo fmt --all -- --check
cargo metadata --no-deps --format-version 1
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
```

提交 `Cargo.lock`，以便 CLI workspace 的本地和 CI 构建使用相同依赖解析结果。

## Crate 依赖方向

依赖只能从表格下方向上方流动；`cli` 是组合入口，`domain` 不依赖其他 workspace crate。

| 层级 | Crate | 允许的 workspace 依赖 |
|---:|---|---|
| 0 | `domain` | 无 |
| 1 | `event-schema` | `domain` |
| 2 | `git-store` | `domain`, `event-schema` |
| 3 | `index` | `domain`, `event-schema`, `git-store` |
| 4 | `search` | `domain`, `index` |
| 5 | `mcp` | `domain`, `search` |
| 5 | `adapter-cursor` | `domain`, `search` |
| 5 | `adapter-codex` | `domain`, `search` |
| 6 | `installer` | `domain`, `adapter-cursor`, `adapter-codex` |
| 7 | `cli` | 所有库 crate |

跨 crate 的可恢复错误统一使用 `sctx_domain::Error` 和 `sctx_domain::Result`。新增依赖时必须保持以上有向无环结构，并同步更新 workspace 契约测试。

## Fixtures 与 schema

- `fixtures/` 保存跨 crate 的只读测试输入，不在测试中原地改写。
- `schemas/` 保存需要随源码版本控制的 schema。
- `npm/` 保存后续 NPM launcher 与平台包源码。

这些目录不得被 `.gitignore` 整体排除。构建产物必须使用精确到产物目录的忽略规则。

## 设计不变量

任何后续实现都必须保持 [`technical-design.md`](./technical-design.md) 第 20 节的核心不变量。骨架中的 crate 边界不是对领域模型的替代定义。
