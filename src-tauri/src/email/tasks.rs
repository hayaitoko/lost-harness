//! Google Tasks v1 client for the profile's connected Google account.

use serde::{Deserialize, Serialize};

use super::api_error::GoogleApi;
use super::google::{GoogleClient, Method};

const TASKS_API: &str = "https://tasks.googleapis.com/tasks/v1/lists/@default/tasks";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub notes: String,
    pub due: Option<String>,
    pub completed: bool,
}

pub struct TasksClient {
    google: GoogleClient,
}

impl TasksClient {
    pub fn new(google: GoogleClient) -> Self {
        Self { google }
    }

    pub async fn list(&self, max: u32) -> anyhow::Result<Vec<Task>> {
        let mut url = url::Url::parse(TASKS_API).expect("static Tasks API URL");
        url.query_pairs_mut()
            .append_pair("maxResults", &max.clamp(1, 100).to_string())
            .append_pair("showCompleted", "true")
            .append_pair("showHidden", "false");
        let value = self
            .google
            .json(GoogleApi::Tasks, Method::Get, url.as_str(), None)
            .await?;
        let items = value
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(items.into_iter().filter_map(parse_task).collect())
    }

    pub async fn create(
        &self,
        title: &str,
        notes: &str,
        due: Option<&str>,
    ) -> anyhow::Result<Task> {
        let title = title.trim();
        if title.is_empty() {
            anyhow::bail!("a task needs a title");
        }
        let due = due.map(str::trim).filter(|v| !v.is_empty());
        if let Some(value) = due {
            chrono::DateTime::parse_from_rfc3339(value)
                .map_err(|_| anyhow::anyhow!("a task due time must be an RFC 3339 timestamp"))?;
        }
        let body = serde_json::json!({ "title": title, "notes": notes.trim(), "due": due });
        let value = self
            .google
            .json(GoogleApi::Tasks, Method::Post, TASKS_API, Some(&body))
            .await?;
        parse_task(value)
            .ok_or_else(|| anyhow::anyhow!("Google Tasks returned a task without an id"))
    }

    pub async fn set_completed(&self, id: &str, completed: bool) -> anyhow::Result<Task> {
        let url = task_url(id)?;
        let body =
            serde_json::json!({ "status": if completed { "completed" } else { "needsAction" } });
        let value = self
            .google
            .json(GoogleApi::Tasks, Method::Patch, url.as_str(), Some(&body))
            .await?;
        parse_task(value)
            .ok_or_else(|| anyhow::anyhow!("Google Tasks returned a task without an id"))
    }

    pub async fn delete(&self, id: &str) -> anyhow::Result<()> {
        let url = task_url(id)?;
        self.google
            .json(GoogleApi::Tasks, Method::Delete, url.as_str(), None)
            .await?;
        Ok(())
    }
}

fn task_url(id: &str) -> anyhow::Result<url::Url> {
    if id.trim().is_empty() || id.len() > 512 {
        anyhow::bail!("invalid task id");
    }
    let mut url = url::Url::parse(TASKS_API).expect("static Tasks API URL");
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid Tasks API URL"))?
        .push(id);
    Ok(url)
}

fn parse_task(value: serde_json::Value) -> Option<Task> {
    Some(Task {
        id: value.get("id")?.as_str()?.to_string(),
        title: value
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("(untitled task)")
            .to_string(),
        notes: value
            .get("notes")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        due: value
            .get("due")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        completed: value.get("status").and_then(|v| v.as_str()) == Some("completed"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_parser_does_not_fabricate_optional_fields() {
        let task =
            parse_task(serde_json::json!({"id": "x", "title": "Buy tea", "status": "needsAction"}))
                .unwrap();
        assert_eq!(task.notes, "");
        assert_eq!(task.due, None);
        assert!(!task.completed);
    }

    #[test]
    fn task_url_escapes_an_id_as_one_path_segment() {
        assert!(task_url("a/b?c").unwrap().as_str().contains("a%2Fb%3Fc"));
    }
}
