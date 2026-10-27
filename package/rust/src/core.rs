//! 显式状态机：只依据输入修改数据并产生 Effect，不实际执行外部操作。
//!
//! 这是便于后续验证的普通 Rust 原型，不是已通过 Verus 等工具证明的代码。

use std::collections::BTreeSet;

use serde_json::Value;

use crate::types::{
    AgentContext, AgentEvent, AgentToolResult, AssistantContent, AssistantMessage, EndReason,
    LoopError, Message, ModelRequest, OperationError, RunOutcome, StopReason, ToolCall,
    ToolDeclaration, ToolResultMessage,
};

/// State 表示流程阶段；具体历史和调用位置由 Core 的私有字段保存。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// 已检查入口参数，还没有报告生命周期事件。
    Ready,
    /// 等待完整模型响应。
    AwaitModel,
    /// 等待当前工具参数的校验结果。
    AwaitValidation,
    /// 已校验通过，等待当前工具的执行结果。
    AwaitTool,
    /// 终态；之后的输入不再产生事件或动作。
    Done(EndReason),
}

/// 执行层送回的 Input。cancelled 是读取取消信号后的快照，不由 Core 读取外界。
///
/// 同一 Core 只允许一个未完成的 Effect；执行层必须按顺序回送对应的结果。
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// 开始运行。
    Start {
        /// 开始前是否已经取消。
        cancelled: bool,
    },
    /// 模型响应或执行层合成的模型错误响应。
    ModelReturned {
        /// 完整响应。
        message: AssistantMessage,
        /// 模型返回时是否已取消。
        cancelled: bool,
    },
    /// 当前工具的参数校验结果。
    ArgumentsValidated {
        /// 成功时是将要交给 execute 的参数，不再进行隐式转换。
        result: Result<Value, OperationError>,
        /// 校验结束时是否已取消。
        cancelled: bool,
    },
    /// 当前工具执行的结果。
    ToolReturned {
        /// 工具报告的结果，或执行失败。
        result: Result<AgentToolResult, OperationError>,
        /// 实际执行耗时；取消导致 execute 未启动时为 None。
        duration_ms: Option<u64>,
        /// 执行返回时是否已取消。
        cancelled: bool,
    },
}

/// Core 发出的外部动作。它只是数据，构造此值不会触发工具或网络请求。
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// 用当前历史请求模型。
    RequestModel(ModelRequest),
    /// 校验指定工具的参数。
    ValidateTool {
        /// 固定工具表中的位置。
        tool_index: usize,
        /// 当前调用，包含原始参数。
        call: ToolCall,
    },
    /// 执行已经通过校验的工具。
    ExecuteTool {
        /// 固定工具表中的位置。
        tool_index: usize,
        /// 当前调用。
        call: ToolCall,
        /// validator 明确返回的参数。
        arguments: Value,
    },
}

/// 一次转换的输出：先依次报告 events，再处理唯一的 effect。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Step {
    /// 对外可观察的事件，顺序有意义。
    pub events: Vec<AgentEvent>,
    /// 下一项外部操作；终态时为 None。
    pub effect: Option<Effect>,
}

/// 调度核心。字段私有，避免调用方绕过转换规则修改 State 或工具位置。
///
/// 消息与工具声明的所有权移入 Core；执行层只能拿到只读历史或 Effect 中的快照。
pub struct Core {
    state: State,
    context: AgentContext,
    tools: Vec<ToolDeclaration>,
    prompts: Vec<Message>,
    initial_length: usize,
    assistant: Option<AssistantMessage>,
    calls: Vec<ToolCall>,
    call_index: usize,
    tool_results: Vec<ToolResultMessage>,
}

impl Core {
    /// 从现有历史和新提示建立运行；此时尚未产生任何事件。
    pub fn new(
        prompts: Vec<Message>,
        context: AgentContext,
        tools: Vec<ToolDeclaration>,
    ) -> Result<Self, LoopError> {
        let mut names = BTreeSet::new();
        for tool in &tools {
            if tool.name.is_empty() || !names.insert(tool.name.clone()) {
                return Err(LoopError::InvalidToolName(tool.name.clone()));
            }
        }
        Ok(Self {
            state: State::Ready,
            initial_length: context.messages.len(),
            context,
            tools,
            prompts,
            assistant: None,
            calls: Vec::new(),
            call_index: 0,
            tool_results: Vec::new(),
        })
    }

