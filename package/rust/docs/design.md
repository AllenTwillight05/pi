# 设计、行为边界与后续 proof

## 为什么分 Core 和执行层

LLM 选择“调用什么工具”；runtime 仍需遵守“什么情况下允许调用、怎样记录结果、何时继续”的程序规则。
两者不是同一种决策。本设计把 **runtime 内部的调度规则**集中到 Core，没有增加替代 LLM 的推理层。

原版 `runLoop` 在 async 循环内直接等待模型、处理工具和修改历史。Rust 原型用
Functional Core / Imperative Shell 组织相同范围内的职责：

```text
                          整个 agent runtime
┌──────────────────────────────────────────────────────────────────┐
│ Core：State + Input → 新 State + 有序 AgentEvent + 至多一个 Effect │
│                              ↓                                   │
│ 执行层：报告事件 → 调模型/校验器/工具 → 将结果作为 Input 交回 Core │
└──────────────────────────────────────────────────────────────────┘
```

`Effect` 是数据，不会因为被构造就执行工具。`Core::advance` 不读取时间、不处理网络、
不读取取消 token；给定相同状态和输入，得到相同输出。时钟测量、外部失败和取消信号快照来自执行层。
Core 的字段私有，调用方不能直接把 State 改为“已经校验”。

这种拆分不是 Rust 所必需的，也不自动获得 proof。它让验证对象和环境边界更明确。
消息目前采用 clone 快照，重在可读性，不是高吞吐、大上下文场景的性能实现。

## 状态转换

完整 Core 状态除了 `State` 标签，还包括历史、固定工具声明、当前助手消息、
按出现顺序提取的调用列表、调用位置及当前轮结果。

| 当前 State | Input 或条件 | 结果 |
| --- | --- | --- |
| Ready | Start | 报告运行开始、轮次开始，记录新提示，请求模型 |
| Ready | Start 且已取消 | 记录取消助手消息，结算本轮和运行，不请求模型 |
| AwaitModel | 正常响应且无工具 | 记录助手消息，结算 Completed |
| AwaitModel | error / aborted | 记录助手消息，结算 Failed / Aborted，不处理工具 |
| AwaitModel | 响应含工具调用 | 依次处理调用；可执行的调用进入 AwaitValidation |
| AwaitValidation | 校验成功且未取消 | 进入 AwaitTool，产生唯一允许的 ExecuteTool 分支 |
| AwaitValidation | 校验失败 | 记录错误结果，处理下一个调用 |
| AwaitTool | 工具返回 | 记录成功或错误结果，处理下一个调用 |
| 工具批次处理完 | 所有结果已入历史 | 结算当前轮，开始下一轮并请求模型 |
| Done | 任意后续输入 | 不改历史，不再报告事件或产生 Effect |

非终态收到阶段不匹配的 Input 返回 `UnexpectedInput`，不修改 Core。
调用方只能在 Done 时通过 `into_outcome` 消费 Core 并取出结果。

### 调用预检和错误

每次开始处理调用先报告 `ToolExecutionStart`；这个事件表示开始处理，不保证实际执行。
依次检查：

1. 整个响应若为 `Length`，立即生成截断错误；不调用 validator，也不执行任何工具。
2. 查找固定工具表，找不到就生成错误。
3. `arguments_error` 非空就生成解析错误，即使 JSON 参数值看起来有效。
4. 发出 `ValidateTool`；只有 validator 成功且未取消才发出 `ExecuteTool`。

例如模型本来要生成 `echo("hello")`，被截断后留下合法参数 `"hel"`。
仅检查参数类型无法发现语义上的不完整，因此必须根据整个响应的 Length 拒绝执行。

预检或校验失败没有执行耗时。实际 execute 的成功和失败均报告单调时钟耗时，
不包括 validator 和事件回调。工具返回的 `details` 与 `is_error` 都保留。

本版没有 retry、崩溃恢复或外部幂等性保证。同一调用位置在一次未崩溃运行中至多调度一次，
不意味着跨运行、跨崩溃的外部副作用 exactly-once。

### 工具注册

