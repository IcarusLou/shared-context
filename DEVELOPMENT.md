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

## NPM 打包检查

NPM 层没有第三方 JavaScript 运行时依赖，使用 Node.js 18 或更新版本：

```bash
cd npm
npm test
```

macOS 测试会为 `aarch64-apple-darwin` 和 `x86_64-apple-darwin` 构建真实 CLI，因此两种
Rust target 都必须安装。arm64 主机执行当前架构真实二进制的 launcher、offline install
和显式 setup smoke；x64 在没有 Intel 主机时只验证 Mach-O、签名、包内容、`os/cpu`、
离线 lock 和 checksum 契约，不将交叉产物或 Rosetta 当作原生执行证据。

本地 artifact builder 的输入必须是显式路径指定的、已签名、单架构 Mach-O。命令和离线
安装方法见 [`npm/README.md`](./npm/README.md)。构建脚本只写本地输出，不执行 publish
或 upload。

需要在当前 Mac 上测试源码到本地 NPM 安装的完整链路时，运行：

```bash
cd npm
npm run install:local
```

脚本默认执行 release 编译、临时 ad-hoc 签名、当前架构双 tgz 打包，并以 offline 模式安装
到 `target/npm-local`。它只验证安装后的 `sctx`，不会执行 Setup 或修改 Agent 配置。

## 设计不变量

任何后续实现都必须保持 [`technical-design.md`](./technical-design.md) 第 20 节的核心不变量。骨架中的 crate 边界不是对领域模型的替代定义。
