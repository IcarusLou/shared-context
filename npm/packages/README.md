# NPM package sources

这里保存 `@bytedance-dev/shared-context` 主 launcher 与两个 macOS 平台包的可追踪源码。
三个包均通过 `publishConfig` 固定发布到 `https://bnpm.byted.org`。平台二进制不进入版本
控制，只能由 [`../scripts/build-platform-package.js`](../scripts/build-platform-package.js) 或
离线 Bundle builder 从显式传入的已签名薄 Mach-O 生成。构建、离线安装和 `setup` 用法见
[`../README.md`](../README.md)。
