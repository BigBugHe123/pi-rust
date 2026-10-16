# DeepSeek 流式 provider 扩展示例

接入约定与验收要求见 [Provider 接入规范](../../docs/extensions/provider-integration.md)。

该示例使用独立的 `deepseek-extension` provider ID，通过真实 DLL / so / dylib 注册流式回调，
复用 `rpi-provider-runtime` 的 ABI 桥接和 OpenAI-compatible SSE 解析。
它是 ABI 教学样例，不会成为默认 CLI 的依赖。正式 `deepseek` provider 已迁出核心，位于
[独立 rpi-package 项目](https://github.com/pi-rust/rpi-package/tree/feature-1014/packages/rpi-provider-deepseek)。

默认模型使用 `deepseek-flash`，认证声明为 `DEEPSEEK_API_KEY`。
模型名与 `reasoning_content` 字段参照 [DeepSeek 首次调用](https://api-docs.deepseek.com/)
和 [思考模式文档](https://api-docs.deepseek.com/guides/thinking_mode/)。

## 1. 自动验证扩展流式输出（无需 key）

在仓库根目录执行：

```sh
cargo build -p rpi-example-provider-deepseek
cargo test -p rpi-example-provider-deepseek --test streaming -- --nocapture
```

三个测试加载编译出来的动态库，向本机的模拟 DeepSeek SSE 服务发起真实 HTTP 请求：

- **真正增量输出**：服务端先发送 `reasoning_content` 和第一段正文，等测试收到正文增量后才发送剩余正文和结束帧。
  如果扩展等待完整响应再输出，测试会超时失败。同时检查中文 UTF-8 跨 HTTP chunk、usage 和唯一的 Done 事件。
- **流中取消**：正文到达后取消，服务端仍保持响应未结束，检查 Aborted、唯一 Error 事件和已输出正文。
- **认证失败**：模拟 HTTP 401，检查错误传递，不允许伪造 Done。

输出应为 `3 passed; 0 failed; 1 ignored`。被忽略的是下面的真实 API 测试。
测试找不到动态库时会明确失败并提示先 build，不会静默跳过。

## 2. 真实 DeepSeek API smoke test

在 PowerShell 中先将自己的 key 放入当前进程的环境变量，再显式运行该测试：

```powershell
$env:DEEPSEEK_API_KEY = '<你的 DeepSeek key>'
cargo test -p rpi-example-provider-deepseek --test streaming live_deepseek_extension_stream -- --ignored --exact --nocapture
```

该测试使用真实动态库和真实 API，请求十句短文，要求收到至少两个正文增量；每次 TextDelta 到达时就打印，最后打印 provider ID 和 TextDelta 事件数。
它最多请求 1024 个输出 token，不重试；正常的自动测试不会运行它。

## 3. 在 rpi 中交互验证

使用本分支构建的 CLI，在仓库根目录执行：

```sh
cargo run -p rpi-cli -- package link deepseek-extension --path examples/provider-deepseek --debug
cargo run -p rpi-cli -- --list-models deepseek-extension
cargo run -p rpi-cli -- auth check --provider deepseek-extension --json
cargo run -p rpi-cli -- --provider deepseek-extension --model deepseek-flash --no-tools
```

认证也可以通过 `rpi auth login --provider deepseek-extension` 保存到 auth.json。
在 TUI 中发送一个简短问题，观察文字逐步出现；生成中按 Esc，检查能取消且保留已经输出的文字。
测试后可用 `rpi package unlink deepseek-extension` 卸载。

这是文本 / 思考流式验证示例，交互命令明确使用 `--no-tools`。
本示例不声明完整工具循环；生产 package 已适配 `reasoning_content` 多轮回传并提供对应测试，
详见独立 package 的 README 和官方思考模式文档。
