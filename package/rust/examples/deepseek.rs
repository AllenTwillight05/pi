//! DeepSeek 真实模型 → 本地 add 工具 → DeepSeek 最终回答。

use std::env;
use std::path::Path;
use std::time::Duration;

#[path = "support/deepseek.rs"]
mod deepseek;

use deepseek::{DeepSeek, configuration};
use pi_agent_loop::tools::default_registry;
use pi_agent_loop::{
    AgentContext, AgentEvent, AssistantContent, CancellationToken, EndReason, Message,
    run_agent_loop,
};
use reqwest::Client;
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".env.local");
    let (api_key, model_name) = configuration(&path, |name| env::var(name).ok())?;
    let mut model = DeepSeek {
        client: Client::builder().timeout(Duration::from_secs(60)).build()?,
        endpoint: "https://api.deepseek.com/chat/completions".into(),
        api_key,
        model: model_name,
        requests: 0,
        demo: true,
    };
    let tools = default_registry()?;
    let outcome = run_agent_loop(
        vec![Message::User {
            content: "使用 add 工具计算 1 + 2，再用中文回答结果。".into(),
        }],
        AgentContext {
            messages: vec![Message::System {
                content:
                    "你是计算助手。计算加法必须调用 add 工具，收到结果后直接回答，不要重复调用。"
                        .into(),
            }],
        },
        &mut model,
        &tools,
        &CancellationToken::new(),
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
                if let Some(error) = &message.error_message {
                    eprintln!("[模型错误] {error}");
                }
            }
            AgentEvent::AgentEnd { reason, .. } => println!("[结束] {reason:?}"),
            _ => {}
        },
    )
    .await?;
    if outcome.reason != EndReason::Completed {
        return Err("DeepSeek run did not complete".into());
    }
    if !outcome.new_messages.iter().any(|message| matches!(
        message, Message::ToolResult(result) if !result.result.is_error && result.result.details == json!({"sum": 3})
    )) {
        return Err("Expected a successful add tool result with sum=3".into());
    }
    Ok(())
}
