//! 模型是 mock，Core 与 Add 工具是真实实现；无需联网或 API key。

use std::collections::VecDeque;

use pi_agent_loop::tools::default_registry;
use pi_agent_loop::{
    AgentContext, AgentEvent, AssistantContent, AssistantMessage, BoxFuture, CancellationToken,
    Message, Model, ModelRequest, OperationError, StopReason, ToolCall, run_agent_loop,
};
use serde_json::json;

struct ScriptedModel {
    responses: VecDeque<AssistantMessage>,
}

impl Model for ScriptedModel {
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<AssistantMessage, OperationError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(OperationError("Operation aborted".into()));
            }
            println!(
                "\n[模拟模型] 收到 {} 条历史消息、{} 个工具声明",
                request.messages.len(),
                request.tools.len()
            );
            // 第二次响应前检查真实工具结果；答案仍是脚本，不是 LLM 推理得到的。
            if self.responses.len() == 1 {
                let Some(Message::ToolResult(result)) = request.messages.last() else {
                    return Err(OperationError(
                        "Expected a tool result before the final answer".into(),
                    ));
                };
                if result.result.is_error || result.result.details != json!({ "sum": 3 }) {
                    return Err(OperationError("Unexpected add result".into()));
                }
            }
            self.responses
                .pop_front()
                .ok_or_else(|| OperationError("Mock response script exhausted".into()))
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut model = ScriptedModel {
        responses: VecDeque::from([
            AssistantMessage {
                content: vec![AssistantContent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({ "a": 1, "b": 2 }),
                    arguments_error: None,
                })],
                stop_reason: StopReason::ToolUse,
                error_message: None,
            },
            AssistantMessage {
                content: vec![AssistantContent::Text {
                    text: "答案是 3".into(),
                }],
                stop_reason: StopReason::Stop,
                error_message: None,
            },
        ]),
    };
    let tools = default_registry()?;
    let cancellation = CancellationToken::new();
    let outcome = run_agent_loop(
        vec![Message::User {
            content: "计算 1 + 2".into(),
        }],
        AgentContext {
            messages: vec![Message::System {
                content: "使用 add 工具计算整数加法。".into(),
            }],
        },
        &mut model,
        &tools,
        &cancellation,
        &mut |event| match event {
            AgentEvent::ToolExecutionStart(call) => {
                println!("[工具请求] {} {}", call.name, call.arguments)
            }
            AgentEvent::ToolExecutionEnd(result) => {
                println!("[工具结果] {:?}", result.result.content)
            }
            AgentEvent::MessageEnd(Message::Assistant(message)) => {
                for content in &message.content {
                    if let AssistantContent::Text { text } = content {
                        println!("[助手消息] {text}");
                    }
                }
            }
            AgentEvent::AgentEnd { reason, .. } => println!("\n[结束] {reason:?}"),
            _ => {}
        },
    )
    .await?;
    println!(
        "本次新增 {} 条消息，完整历史 {} 条消息。",
        outcome.new_messages.len(),
        outcome.context.messages.len()
    );
    Ok(())
}