    /// 不添加提示，继续已有历史；保留 TypeScript 入口的两个基本检查。
    pub fn continue_from_context(
        context: AgentContext,
        tools: Vec<ToolDeclaration>,
    ) -> Result<Self, LoopError> {
        match context.messages.last() {
            None => return Err(LoopError::EmptyContinuation),
            Some(Message::Assistant(_)) => return Err(LoopError::AssistantContinuation),
            _ => {}
        }
        Self::new(Vec::new(), context, tools)
    }

    /// 读取当前阶段；不能通过此接口修改它。
    pub fn state(&self) -> State {
        self.state
    }

    /// 借用只读历史；& 表示借用，历史的所有权仍属于 Core。
    pub fn context(&self) -> &AgentContext {
        &self.context
    }

    /// 消费一个匹配当前阶段的输入，返回下一步动作。
    ///
    /// 错误阶段的输入不会改变数据；终态忽略后续输入，确保不会重复调度。
    pub fn advance(&mut self, input: Input) -> Result<Step, LoopError> {
        if matches!(self.state, State::Done(_)) {
            return Ok(Step::default());
        }
        let matches_state = matches!(
            (&self.state, &input),
            (State::Ready, Input::Start { .. })
                | (State::AwaitModel, Input::ModelReturned { .. })
                | (State::AwaitValidation, Input::ArgumentsValidated { .. })
                | (State::AwaitTool, Input::ToolReturned { .. })
        );
        if !matches_state {
            return Err(LoopError::UnexpectedInput);
        }

        let mut step = Step::default();
        match input {
            Input::Start { cancelled } => {
                step.events.push(AgentEvent::AgentStart);
                step.events.push(AgentEvent::TurnStart);
                for prompt in std::mem::take(&mut self.prompts) {
                    self.append_message(prompt, &mut step);
                }
                if cancelled {
                    self.receive_model(aborted_message(), true, &mut step);
                } else {
                    self.request_model(&mut step);
                }
            }
            Input::ModelReturned { message, cancelled } => {
                self.receive_model(message, cancelled, &mut step);
            }
            Input::ArgumentsValidated { result, cancelled } => {
                if cancelled {
                    self.record_tool(AgentToolResult::error("Operation aborted"), None, &mut step);
                    self.finish_turn(&mut step);
                    self.finish(EndReason::Aborted, &mut step);
                } else {
                    match result {
                        Ok(arguments) => {
                            let call = self.calls[self.call_index].clone();
                            // 只有这个分支能产生 ExecuteTool：工具存在，参数校验成功，
                            // 且 receive_model 已阻止 length/error/aborted 响应进入此阶段。
                            let tool_index = self
                                .tools
                                .iter()
                                .position(|t| t.name == call.name)
                                .expect("AwaitValidation always refers to an existing tool");
                            self.state = State::AwaitTool;
                            step.effect = Some(Effect::ExecuteTool {
                                tool_index,
                                call,
                                arguments,
                            });
                        }
                        Err(error) => {
                            self.record_tool(AgentToolResult::error(error.0), None, &mut step);
                            self.next_tool(&mut step);
                        }
                    }
                }
            }
            Input::ToolReturned {
                result,
                duration_ms,
                cancelled,
            } => {
                let result = result.unwrap_or_else(|error| AgentToolResult::error(error.0));
                self.record_tool(result, duration_ms, &mut step);
                if cancelled {
                    self.finish_turn(&mut step);
                    self.finish(EndReason::Aborted, &mut step);
                } else {
                    self.next_tool(&mut step);
                }
            }
        }
        Ok(step)
    }

    /// 取走终态结果。self 按值传入，表示 Core 的所有权被消费，不能再次使用。
    pub fn into_outcome(self) -> Result<RunOutcome, LoopError> {
        let State::Done(reason) = self.state else {
            return Err(LoopError::NotFinished);
        };
        Ok(RunOutcome {
            new_messages: self.context.messages[self.initial_length..].to_vec(),
            context: self.context,
            reason,
        })
    }