`ToolRegistry` 保存同一条目中的工具声明与实现。注册时读取一次声明，拒绝空名称和重复名称；
失败不会覆盖已有工具。声明快照按注册顺序交给 Core，Core 按名称确定索引，执行层再通过同一索引
调用注册工具的 validator 和 execute。运行函数共享借用注册表，运行期间不能注册或替换条目。

`src/tools/mod.rs` 集中创建默认注册表，当前注册用于验证链路的 `Add`；未来工具使用相同的
`AgentTool` 接口和 `register` 入口。`add` 的名称、说明与 Schema 均来自工具自身的声明，
聊天函数只接收注册表，系统提示不列举工具，也没有按工具名称分流的执行分支。

### 消息、事件与继续执行

新运行的 prompts 写入历史并包含在 new_messages；继续入口保留已有历史，不重放其消息事件。
与原版一样，继续入口拒绝空历史和末尾为 assistant 的历史。真实适配器可能还有额外的上下文要求。

模型返回完整消息后才报告 MessageStart / MessageEnd，本版没有流式 MessageUpdate。
每个工具结果的事件顺序是 ToolExecutionEnd → MessageStart → MessageEnd。
当前批次全部完成后才报告 TurnEnd，再开始下一次模型请求。
AgentEnd 恰好一次且为最后一个事件；同步回调返回后，运行函数才结算。

一轮包含一个助手响应和其工具处理；一次 run 可以包含很多轮。
StopReason 的 Stop / ToolUse 都以实际有无调用决定继续，Length 无工具则正常结束，
工具执行错误通常继续交给模型修正，而模型 Error / Aborted 是整个 run 的终止条件。

工具调用以当前 run 的轮次和列表位置来理解，外部 ID 可重复。结果 ID 原样保留，
结果顺序与调用顺序一致。运行结果同时提供完整 context 与本次新增消息，避免调用方猜测历史是否已更新。

### 取消规则

执行层在外部操作启动前及返回后读取 token，将快照放进 Input。Core 不自行读取共享信号。
每个 Effect 执行前让出调度，避免立即返回的模型或工具持续占用执行线程，使取消任务无法运行。

- 运行开始前或请求模型前取消：不发起模型请求，记录一条取消助手消息并结束。
- 等待模型时取消：等待适配器返回，将响应标为 Aborted，不启动其中的工具调用。
- 当前调用正在校验或 execute 尚未启动时取消：为当前已开始处理的调用记录取消错误，然后结束。
- 正在 execute 时取消：不丢弃 Future；等待工具返回，保留其真实成功或失败结果，然后结束。
- 后续未开始处理的调用不执行、不补造结果；取消后的工具批次可能只有部分结果。

取消后不再发起模型或工具调用。事件回调中请求取消也会在下一项外部操作前被检查。
Core 的一次转换可能已经生成若干观察事件或处理若干无需外部操作的错误；不会逐事件打断该转换。

取消是协作式信号，不会强行杀死线程、回滚文件或网络副作用。外部实现若不返回，运行就不会及时结束。
取消信号与“操作启动”的并发竞争也不提供全局原子性；保证针对执行层已经观察到的取消成立。
没有隐含轮数上限：证明终止需要模型在有限轮后停止、外部操作最终返回等额外假设。

## 与 TypeScript 的关系

参照提交：`8efd314fa3d3232ce197d8d7a6a5660530b9d83e`。

| TypeScript 职责 | Rust 本版 |
| --- | --- |
| runAgentLoop / runAgentLoopContinue | 对应两个运行入口与 Core 构造入口 |
| runLoop 的继续/停止和历史管理 | Core 的显式转换 |
| streamAssistantResponse | Model::complete + 完整消息事件；没有 partial stream |
| executeToolCallsSequential | Core 的位置推进 + 执行层的 ValidateTool / ExecuteTool |
| validateToolArguments | 工具自带严格 validator；不复刻 TypeBox coercion 或 optional-null 规则 |
| failToolCallsFromTruncatedMessage | Core 优先拒绝所有 Length 调用 |
| getCurrentTools / declareToolChanges | 固定 tools 随每次 ModelRequest 传递，不迁移 transcript delta 机制 |
| AgentEventSink | 同步 FnMut 回调；不提供 EventStream 或异步 listener |

