//! Core 与执行层共用的数据契约，不包含网络客户端或供应商配置。

use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Rust 的 enum 表示几个互斥的分支，对应 TypeScript 的消息联合类型。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    /// 固定系统提示；本版不支持动态工具声明或提示分节。
    System {
        /// 系统提示文本。
        content: String,
    },
    /// 用户的文本输入。
    User {
        /// 用户输入文本。
        content: String,
    },
    /// 模型返回的完整消息。
    Assistant(AssistantMessage),
    /// 某次工具调用的结果。
    ToolResult(ToolResultMessage),
}

/// 模型生成的一段内容；保留文本与工具调用的原始顺序。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AssistantContent {
    /// 模型输出的文本。
    Text {
        /// 文本内容。
        text: String,
    },
    /// 模型请求 runtime 调用工具。
    ToolCall(ToolCall),
}

/// 工具调用 ID 仅用于关联结果；Core 依靠轮次和位置推进，不要求 ID 全局唯一。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// 模型提供的调用 ID。
    pub id: String,
    /// 目标工具的名称。
    pub name: String,
    /// 原始 JSON 参数，由工具自己的 validator 严格检查。
    pub arguments: Value,
    /// 适配器无法解析完整参数时填写；即使 arguments 有值也禁止执行。
    pub arguments_error: Option<String>,
}

/// 只包含第一版会处理的最终停止原因，不接收 partial/pending 响应。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    /// 正常完成；是否继续仍由消息中有无工具调用决定。
    Stop,
    /// 模型请求使用工具。
    ToolUse,
    /// 达到输出上限；此响应中的工具调用全部拒绝执行。
    Length,
    /// 模型或适配器出错。
    Error,
    /// 操作取消。
    Aborted,
}

/// 完整的模型响应。本版不携带 token 用量、图片或供应商专用字段。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// 文本与工具调用序列。
    pub content: Vec<AssistantContent>,
    /// 模型为什么结束这次响应。
    pub stop_reason: StopReason,
    /// 错误或取消时的说明。
    pub error_message: Option<String>,
}

/// 对模型可见的工具声明；parameters 是描述性 JSON Schema。
///
/// 本库不解释 Schema，也不证明它与工具的 validator 等价；二者一致是工具作者的责任。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDeclaration {
    /// 用于查找执行实现的名称，同一次运行中必须唯一。
    pub name: String,
    /// 告诉模型工具的作用。
    pub description: String,
    /// 向模型说明参数结构，实际校验由 AgentTool::validate_arguments 完成。
    pub parameters: Value,
}

/// 工具执行产生的结果。失败也可以直接用 is_error 表示。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentToolResult {
    /// 发给模型的文本块，空列表表示没有文本。
    pub content: Vec<String>,
    /// 供调用方读取的结构化详情。
    pub details: Value,
    /// 明确区分成功与工具报告的失败。
    pub is_error: bool,
}

impl AgentToolResult {
    /// 把错误转成普通工具结果，让模型有机会修正下一次调用。
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![message.into()],
            details: Value::Null,
            is_error: true,
        }
    }
}

/// 历史中与原始工具调用关联的结果消息。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResultMessage {
    /// 原始调用 ID，可重复；结果顺序与调用位置相对应。
    pub tool_call_id: String,
    /// 原始工具名。
    pub tool_name: String,
    /// 工具结果或 runtime 生成的错误结果。
    pub result: AgentToolResult,
    /// 仅实际 execute 的耗时，不包括校验；未执行时为 None。
    pub duration_ms: Option<u64>,
}

/// 调用方传入或取回的会话历史；工具表由运行接口单独传入。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentContext {
    /// 按发生顺序记录的完整消息。
    pub messages: Vec<Message>,
}

/// 每次模型请求都收到历史快照和同一份固定工具声明。
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRequest {
    /// 包含系统消息的完整历史。
    pub messages: Vec<Message>,
    /// 本次运行可使用的工具声明。
    pub tools: Vec<ToolDeclaration>,
}

/// 一次运行的结束原因，与单次模型响应的 StopReason 不同。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndReason {
    /// 模型正常回答，且没有后续工具调用。
    Completed,
    /// 模型或其适配器失败。
    Failed,
    /// 模型报告取消或调用方请求取消。
    Aborted,
}

/// 正常结算的运行结果；失败或取消也返回已经积累的历史。
#[derive(Clone, Debug, PartialEq)]
pub struct RunOutcome {
    /// 可传给下一次运行的完整历史。
    pub context: AgentContext,
    /// 仅本次新增的消息；新运行包含 prompts，继续运行不包含旧历史。
    pub new_messages: Vec<Message>,
    /// 本次运行如何结束。
    pub reason: EndReason,
}

/// 供 UI、日志或测试观察的事件，不是驱动 Core 的 Input。
///
/// 本版只报告完整消息，不报告模型片段或工具进度。回调应正常返回；不捕获 panic。
#[derive(Clone, Debug, PartialEq)]
pub enum AgentEvent {
    /// 一次运行开始。
    AgentStart,
    /// 一次模型响应及其工具处理开始。
    TurnStart,
    /// 一条完整消息开始；本版紧接着报告 MessageEnd。
    MessageStart(Message),
    /// 一条消息已经写入 Core 历史。
    MessageEnd(Message),
    /// 开始处理工具调用；包含可能不会实际执行的调用。
    ToolExecutionStart(ToolCall),
    /// 调用处理完成，包括校验错误或截断拒绝等结果。
    ToolExecutionEnd(ToolResultMessage),
    /// 本轮助手响应与已处理的工具结果已经入历史。
    TurnEnd {
        /// 本轮模型响应。
        message: AssistantMessage,
        /// 本轮实际记录的结果；取消后未启动的调用没有结果。
        tool_results: Vec<ToolResultMessage>,
    },
    /// 最后一个事件，包含本次新增消息与结束原因。
    AgentEnd {
        /// 本次新增消息。
        messages: Vec<Message>,
        /// 结束原因。
        reason: EndReason,
    },
}

/// Result<T, OperationError> 对应显式的成功/失败返回，不使用异常控制正常流程。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationError(pub String);

impl fmt::Display for OperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for OperationError {}

/// 调用参数或状态机协议错误；与会被记录进消息历史的模型/工具失败区分。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoopError {
    /// 没有历史可继续。
    EmptyContinuation,
    /// 最后一条助手消息不满足原版继续入口的要求。
    AssistantContinuation,
    /// 工具名称为空或重复。
    InvalidToolName(String),
    /// 外层把操作结果交给了不匹配的 State；该输入不会修改 Core。
    UnexpectedInput,
    /// 尚未到终态就试图取出运行结果。
    NotFinished,
}

impl fmt::Display for LoopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyContinuation => f.write_str("Cannot continue: no messages in context"),
            Self::AssistantContinuation => {
                f.write_str("Cannot continue from message role: assistant")
            }
            Self::InvalidToolName(name) => {
                write!(f, "Tool names must be nonempty and unique: {name:?}")
            }
            Self::UnexpectedInput => f.write_str("Input does not match the current state"),
            Self::NotFinished => f.write_str("The agent loop has not finished"),
        }
    }
}

impl Error for LoopError {}
