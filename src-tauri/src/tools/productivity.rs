//! Google Calendar and Google Tasks tools.
//!
//! They use the same per-profile OAuth token as Gmail, but are independent
//! tools with explicit, approval-spine risk classifications. A Planner screen
//! click is direct human consent; an agent call reaches these tools instead.

use std::future::Future;
use std::pin::Pin;

use chrono::{Duration, Utc};
use serde_json::json;

use crate::email::calendar::CalendarClient;
use crate::email::tasks::TasksClient;
use crate::tools::email::{note_reconnect_if_needed, EmailToolDeps};
use crate::tools::{Capability, ExecCtx, RiskClass, Tool, ToolInput, ToolResult};

const GOOGLE_DESTINATION: &str = "www.googleapis.com";
const MAX_LIST: u32 = 100;

#[derive(Clone)]
pub struct ProductivityToolDeps {
    email: EmailToolDeps,
}

impl ProductivityToolDeps {
    pub fn new(email: EmailToolDeps) -> Self {
        Self { email }
    }

    fn calendar(&self, profile: &str) -> anyhow::Result<CalendarClient> {
        Ok(CalendarClient::new(self.email.google_client(profile)?))
    }

    fn tasks(&self, profile: &str) -> anyhow::Result<TasksClient> {
        Ok(TasksClient::new(self.email.google_client(profile)?))
    }

    fn error(&self, profile: &str, error: impl std::fmt::Display) -> ToolResult {
        let message = error.to_string();
        note_reconnect_if_needed(&self.email, profile, &message);
        ToolResult::Err(message)
    }
}

fn string_arg(args: &serde_json::Value, name: &str, required: bool) -> Result<String, String> {
    match args.get(name).and_then(|value| value.as_str()) {
        Some(value) if value.len() <= 20_000 && (!required || !value.trim().is_empty()) => {
            Ok(value.trim().to_string())
        }
        Some(_) if required => Err(format!(
            "{name} must be a non-empty string no longer than 20,000 characters"
        )),
        Some(_) => Err(format!("{name} is too long")),
        None if required => Err(format!("missing required string {name}")),
        None => Ok(String::new()),
    }
}

fn list_limit(args: &serde_json::Value) -> u32 {
    args.get("max")
        .and_then(|value| value.as_u64())
        .map(|value| value.clamp(1, MAX_LIST as u64) as u32)
        .unwrap_or(25)
}

macro_rules! productivity_tool {
    ($name:ident, $tool_name:literal, $risk:expr, $description:literal, $schema:expr, $body:expr) => {
        pub struct $name {
            deps: ProductivityToolDeps,
        }
        impl $name {
            pub fn new(deps: ProductivityToolDeps) -> Self {
                Self { deps }
            }
        }
        impl Tool for $name {
            fn name(&self) -> &str {
                $tool_name
            }
            fn description(&self) -> &str {
                $description
            }
            fn risk(&self) -> RiskClass {
                $risk
            }
            fn requires(&self) -> &[Capability] {
                &[Capability::Calendar]
            }
            fn schema(&self) -> serde_json::Value {
                $schema
            }
            fn destination(&self, _args: &serde_json::Value) -> Option<String> {
                Some(GOOGLE_DESTINATION.to_string())
            }
            fn run<'a>(
                &'a self,
                input: ToolInput,
                ctx: &'a ExecCtx,
            ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
                Box::pin(async move { $body(&self.deps, input.args, ctx).await })
            }
        }
    };
}

async fn calendar_list(
    deps: &ProductivityToolDeps,
    args: serde_json::Value,
    ctx: &ExecCtx,
) -> ToolResult {
    let days = args
        .get("days")
        .and_then(|value| value.as_i64())
        .map(|value| value.clamp(1, 90))
        .unwrap_or(7);
    let client = match deps.calendar(&ctx.profile) {
        Ok(client) => client,
        Err(error) => return deps.error(&ctx.profile, error),
    };
    match client
        .list_upcoming(
            Utc::now(),
            Utc::now() + Duration::days(days),
            list_limit(&args),
        )
        .await
    {
        Ok(events) => ToolResult::Ok(json!({ "events": events, "count": events.len() })),
        Err(error) => deps.error(&ctx.profile, error),
    }
}

async fn calendar_create(
    deps: &ProductivityToolDeps,
    args: serde_json::Value,
    ctx: &ExecCtx,
) -> ToolResult {
    let (title, start, end) = match (
        string_arg(&args, "title", true),
        string_arg(&args, "start", true),
        string_arg(&args, "end", true),
    ) {
        (Ok(title), Ok(start), Ok(end)) => (title, start, end),
        (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
            return ToolResult::Err(error)
        }
    };
    let description = match string_arg(&args, "description", false) {
        Ok(value) => value,
        Err(error) => return ToolResult::Err(error),
    };
    let client = match deps.calendar(&ctx.profile) {
        Ok(client) => client,
        Err(error) => return deps.error(&ctx.profile, error),
    };
    match client.create(&title, &description, &start, &end).await {
        Ok(event) => ToolResult::Ok(json!({ "created": event })),
        Err(error) => deps.error(&ctx.profile, error),
    }
}

async fn calendar_delete(
    deps: &ProductivityToolDeps,
    args: serde_json::Value,
    ctx: &ExecCtx,
) -> ToolResult {
    let id = match string_arg(&args, "id", true) {
        Ok(value) => value,
        Err(error) => return ToolResult::Err(error),
    };
    let client = match deps.calendar(&ctx.profile) {
        Ok(client) => client,
        Err(error) => return deps.error(&ctx.profile, error),
    };
    match client.delete(&id).await {
        Ok(()) => ToolResult::Ok(json!({ "deleted": true, "id": id })),
        Err(error) => deps.error(&ctx.profile, error),
    }
}