    fn request_model(&mut self, step: &mut Step) {
        self.state = State::AwaitModel;
        step.effect = Some(Effect::RequestModel(ModelRequest {
            messages: self.context.messages.clone(),
            tools: self.tools.clone(),
        }));
    }

    fn receive_model(&mut self, mut message: AssistantMessage, cancelled: bool, step: &mut Step) {
        if cancelled {
            message.stop_reason = StopReason::Aborted;
            message.error_message = Some("Operation aborted".into());
        }
        self.append_message(Message::Assistant(message.clone()), step);
        self.assistant = Some(message.clone());
        self.tool_results.clear();
        self.calls = message
            .content
            .iter()
            .filter_map(|content| match content {
                AssistantContent::ToolCall(call) => Some(call.clone()),
                AssistantContent::Text { .. } => None,
            })
            .collect();
        self.call_index = 0;

        match message.stop_reason {
            StopReason::Error | StopReason::Aborted => {
                self.finish_turn(step);
                self.finish(
                    if message.stop_reason == StopReason::Error {
                        EndReason::Failed
                    } else {
                        EndReason::Aborted
                    },
                    step,
                );
            }
            _ if self.calls.is_empty() => {
                self.finish_turn(step);
                self.finish(EndReason::Completed, step);
            }
            _ => self.next_tool(step),
        }
    }

    fn next_tool(&mut self, step: &mut Step) {
        while let Some(call) = self.calls.get(self.call_index).cloned() {
            step.events
                .push(AgentEvent::ToolExecutionStart(call.clone()));
            // length 的优先级高于查工具和参数校验：合法的 JSON 仍可能是不完整参数。
            let immediate_error = if self
                .assistant
                .as_ref()
                .is_some_and(|message| message.stop_reason == StopReason::Length)
            {
                Some(format!(
                    "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                    call.name,
                ))
            } else if !self.tools.iter().any(|tool| tool.name == call.name) {
                Some(format!("Tool {} not found", call.name))
            } else {
                call.arguments_error
                    .as_ref()
                    .map(|error| format!("Invalid arguments for tool \"{}\": {error}", call.name))
            };
            if let Some(error) = immediate_error {
                self.record_tool(AgentToolResult::error(error), None, step);
                continue;
            }
            let tool_index = self
                .tools
                .iter()
                .position(|tool| tool.name == call.name)
                .expect("unknown tools were handled above");
            self.state = State::AwaitValidation;
            step.effect = Some(Effect::ValidateTool { tool_index, call });
            return;
        }

        // 所有工具结果已在 record_tool 中入历史，才能开始下一次模型请求。
        self.finish_turn(step);
        step.events.push(AgentEvent::TurnStart);
        self.request_model(step);
    }

    fn record_tool(&mut self, result: AgentToolResult, duration_ms: Option<u64>, step: &mut Step) {
        let call = &self.calls[self.call_index];
        let message = ToolResultMessage {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            result,
            duration_ms,
        };
        step.events
            .push(AgentEvent::ToolExecutionEnd(message.clone()));
        self.append_message(Message::ToolResult(message.clone()), step);
        self.tool_results.push(message);
        self.call_index += 1;
    }

    fn append_message(&mut self, message: Message, step: &mut Step) {
        self.context.messages.push(message.clone());
        step.events.push(AgentEvent::MessageStart(message.clone()));
        step.events.push(AgentEvent::MessageEnd(message));
    }

    fn finish_turn(&self, step: &mut Step) {
        step.events.push(AgentEvent::TurnEnd {
            message: self
                .assistant
                .clone()
                .expect("a turn ends only after a model response"),
            tool_results: self.tool_results.clone(),
        });
    }

    fn finish(&mut self, reason: EndReason, step: &mut Step) {
        self.state = State::Done(reason);
        step.events.push(AgentEvent::AgentEnd {
            messages: self.context.messages[self.initial_length..].to_vec(),
            reason,
        });
    }
}

/// 无法发起模型请求时使用的取消响应，也供执行层在请求前检测到取消时使用。
pub(crate) fn aborted_message() -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        stop_reason: StopReason::Aborted,
        error_message: Some("Operation aborted".into()),
    }
}
