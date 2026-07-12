mod error;
mod handler;
pub mod notification;
mod protocol;
pub mod proxy;
mod server;
mod tools;

pub use error::{McpError, Result};
pub use handler::{
    CancelToken, CompositeToolHandler, ContextRefreshRequest, ContextRefreshToolHandler,
    NotificationToolHandler, PtyToolHandler, ReplayToolHandler, SchedulerToolHandler,
    ScratchpadToolHandler, SharedContextRefreshQueue, SharedNotificationRegistry,
    SharedScheduler, SharedScratchpad, SharedSessionManager, SharedSidebarMessages,
    SharedTuiState, SidebarDirtyFlag, SidebarMessageToolHandler, ToolHandler, TuiHighlight,
    TuiState, TuiToolHandler,
};
pub use protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
pub use server::McpServer;
pub use tools::{
    context_refresh_tool_definitions, notification_tool_definitions, pty_tool_definitions,
    replay_tool_definitions, scheduler_tool_definitions, scratchpad_tool_definitions,
    sidebar_tool_definitions, tui_tool_definitions,
};
