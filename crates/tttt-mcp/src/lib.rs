mod error;
mod handler;
pub mod notification;
mod protocol;
pub mod proxy;
mod server;
mod tools;

pub use error::{McpError, Result};
pub use handler::{
    CompositeToolHandler, NotificationToolHandler, PtyToolHandler, SchedulerToolHandler,
    ScratchpadToolHandler, SharedNotificationRegistry, SharedScheduler, SharedSessionManager,
    ToolHandler,
};
pub use protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
pub use server::McpServer;
pub use tools::{
    notification_tool_definitions, pty_tool_definitions, scheduler_tool_definitions,
    scratchpad_tool_definitions,
};
