# Provider packages

核心保留 Anthropic Messages、OpenAI Chat Completions / Responses、DeepSeek，以及离线测试用的 Faux。
OpenRouter、llama.cpp 和 Proxy 的供应商配置、模型目录及专用管理功能放到独立 package。
它们复用核心协议实现，保留流式工具调用、用量统计、重试和取消行为。

## 当前拆分

| package | 能力 | 默认认证 |
| --- | --- | --- |
| `rpi-provider-openrouter` | OpenRouter 目录、请求头和 OpenAI-compatible 推理 | `OPENROUTER_API_KEY` |
| `rpi-provider-llama-cpp` | 本地推理、健康检查、模型查询，以及 `rpi-llama` 管理程序 | 可匿名；可选 `LLAMA_CPP_API_KEY` |
| `rpi-provider-proxy` | 自定义 OpenAI-compatible 代理 | 可匿名；可选 `PROXY_API_KEY` |
| `rpi-provider-runtime` | package 共用的同步 ABI / 异步 Provider 桥接库 | 不解析宿主凭据 |

这些 package 是 workspace 的独立成员，CLI 不依赖它们。构建默认 CLI 不会把这些适配器链接进可执行文件。
原 `rpi_ai::providers::{openrouter,llama_cpp,proxy}` 的库调用方需要改用对应的独立 crate。
DeepSeek 和其他已有 `models.json` 网关继续可以复用 OpenAI-compatible 协议，不强制安装 package。

## 安装和配置

在本仓库根目录开发、验证：

```sh
rpi package link openrouter --path packages/provider-openrouter --debug
rpi --list-models openrouter
```

配置 `OPENROUTER_API_KEY` 后：

```sh
rpi --provider openrouter --model openrouter/auto
```

切换到项目中的本地模型：

```sh
rpi package link llama-cpp --path packages/provider-llama-cpp --debug
rpi --provider llama-cpp --model local --base-url http://localhost:8080/v1
```

`local` 和 Proxy 的 `default` 是启动用的目录预设。服务器需要其他模型 id、上下文大小或 token 上限时，在原有 `models.json` 中配置同名 provider 的 `models`。
用户的模型定义、端点和请求头覆盖 package 默认值。安装的 package 通过 `rpi package unlink <name>` 移除；
已有 `models.json` 的通用协议配置仍可工作。

llama.cpp 的服务管理代码位于 package 内，开发时可运行：

```sh
cargo run -p rpi-provider-llama-cpp --bin rpi-llama -- status
```

## 注册与加载

插件使用现有 `declare` 槽位声明模型目录，无需增加 ABI 字段：

```rust,ignore
serde_json::json!({
    "providers": [{
        "id": "openrouter",
        "models": [model], // 完整 rpi_ai::Model
        "apiKeyEnv": "OPENROUTER_API_KEY",
        "requiresAuth": true
    }]
})
```

每个模型的 `provider` 必须等于声明的 `id`，声明必须由同一个插件的实际 provider 注册支持。
同名 provider 按加载顺序保留第一个，其目录也随该实现一起保留。声明可以早于或晚于 provider 注册。
没有目录声明的旧插件仍可配合 `models.json` 工作。

启动顺序是：加载扩展一次 → 收集 package 目录和 runtime factories → 合并用户模型配置 →
按 provider id 解析凭据 → 选择模型并绑定 runtime → 创建 harness。
`--list-models` 可在无凭据时显示 package 模型；`--no-extensions` 禁用这些声明和实现。
TUI 的 `/provider` 同样显示已加载的 package、模型数和认证状态，未配置 key 的 package 也可选为默认。
`/provider <id>` 保存默认 provider 和匹配的模型，重启后生效；`/model` 仍用于当前会话切换。
`/provider remove <id>` 只移除 `models.json` 配置，不卸载 package；卸载使用 `rpi package unlink <name>`。
`/model openrouter/auto` 和完整的 `openrouter/openrouter/auto` 均可定位该模型，跨 provider 重名时需完整限定。
扩展目录仍遵守项目信任、`--project-only` 和显式扩展路径规则。

凭据顺序保持 `--api-key`（仅选中的 provider）→ `auth.json` → `models.json` → provider 自己的环境变量。
匿名运行必须由 package 明确声明，OpenAI 默认认证检查不会因此放宽，其他 package 不会继承 `OPENAI_API_KEY`。

认证策略跟随 package，通用 `auth login/logout/check/refresh` 和 `auth.json` 存储由宿主管理。
`auth check`、`/provider` 和实际请求共用凭据解析：空值、未解析的环境变量引用和过期 OAuth token 不算可用凭据；
有效的 OAuth access token 按 Bearer 注入请求。`auth check` 不刷新 token，也不发起服务端认证验证。

```sh
rpi auth login --provider openrouter
rpi auth check --provider openrouter --json
rpi auth logout --provider openrouter
# 未安装时也可显式加载（扩展路径在 auth 子命令后）
rpi auth check --provider openrouter --extension /path/to/provider.dll --json
```

`auth check --json` 的 `source` 返回 `auth.json`、`oauth`、`models.json`、专属环境变量名、`headers`、`anonymous` 或 `none`，
不输出凭据内容；`--no-extensions` 禁用 package 认证声明。auth 和启动使用相同的扩展发现、项目信任与显式路径规则。
退出码为：就绪 `0`，缺少凭据 `1`，配置读取失败 `2`。API-key logout 只删除存储的 key，不清除环境变量或 models.json。

核心只保留 Anthropic、OpenAI、DeepSeek 的环境变量回退和默认模型优先项。其他供应商不再隐式识别
环境变量：安装带 `apiKeyEnv` 声明的 package，或在 models.json 显式填写 `apiKey: "$VENDOR_API_KEY"`。
已有 auth.json 和显式 apiKey 配置无需迁移，package 默认模型由其目录提供。

`/reload` 重新绑定 fresh plugin runtime、认证后的目录和当前模型，同时更新 `/model` 选择器和插件的 `set_model`。
`/provider` 每次打开时读取重载后的 package 声明。
如果当前模型所属的 package 消失或无法重新解析，reload 返回错误并保留原会话，先切换模型再移除该 package。
取消立即结束宿主请求；插件桥接会取消自己的网络任务，DLL 引用保持到同步请求真正退出。

## 验证

```sh
cargo build -p rpi-provider-openrouter -p rpi-provider-llama-cpp -p rpi-provider-proxy
cargo test -p rpi-cli --test provider_packages
cargo test --workspace
```

端到端测试用真实 DLL、CLI 子进程和本机 SSE 服务，覆盖无凭据目录、禁用扩展、认证隔离、带认证的
OpenRouter、匿名 llama.cpp / Proxy，以及实际流式输出。其他协议可以沿同样的声明和 factory 边界接入。
