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

## Factory 与执行绑定

内置协议和原生扩展实现同一个宿主侧 `ProviderFactory` 契约。注册表优先查找 provider ID 对应的扩展 factory，
再查找 API 协议对应的内置 factory；两者均由同一个 `ConfiguredProvider` 包装。
factory 负责提供协议实现，不读取环境变量、不决定凭据优先级；认证统一由宿主解析并绑定。

解析阶段将每个模型分成两种视图：

| 视图 | 内容与用途 |
| --- | --- |
| 公开描述 | 仍使用现有 `Model` 类型以兼容 CLI / harness / 插件 ABI，但不携带任何请求头。用于目录、当前模型和模型切换 |
| 私有 `ExecutionBinding` | 保存该实例的真实端点、模型级请求头与默认认证，只在执行请求时恢复 |

`Provider::models()`、`catalog_all()`、`catalog_with_manifests()`、认证后的目录以及 `ResolvedModel.model` 均返回公开描述。
`base_url` 仍在描述中可见以兼容现有配置和展示；实际执行以私有绑定为准。
调用者传入的模型必须匹配已注册的 provider / API / model ID，不能通过改写描述中的端点或请求头重定向已绑定凭据。
每次请求的 `SimpleStreamOptions` 仍可以按既有规则提供显式 API key 或额外请求头。

涉及 provider package 的 `/reload` 会重新解析配置并构建新的执行绑定，随后替换 provider 实例和公开目录；
凭据不再通过当前模型的 headers 传递。仅内置 provider 的会话保留既有的资源重载行为。
当前 package 被移除时，重载会在切换前失败并保留旧会话，避免把同名配置静默转交给另一个协议实现。
完整重载仍包含多个对象的更新，单一代次快照的原子切换属于后续生命周期改进。

核心 `DeepSeekProvider::new` 保留现有入口，推理已委托通用 Chat Completions 流式实现，
统一文本 / 思考事件、错误、用量和取消路径，并使用传入模型配置的端点。

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

`/reload` 重新绑定 fresh plugin runtime、私有执行配置和公开模型目录，同时更新 `/model` 选择器和插件的 `set_model`。
`/provider` 每次打开时读取重载后的 package 声明。
如果当前模型所属的 package 消失或无法重新解析，reload 返回错误并保留原会话，先切换模型再移除该 package。
取消立即结束宿主请求；插件桥接会取消自己的网络任务，DLL 引用保持到同步请求真正退出。

## 验证

可运行的 DeepSeek 流式扩展示例见 [examples/provider-deepseek](../../examples/provider-deepseek/README.md)。
它使用独立的 `deepseek-extension` ID，提供真实动态库加载、本地分段 SSE、取消和 HTTP 错误测试，
以及需要显式启用的真实 DeepSeek API smoke test。

```sh
cargo build -p rpi-provider-openrouter -p rpi-provider-llama-cpp -p rpi-provider-proxy
cargo test -p rpi-cli --test provider_packages
cargo test --workspace
```

端到端测试用真实 DLL、CLI 子进程和本机 SSE 服务，覆盖无凭据目录、禁用扩展、认证隔离、带认证的
OpenRouter、匿名 llama.cpp / Proxy，以及实际流式输出。其他协议可以沿同样的声明和 factory 边界接入。
