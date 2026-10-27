//! 执行层：依次解释 Core 发出的 Effect，再将操作结果送回 Core。
//!
//! 不使用后台任务，也不在取消时丢弃正在执行的 Future。Future 可以借用模型或工具，
//! 由调用方的 async executor 持续轮询直到返回；不会因此自动获得线程并行能力。

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use serde_json::Value;
use tokio::task::yield_now;
use tokio_util::sync::CancellationToken;

use crate::core::{Core, Effect, Input, aborted_message};
use crate::tool_registry::ToolRegistry;
use crate::types::{
    AgentContext, AgentEvent, AgentToolResult, AssistantMessage, LoopError, Message, ModelRequest,
    OperationError, RunOutcome, StopReason, ToolDeclaration,
};

/// 不同 async 函数的 Future 大小不同，用 Box 放到堆上以支持统一的 trait 接口。
///
/// Pin 保证轮询期间 Future 不被移动；'a 表示它可以借用调用期间仍然存活的对象。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 模型适配器契约。trait 类似 TypeScript interface；本库不提供真实供应商适配器。
pub trait Model {
    /// 返回完整响应；Err 会转成 stop_reason=Error 的助手消息。
    ///
    /// 必须协作处理 cancellation，并最终返回。返回取消响应也会结束整个 run。
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<AssistantMessage, OperationError>>;
}

/// 可在一次运行中固定注册的工具。声明、validator 与实现必须彼此一致。
pub trait AgentTool: Send + Sync {
    /// 提供给模型的名称、说明和参数 Schema；注册表在注册时读取一次并缓存。
    fn declaration(&self) -> ToolDeclaration;

    /// 严格检查参数，成功时返回将传给 execute 的 JSON 值。
    ///
    /// 不应执行外部副作用，不应隐式转换类型。validator 本身的正确性属于环境假设。
    fn validate_arguments(&self, arguments: &Value) -> Result<Value, OperationError>;

    /// 执行已校验参数。Err 或 is_error=true 都会成为工具错误结果。
    ///
    /// 必须协作处理 cancellation，并最终返回。取消不能撤销已经发生的副作用。
    fn execute<'a>(
        &'a self,
        tool_call_id: &'a str,
        arguments: Value,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<AgentToolResult, OperationError>>;
}

/// 启动一次新运行，新增消息包含 prompts，调用方传入的历史按值移入 Core。
///
/// emit 同步接收有序事件；应快速、正常返回，不在其中阻塞或 panic。
/// 不设置隐含轮数上限：模型持续请求工具时，需调用方取消才能终止。
pub async fn run_agent_loop<M: Model, E: FnMut(&AgentEvent) + ?Sized>(
    prompts: Vec<Message>,
    context: AgentContext,
    model: &mut M,
    tools: &ToolRegistry,
    cancellation: &CancellationToken,
    emit: &mut E,
) -> Result<RunOutcome, LoopError> {
    let core = Core::new(prompts, context, tools.declarations())?;
    drive(core, model, tools, cancellation, emit).await
}

/// 继续已有历史，不重复报告旧消息，也不把旧消息包含在 new_messages 中。
pub async fn run_agent_loop_continue<M: Model, E: FnMut(&AgentEvent) + ?Sized>(
    context: AgentContext,
    model: &mut M,
    tools: &ToolRegistry,
    cancellation: &CancellationToken,
    emit: &mut E,
) -> Result<RunOutcome, LoopError> {
    let core = Core::continue_from_context(context, tools.declarations())?;
    drive(core, model, tools, cancellation, emit).await
}

async fn drive<M: Model, E: FnMut(&AgentEvent) + ?Sized>(
    mut core: Core,
    model: &mut M,
    tools: &ToolRegistry,
    cancellation: &CancellationToken,
    emit: &mut E,
) -> Result<RunOutcome, LoopError> {
    let mut step = core.advance(Input::Start {
        cancelled: cancellation.is_cancelled(),
    })?;
    loop {
        for event in &step.events {
            emit(event);
        }
        let Some(effect) = step.effect else {
            return core.into_outcome();
        };
        // 即使外部 Future 立即返回，也让并发的取消任务有机会运行。
        yield_now().await;
        let input = match effect {
            Effect::RequestModel(request) => {
                let message = if cancellation.is_cancelled() {
                    aborted_message()
                } else {
                    model
                        .complete(request, cancellation)
                        .await
                        .unwrap_or_else(|error| AssistantMessage {
                            content: Vec::new(),
                            stop_reason: StopReason::Error,
                            error_message: Some(error.0),
                        })
                };
                Input::ModelReturned {
                    message,
                    cancelled: cancellation.is_cancelled(),
                }
            }
            Effect::ValidateTool { tool_index, call } => {
                let result = if cancellation.is_cancelled() {
                    Err(OperationError("Operation aborted".into()))
                } else {
                    tools.tool(tool_index).validate_arguments(&call.arguments)
                };
                Input::ArgumentsValidated {
                    result,
                    cancelled: cancellation.is_cancelled(),
                }
            }
            Effect::ExecuteTool {
                tool_index,
                call,
                arguments,
            } => {
                let (result, duration_ms) = if cancellation.is_cancelled() {
                    (Err(OperationError("Operation aborted".into())), None)
                } else {
                    let started = Instant::now();
                    let result = tools
                        .tool(tool_index)
                        .execute(&call.id, arguments, cancellation)
                        .await;
                    // 将时钟留在执行层，Core 的转换与真实时间无关。
                    let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    (result, Some(millis))
                };
                Input::ToolReturned {
                    result,
                    duration_ms,
                    cancelled: cancellation.is_cancelled(),
                }
            }
        };
        step = core.advance(input)?;
    }
}
