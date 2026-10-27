//! 最小终端聊天：逐行输入、保留上下文、显示完整回答和工具结果。

use std::env;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::time::Duration;

#[path = "support/deepseek.rs"]
mod deepseek;

use deepseek::{DeepSeek, configuration};
use pi_agent_loop::tools::default_registry;
use pi_agent_loop::{
    AgentContext, AgentEvent, AssistantContent, AssistantMessage, BoxFuture, CancellationToken,
    EndReason, Message, Model, ModelRequest, OperationError, StopReason, ToolRegistry,
    run_agent_loop,
};
use reqwest::Client;

#[cfg(test)]
#[path = "support/chat_tests.rs"]
mod tests;

const HELP: &str = "输入文本后按 Enter 发送；/new 新建会话；/help 帮助；/exit 退出。";
const SYSTEM: &str = "You are a helpful assistant. Use the available tools when appropriate.";

fn new_context() -> AgentContext {
    AgentContext {
        messages: vec![Message::System {
            content: SYSTEM.into(),
        }],
    }
}

async fn chat<M: Model, R: BufRead, W: Write>(
    model: &mut M,
    tools: &ToolRegistry,
    input: &mut R,
    output: &mut W,
    label: &str,
) -> io::Result<()> {
    writeln!(output, "PI Rust | {label}\n{HELP}")?;
    let mut context = new_context();
    let mut line = String::new();
    loop {
        write!(output, "\nuser> ")?;
        output.flush()?;
        line.clear();
        // 只在空闲时阻塞读取；一次输入的模型/工具循环结束后才读取下一行。
        if input.read_line(&mut line)? == 0 {
            writeln!(output)?;
            return Ok(());
        }
        let prompt = line.trim();
        match prompt {
            "" => continue,
            "/exit" | "/quit" => return Ok(()),
            "/help" => {
                writeln!(output, "{HELP}")?;
                continue;
            }
            "/new" => {
                context = new_context();
                writeln!(output, "已新建会话。")?;
                continue;
            }
            command if command.starts_with('/') => {
                writeln!(output, "未知命令：{command}\n{HELP}")?;
                continue;
            }
            _ => {}
        }
        writeln!(output, "等待模型回答...")?;
        output.flush()?;
        let cancellation = CancellationToken::new();
        let mut write_result = Ok(());
        let outcome = run_agent_loop(
            vec![Message::User {
                content: prompt.into(),
            }],
            context,
            model,
            tools,
            &cancellation,
            &mut |event| {
                if write_result.is_ok() {
                    write_result = render_event(output, event);
                    if write_result.is_err() {
                        cancellation.cancel();
                    }
                }
            },
        )
        .await
        .map_err(io::Error::other)?;
        write_result?;
        // context 已包含新用户输入和所有模型/工具消息，不重复追加 new_messages。
        context = outcome.context;
        if outcome.reason != EndReason::Completed {
            writeln!(output, "本次请求未完成，可继续输入或用 /new 新建会话。")?;
        }
    }
}

fn render_event(output: &mut impl Write, event: &AgentEvent) -> io::Result<()> {
    match event {
        AgentEvent::MessageEnd(Message::Assistant(message)) => {
            for content in &message.content {
                if let AssistantContent::Text { text } = content {
                    writeln!(output, "assistant> {text}")?;
                }
            }
            if let Some(error) = &message.error_message {
                writeln!(output, "Error: {error}")?;
            }
            if message.stop_reason == StopReason::Length {
                writeln!(output, "Response was truncated before completion.")?;
            }
        }
        AgentEvent::ToolExecutionStart(call) => {
            writeln!(output, "tool> {} {}", call.name, call.arguments)?;
        }
        AgentEvent::ToolExecutionEnd(result) => {
            let status = if result.result.is_error {
                "error"
            } else {
                "result"
            };
            writeln!(
                output,
                "tool {status}> {}",
                result.result.content.join("\n")
            )?;
        }
        _ => {}
    }
    output.flush()
}

/// 离线模式只回显用户输入与上一条输入，用于确认交互和历史保留；不是 LLM。
struct Mock;

impl Model for Mock {
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest,
        _cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<AssistantMessage, OperationError>> {
        Box::pin(async move {
            let prompts: Vec<&str> = request
                .messages
                .iter()
                .filter_map(|message| match message {
                    Message::User { content } => Some(content.as_str()),
                    _ => None,
                })
                .collect();
            let current = prompts.last().copied().unwrap_or_default();
            let previous = prompts.iter().rev().nth(1).copied().unwrap_or("无");
            Ok(AssistantMessage {
                content: vec![AssistantContent::Text {
                    text: format!("[离线回显] {current}；上一条输入：{previous}"),
                }],
                stop_reason: StopReason::Stop,
                error_message: None,
            })
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let tools = default_registry()?;
    match arguments.as_slice() {
        [] => {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".env.local");
            let (api_key, model_name) = configuration(&path, |name| env::var(name).ok())?;
            let mut model = DeepSeek {
                client: Client::builder().timeout(Duration::from_secs(60)).build()?,
                endpoint: "https://api.deepseek.com/chat/completions".into(),
                api_key,
                model: model_name.clone(),
                requests: 0,
                demo: false,
            };
            chat(
                &mut model,
                &tools,
                &mut io::stdin().lock(),
                &mut io::stdout().lock(),
                &model_name,
            )
            .await?;
        }
        [argument] if argument == "--mock" => {
            chat(
                &mut Mock,
                &tools,
                &mut io::stdin().lock(),
                &mut io::stdout().lock(),
                "离线回显，无 API 请求",
            )
            .await?;
        }
        [argument] if argument == "--help" || argument == "-h" => {
            println!("cargo run --locked --example chat [-- --mock]\n{HELP}");
        }
        _ => return Err("Unknown arguments; use --help".into()),
    }
    Ok(())
}
