# Rust 最小 Agent Loop

这是对 `packages/agent/src/agent-loop.ts` **选定职责**的独立 Rust 重实现。它住在 pi
仓库中，但不调用 TypeScript、不需要以 pi 为入口，也不需要 Node.js 才能编译运行。

当前交付是一个**库**和三个**示例程序**：库提供调度能力，示例调用这个库。
`add` 使用预设的 mock 模型；`deepseek` 调用真实 DeepSeek API。
`chat` 提供最基础的终端多轮对话。三者共用真实的参数校验和 `add` 工具。

## 先运行一次

需要 Rust 1.94 或更高版本。标准安装方法见 [Rust 安装说明](https://www.rust-lang.org/tools/install)。
先用 `rustc --version` 和 `cargo --version` 确认环境，再从仓库根目录运行：

```bash
cd package/rust
cargo run --locked --example add
```

如果是在本次开发使用的 Linux 环境，已有工具链但 `cargo` 不在 PATH，可先运行：

```bash
export PATH="$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin:$PATH"
```

示例会显示这条轨迹：

```text
用户：计算 1 + 2
→ 模拟模型收到系统提示、用户消息和 add 声明
→ 模拟模型请求 c1: add({"a":1,"b":2})
→ Core 要求校验，执行层严格检查两个整数参数
→ Core 要求执行，真实 Add 工具返回 3
→ 结果以 c1 关联写入历史
→ 模拟模型收到含工具结果的历史，检查结果确实是 3
→ 模拟模型返回预设文本“答案是 3”
→ 运行结束
```

最后是本次新增 4 条消息，完整历史 5 条：已有系统消息，加上用户、助手调用、工具结果、助手回答。
整个过程没有 HTTP 请求、API key 或付费调用。脚本让行为可重复，便于学习和检查调度规则；
它不验证真实 LLM 的回答质量。

## 使用真实 DeepSeek 模型

在 `package/rust/.env.local` 中持久保存本地配置：

```dotenv
DEEPSEEK_API_KEY=你的实际key
DEEPSEEK_MODEL=deepseek-flash
```

该文件已被 Git 忽略；首次使用可以复制 `.env.local.example` 并填入 key。
示例始终从 Rust 包目录读取 `.env.local`，不依赖启动时的工作目录。
同名进程环境变量优先于文件配置；不再需要每次执行 `export`。

从仓库根目录运行：

```bash
cd package/rust
source "$HOME/.cargo/env"
cargo run --locked --example deepseek
```

默认模型是 `deepseek-flash`，可通过 `DEEPSEEK_MODEL` 修改。
接口固定为 `https://api.deepseek.com/chat/completions`，关闭思考模式，使用非流式完整响应。
第一次请求要求模型调用工具；Core 校验参数后执行本地 `add`，适配器再把工具结果交回模型生成回答。
这条路径没有脚本模型或预设答案，会使用 API 额度。

正常轨迹包含两次 `[DeepSeek]` 请求、`[工具请求] add ...`、`[工具结果] ["3"]`、模型的实际回答和
`[结束] Completed`。示例最多请求模型 4 次，每次输出上限 512 token、超时 60 秒。
HTTP 错误、模型失败或没有成功计算出 3 都会以非零状态退出；401 时检查 API key，402 时检查账户余额。
`.env.local` 是本地明文配置，不要把 key 写入源码或 `.env.local.example`。

协议参照官方 [Chat Completion API](https://api-docs.deepseek.com/api/create-chat-completion) 和
[Tool Calls](https://api-docs.deepseek.com/guides/tool_calls)。HTTP 依赖只用于示例和测试，核心库不增加供应商接口。

## 终端多轮对话

配置好上面的 `.env.local` 后，在 `package/rust/` 下运行：

```bash
cargo run --locked --example chat
```

这是最小的逐行终端界面：在 `user>` 后输入，按 Enter 发送；显示完整助手回答、工具请求和结果，
随后再次等待输入。比如先输入“计算 1 + 2”，再输入“刚才算出的结果是多少？”，第二次请求会携带
第一次的用户消息、工具调用、工具结果和助手回答。普通聊天允许模型直接回答，不强制调用加法工具。

- `/new`：清空当前对话历史，保留系统提示和工具配置。
- `/help`：显示帮助。
- `/exit` 或 `/quit`：退出；输入结束（Linux/macOS 下 Ctrl+D）也会退出。
- Ctrl+C：直接退出进程，包括正在等待模型时。

模型错误会显示在终端，随后可以继续输入。聊天模式不会套用单次演示的累计 4 次请求上限；
每次 HTTP 请求仍有 60 秒超时和 512 token 输出上限。历史保存在当前进程内，重启后不会恢复。
目前一次发送一行，使用终端自身的输入编辑和滚动；尚未实现全屏编辑器、输入历史导航或流式输出。

无需 API key 验证交互：

```bash
cargo run --locked --example chat -- --mock
```

离线模式只回显当前和上一条用户输入，可检查连续输入和 `/new` 是否生效；它不会调用模型或工具。

## 代码从哪里读

建议按下面顺序阅读：

1. `examples/add.rs` / `examples/deepseek.rs`：模拟模型与真实模型如何驱动同一个 Core。
   `examples/chat.rs`：在每次输入后用上次返回的完整历史启动新运行。
   `src/tools/add.rs` 提供加法工具，`src/tools/mod.rs` 是统一注册入口；
   `examples/support/deepseek.rs` 提供共用 HTTP 适配器。
2. `src/types.rs`：消息、工具调用、结果和错误的最小数据结构。
3. `src/core.rs`：显式状态和转换规则，尤其 `advance` 与 `next_tool`。
4. `src/runtime.rs`：执行 `Effect`，把操作结果转换成 `Input` 交回 Core。
5. `tests/agent_loop.rs`：用固定输入检查成功、失败、取消以及有限组合性质。

完整规则、TypeScript 对照和证明边界见 [设计说明](docs/design.md)。本轮不绑定 Verus、TLA+
或 Kani，也没有完成形式化证明。

## 足够读懂本包的 Rust 概念

| Rust 写法 | 在本包中怎么理解 |
| --- | --- |
| `struct` | 一组有类型的字段，例如 `ToolCall`；接近 TS 的数据接口加具体值 |
| `enum` | 几种互斥分支，例如 `Message` 和 `State`；接近 TS discriminated union |
| `match` | 穷举处理 enum 的分支；新增分支时编译器会要求补齐处理 |
| `trait` / `impl` | `Model`、`AgentTool` 定义能力契约；`impl` 提供具体实现 |
| `Result<T, E>` | 显式的 `Ok(value)` 或 `Err(error)`；`?` 遇到 Err 就向调用者返回 |
| `Option<T>` | 显式的 `Some(value)` 或 `None`；例如未执行工具就没有耗时 |
| 所有权 / move | 数据由谁负责保存和释放；把 context 传入库后，通过 outcome 取回 |
| `&T` / `&mut T` | 临时只读借用 / 独占可修改借用，不是把对象复制一份 |
| `clone()` | 显式复制；本原型用消息快照换取容易理解的接口，尚未优化大历史性能 |
| `async` / `Future` / `.await` | 描述稍后完成的操作，由 executor 驱动；等待不等于自动开线程 |
| `Box<dyn AgentTool>` | 统一保存不同的工具实现；Box 持有堆上的对象，dyn 通过 trait 调用 |
| `BoxFuture<'a, T>` | 统一不同 async 实现的返回类型；`'a` 限制借用不能比原对象活得更久 |

Tokio 在这里用于执行层的调度让步以及示例、测试的 async executor；`CancellationToken` 用于合作取消。
生产库的执行层不启动后台任务，也不使用 `unsafe`。

## 如何调用或扩展

- 新输入用 `run_agent_loop`；接着处理已有用户消息或工具结果用 `run_agent_loop_continue`。
- `RunOutcome.context` 是完整历史，`new_messages` 只含本次新增消息；不要把后者再重复追加到前者。
- 模型和工具是调用方提供的实现：接真实模型时实现 `Model::complete`，返回完整的助手消息。
- 新工具实现 `AgentTool`，通过 `ToolRegistry::register` 注册。声明 Schema 和实际 validator 必须一致；库不会自动按 Schema 校验。
- 工具 validator 应严格检查、不产生外部副作用；`execute` 返回 `Err` 或 `is_error: true` 表示失败。
- 示例参数采用 `serde` 严格反序列化：缺字段、未知字段、字符串冒充整数等都会失败。
- 模型和工具必须响应取消并最终返回；如果适配器永不返回，库不能保证取消及时完成。
- 使用 `CancellationToken::clone()` 在外部持有同一取消信号，通过 `cancel()` 请求停止。
- `AgentEvent` 用于观察完整消息与生命周期，`Input` 才是送给 Core 的操作结果；二者用途不同。
- `OperationError` 会进入消息历史；`LoopError` 表示入口或协议不合法，通常需要修正调用代码。

核心库只支持文本、顺序工具、固定工具表、完整模型响应以及取消。图片、thinking、token 用量、
流式片段、工具进度、并行、队列、hooks、持久化和跨语言通信均不在本版范围。

工具统一走这条链路：`register` → 声明快照交给模型 → 按声明名称查找 → 校验参数 → 执行 → 记录结果。
`add` 是验证工具调用链路的普通工具，不是从原版 PI 移植的工具，也没有专用的执行分支。
默认注册表集中在 `src/tools/mod.rs`；以后加入默认工具，只需实现 `AgentTool` 并在这里注册。
聊天函数接收外部注册表，系统提示不绑定具体工具。

```rust
use pi_agent_loop::{ToolRegistry, tools::Add};

let mut tools = ToolRegistry::default();
tools.register(Add)?;
// 将 &tools 传给 run_agent_loop 或 run_agent_loop_continue。
```

## 测试和检查

在 `package/rust/` 下：

```bash
cargo test --locked
cargo test --locked --example deepseek
cargo test --locked --example chat
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo doc --locked --no-deps
```

标准 rustup 环境可以通过 `rustup component add rustfmt clippy` 安装额外检查工具。

自动测试不请求真实模型，不读取 API key；DeepSeek 示例的测试通过本地 HTTP 服务验证请求格式、真实工具执行、
结果回传、认证错误及取消。聊天测试覆盖多轮历史、工具结果保留、会话重置、命令、EOF 和失败后继续输入。
手动运行 `deepseek` 或未带 `--mock` 的 `chat` 示例才会请求官方接口。
其中有限组合测试枚举 425 条轨迹；这个数字来自五种停止原因乘以 0～3 个调用的四种输入组合。
这能提供回归证据，不能证明所有规模、所有外部实现都正确。

修改后还需要从仓库根目录运行 `npm run check`，这是仓库的整体代码检查。
本 Rust 包不加入 npm workspace，不需要修改 npm 依赖或现有 TypeScript 实现。