async fn task_list(
    deps: &ProductivityToolDeps,
    args: serde_json::Value,
    ctx: &ExecCtx,
) -> ToolResult {
    let client = match deps.tasks(&ctx.profile) {
        Ok(client) => client,
        Err(error) => return deps.error(&ctx.profile, error),
    };
    match client.list(list_limit(&args)).await {
        Ok(tasks) => ToolResult::Ok(json!({ "tasks": tasks, "count": tasks.len() })),
        Err(error) => deps.error(&ctx.profile, error),
    }
}

async fn task_create(
    deps: &ProductivityToolDeps,
    args: serde_json::Value,
    ctx: &ExecCtx,
) -> ToolResult {
    let title = match string_arg(&args, "title", true) {
        Ok(value) => value,
        Err(error) => return ToolResult::Err(error),
    };
    let notes = match string_arg(&args, "notes", false) {
        Ok(value) => value,
        Err(error) => return ToolResult::Err(error),
    };
    let due = match string_arg(&args, "due", false) {
        Ok(value) if !value.is_empty() => Some(value),
        Ok(_) => None,
        Err(error) => return ToolResult::Err(error),
    };
    let client = match deps.tasks(&ctx.profile) {
        Ok(client) => client,
        Err(error) => return deps.error(&ctx.profile, error),
    };
    match client.create(&title, &notes, due.as_deref()).await {
        Ok(task) => ToolResult::Ok(json!({ "created": task })),
        Err(error) => deps.error(&ctx.profile, error),
    }
}

async fn task_complete(
    deps: &ProductivityToolDeps,
    args: serde_json::Value,
    ctx: &ExecCtx,
) -> ToolResult {
    let id = match string_arg(&args, "id", true) {
        Ok(value) => value,
        Err(error) => return ToolResult::Err(error),
    };
    let completed = args
        .get("completed")
        .and_then(|value| value.as_bool())
        .unwrap_or(true);
    let client = match deps.tasks(&ctx.profile) {
        Ok(client) => client,
        Err(error) => return deps.error(&ctx.profile, error),
    };
    match client.set_completed(&id, completed).await {
        Ok(task) => ToolResult::Ok(json!({ "task": task })),
        Err(error) => deps.error(&ctx.profile, error),
    }
}

async fn task_delete(
    deps: &ProductivityToolDeps,
    args: serde_json::Value,
    ctx: &ExecCtx,
) -> ToolResult {
    let id = match string_arg(&args, "id", true) {
        Ok(value) => value,
        Err(error) => return ToolResult::Err(error),
    };
    let client = match deps.tasks(&ctx.profile) {
        Ok(client) => client,
        Err(error) => return deps.error(&ctx.profile, error),
    };
    match client.delete(&id).await {
        Ok(()) => ToolResult::Ok(json!({ "deleted": true, "id": id })),
        Err(error) => deps.error(&ctx.profile, error),
    }
}

productivity_tool!(
    CalendarListTool,
    "calendar_list",
    RiskClass::External,
    "List upcoming events from the connected Google Calendar. Args: days (1-90, default 7), max (1-100).",
    json!({"type":"object","properties":{"days":{"type":"integer","minimum":1,"maximum":90},"max":{"type":"integer","minimum":1,"maximum":100}},"additionalProperties":false}),
    calendar_list
);
productivity_tool!(
    CalendarCreateTool,
    "calendar_create",
    RiskClass::Dangerous,
    "Create a Google Calendar event. Args: title, start and end RFC 3339 timestamps, optional description. Always asks for approval.",
    json!({"type":"object","properties":{"title":{"type":"string"},"start":{"type":"string","description":"RFC 3339"},"end":{"type":"string","description":"RFC 3339"},"description":{"type":"string"}},"required":["title","start","end"],"additionalProperties":false}),
    calendar_create
);
productivity_tool!(
    CalendarDeleteTool,
    "calendar_delete",
    RiskClass::Dangerous,
    "Delete a Google Calendar event by id. Always asks for approval.",
    json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}),
    calendar_delete
);
productivity_tool!(
    TaskListTool,
    "task_list",
    RiskClass::External,
    "List tasks from the connected Google Tasks default list. Args: max (1-100).",
    json!({"type":"object","properties":{"max":{"type":"integer","minimum":1,"maximum":100}},"additionalProperties":false}),
    task_list
);
productivity_tool!(
    TaskCreateTool,
    "task_create",
    RiskClass::Dangerous,
    "Create a Google Task. Args: title, optional notes and RFC 3339 due. Always asks for approval.",
    json!({"type":"object","properties":{"title":{"type":"string"},"notes":{"type":"string"},"due":{"type":"string","description":"RFC 3339"}},"required":["title"],"additionalProperties":false}),
    task_create
);
productivity_tool!(
    TaskCompleteTool,
    "task_complete",
    RiskClass::Dangerous,
    "Set a Google Task's completed state. Args: id, optional completed (default true). Always asks for approval.",
    json!({"type":"object","properties":{"id":{"type":"string"},"completed":{"type":"boolean"}},"required":["id"],"additionalProperties":false}),
    task_complete
);
productivity_tool!(
    TaskDeleteTool,
    "task_delete",
    RiskClass::Dangerous,
    "Delete a Google Task by id. Always asks for approval.",
    json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}),
    task_delete
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_bounds_are_enforced_before_any_google_call() {
        assert_eq!(list_limit(&json!({"max": 999})), MAX_LIST);
        assert!(string_arg(&json!({"title": "  "}), "title", true).is_err());
    }
}
