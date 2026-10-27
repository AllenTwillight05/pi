//! 独立于 TypeScript pi 的最小 agent loop。
//!
//! [`Core`] 只处理状态转换；[`run_agent_loop`] 在外层执行模型和工具调用。
//! 用 `cargo run --example add` 查看无需网络、无需 API key 的完整示例。
//! 设计边界与 Rust 入门说明见包内 README 和 `docs/design.md`。
//!
//! 不执行任何模型或工具，也可以直接观察 Core 的第一步动作：
//!
//! ```
//! use pi_agent_loop::{AgentContext, Core, Effect, Input, Message, State};
//!
//! let mut core = Core::new(
//!     vec![Message::User { content: "你好".into() }],
//!     AgentContext::default(),
//!     Vec::new(),
//! )?;
//! let step = core.advance(Input::Start { cancelled: false })?;
//! assert_eq!(core.state(), State::AwaitModel);
//! assert!(matches!(step.effect, Some(Effect::RequestModel(_))));
//! # Ok::<(), pi_agent_loop::LoopError>(())
//! ```

pub mod core;
pub mod runtime;
pub mod tool_registry;
pub mod tools;
pub mod types;

pub use core::{Core, Effect, Input, State, Step};
pub use runtime::{AgentTool, BoxFuture, Model, run_agent_loop, run_agent_loop_continue};
pub use tokio_util::sync::CancellationToken;
pub use tool_registry::ToolRegistry;
pub use types::*;
