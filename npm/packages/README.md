# NPM package sources

这里保存主 launcher 与两个 macOS 平台包的可追踪源码。平台二进制不进入版本控制，只能
由 [`../scripts/build-platform-package.js`](../scripts/build-platform-package.js) 或离线 Bundle
builder 从显式传入的已签名薄 Mach-O 生成。构建、离线安装和 `setup` 用法见
[`../README.md`](../README.md)。
