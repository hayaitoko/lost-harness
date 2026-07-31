//! Google Calendar v3 client for the profile's connected Google account.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::api_error::GoogleApi;
use super::google::{GoogleClient, Method};

const CALENDAR_API: &str = "https://www.googleapis.com/calendar/v3/calendars/primary/events";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    pub summary: String,
    pub description: String,
    /// RFC 3339 timestamp for timed events, ISO date for all-day events.
    pub start: String,
    pub end: String,
    pub all_day: bool,
}

pub struct CalendarClient {
    google: GoogleClient,
}

impl CalendarClient {
    pub fn new(google: GoogleClient) -> Self {
        Self { google }
    }

    pub async fn list_upcoming(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        max: u32,
    ) -> anyhow::Result<Vec<CalendarEvent>> {
        let mut url = url::Url::parse(CALENDAR_API).expect("static Calendar API URL");
        url.query_pairs_mut()
            .append_pair("timeMin", &from.to_rfc3339())
            .append_pair("timeMax", &to.to_rfc3339())
            .append_pair("singleEvents", "true")
            .append_pair("orderBy", "startTime")
            .append_pair("maxResults", &max.clamp(1, 100).to_string());
        let value = self
            .google
            .json(GoogleApi::Calendar, Method::Get, url.as_str(), None)
            .await?;
        let rows = value
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(rows.into_iter().filter_map(parse_event).collect())
    }

    pub async fn create(
        &self,
        summary: &str,
        description: &str,
        start: &str,
        end: &str,
    ) -> anyhow::Result<CalendarEvent> {
        let summary = summary.trim();
        if summary.is_empty() {
            anyhow::bail!("an event needs a title");
        }
        // Timed events are intentionally strict: the UI supplies RFC 3339, so
        // the timezone is explicit rather than silently assuming a locale.
        let start_time = DateTime::parse_from_rfc3339(start)
            .map_err(|_| anyhow::anyhow!("start must be an RFC 3339 timestamp"))?;
        let end_time = DateTime::parse_from_rfc3339(end)
            .map_err(|_| anyhow::anyhow!("end must be an RFC 3339 timestamp"))?;
        if end_time <= start_time {
            anyhow::bail!("event end must be after its start");
        }
        let body = serde_json::json!({
            "summary": summary,
            "description": description.trim(),
            "start": { "dateTime": start },
            "end": { "dateTime": end },
        });
        let value = self
            .google
            .json(GoogleApi::Calendar, Method::Post, CALENDAR_API, Some(&body))
            .await?;
        parse_event(value).ok_or_else(|| {
            anyhow::anyhow!("Google Calendar returned an event without an id or schedule")
        })
    }

    pub async fn delete(&self, id: &str) -> anyhow::Result<()> {
        let url = event_url(id)?;
        self.google
            .json(GoogleApi::Calendar, Method::Delete, url.as_str(), None)
            .await?;
        Ok(())
    }
}

fn event_url(id: &str) -> anyhow::Result<url::Url> {
    if id.trim().is_empty() || id.len() > 512 {
        anyhow::bail!("invalid calendar event id");
    }
    let mut url = url::Url::parse(CALENDAR_API).expect("static Calendar API URL");
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid Calendar API URL"))?
        .push(id);
    Ok(url)
}

fn date_field(value: &serde_json::Value) -> Option<(String, bool)> {
    let obj = value.as_object()?;
    obj.get("dateTime")
        .and_then(|v| v.as_str())
        .map(|v| (v.to_string(), false))
        .or_else(|| {
            obj.get("date")
                .and_then(|v| v.as_str())
                .map(|v| (v.to_string(), true))
        })
}

fn parse_event(value: serde_json::Value) -> Option<CalendarEvent> {
    let id = value.get("id")?.as_str()?.to_string();
    let (start, all_day) = date_field(value.get("start")?)?;
    let (end, end_all_day) = date_field(value.get("end")?)?;
    if all_day != end_all_day {
        return None;
    }
    Some(CalendarEvent {
        id,
        summary: value
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("(untitled event)")
            .to_string(),
        description: value
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        start,
        end,
        all_day,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_parser_keeps_timed_and_all_day_values_honest() {
        let timed = parse_event(serde_json::json!({
            "id": "abc", "summary": "Standup", "start": {"dateTime": "2026-07-25T09:00:00-07:00"},
            "end": {"dateTime": "2026-07-25T09:30:00-07:00"}
        }))
        .unwrap();
        assert!(!timed.all_day);
        let all_day = parse_event(serde_json::json!({
            "id": "def", "start": {"date": "2026-07-25"}, "end": {"date": "2026-07-26"}
        }))
        .unwrap();
        assert!(all_day.all_day);
        assert_eq!(all_day.summary, "(untitled event)");
    }

    #[test]
    fn event_url_escapes_an_id_as_one_path_segment() {
        let u = event_url("a/b?c").unwrap();
        assert!(u.as_str().contains("a%2Fb%3Fc"));
    }
}