本版不是原版 API 或 JSON wire format 的兼容层。例如工具声明在请求独立字段中，
工具结果用嵌套结构表示，省略 timestamp、usage、provider 元数据；事件也只包含选定子集。
系统文本保留为历史中的 System 消息。核心库没有自定义消息转换、图片、thinking、真实供应商、并行、队列、
hooks、工具 terminate 提示、进度更新、持久化或 TypeScript 桥接。
`examples/support/deepseek.rs` 在核心库外提供真实 DeepSeek HTTP 适配器，供单次演示和终端聊天共用。
单次演示强制首次调用工具，并限制累计 4 次模型请求；聊天允许模型自行决定是否调用工具。

### 终端对话与单次运行

原版 PI 的交互界面位于 `packages/coding-agent/src/modes/interactive`，终端组件位于 `packages/tui`。
Rust 的 `examples/chat.rs` 只实现逐行输入、完整回答、工具结果显示和基本会话命令。

例如“计算 1 + 2”对应一次 run，内部可以请求模型两次；随后输入“刚才的结果是什么”会启动新的 run。
外层对话循环把上次 `RunOutcome.context` 原样交给 `run_agent_loop`，并单独传入新的用户消息。
这样系统提示、用户输入、助手回答和工具结果不会丢失，也不会重复追加。
这里不用 `run_agent_loop_continue`：该入口用于继续尚待处理的历史，不接受末尾为助手的正常完整对话。

模型失败也保留返回的历史并重新等待输入；`/new` 只重置会话历史，恢复初始系统提示。
`/exit`、`/quit` 和 EOF 正常退出，Ctrl+C 由终端默认信号处理直接结束进程。
界面不进入 raw mode 或切换备用屏幕，不需要恢复终端模式。输入只在两次 run 之间读取，运行中没有输入队列。
对话历史不持久化，也没有流式渲染、输入历史导航或全屏编辑器。核心状态机与执行层接口不因此扩展。

## 可以验证什么，尚不能声称什么

建议第一条性质：

> 每个 ExecuteTool Effect 都来自当前调用一次成功的 validation，且该调用不属于截断响应。

对应代码结构：只有 AwaitValidation 收到成功结果、且 cancelled 为 false 的分支构造 ExecuteTool。
Length、Error、Aborted 响应中的调用不会进入这个分支。

其他候选性质：

- 处理位置单调推进，同一调用位置不重复调度；即使外部 ID 重复也成立。
- 未取消且所有操作返回时，每个调用位置产生一个结果，包括各种错误。
- 当前批次结果全部入历史后，才能产生下一次 RequestModel。
- Done 之后不再产生 Effect 或事件。
- 取消时保留已经完成的结果，不再启动后续调用。

15 个测试包含 425 条有限组合轨迹以及 async 取消行为检查，是回归证据，不是全称证明。
未来若选择 Verus，可能需要调整 JSON、Vec、trait 和数据所有权的表示；若使用 TLA+，需要另写模型，
说明模型状态与本实现字段的对应关系；Kani 也需要明确输入和循环检查边界。

只验证 Core 仍需明确这些环境假设或单独证明它们：

- 执行层按序处理唯一 Effect，并回送对应结果；Input 不是不可信外部消息接入协议。
- validator 正确检查参数，且与声明的 Schema 一致；Core 接收到“成功”不等于证明参数真的合法。
- 工具表在运行中固定，执行层不绕开 Core 自行调用工具。
- 模型、工具和事件回调不 panic；外部操作按要求最终返回并协作处理取消。
- 模型和工具的内部正确性、网络行为、内存耗尽和跨崩溃副作用未纳入当前保证。

给学长展示原型后应确认：选定子集是否足够、首条性质、证明 Core 还是实际 Rust 全路径、
环境假设是否可接受，以及工具与下一阶段验收证据。本轮不预先承诺某个证明工具可直接接受全部代码。
