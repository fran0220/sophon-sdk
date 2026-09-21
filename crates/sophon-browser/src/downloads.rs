//! CDP download bookkeeping is independent of the lossy console/event log.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uuid::Uuid;

use crate::Error;

pub(crate) const MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_DOWNLOADS: usize = 32;
const MAX_ACTIVE: usize = 4;

struct Download {
    value: Value,
    started: Instant,
}

pub(crate) struct Downloads {
    pub directory: PathBuf,
    entries: BTreeMap<String, Download>,
}

impl Downloads {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            entries: BTreeMap::new(),
        }
    }

    /// Returns native commands, sent by the single CDP writer without replay.
    pub fn event(&mut self, event: &Value) -> Vec<Value> {
        let params = &event["params"];
        let Some(id) = params["guid"]
            .as_str()
            .filter(|id| Uuid::parse_str(id).is_ok())
        else {
            return vec![];
        };
        let mut commands = vec![];
        if event["method"] == "Browser.downloadWillBegin" {
            if self.entries.contains_key(id) {
                return commands;
            }
            let active = self
                .entries
                .values()
                .filter(|d| d.value["state"] == "inProgress")
                .count();
            if self.entries.len() == MAX_DOWNLOADS {
                return vec![
                    cancel(id),
                    json!({"method":"Browser.setDownloadBehavior","params":{"behavior":"deny","eventsEnabled":true}}),
                ];
            }
            let overflow = active >= MAX_ACTIVE;
            self.entries.insert(id.into(), Download {
                started: Instant::now(),
                value: json!({"download_id":id,"frame_id":params["frameId"],"url":params["url"],"suggested_filename":params["suggestedFilename"],"state":if overflow {"cancel_requested"} else {"inProgress"},"reason":if overflow {Some("concurrency_limit")} else {None},"received_bytes":0,"total_bytes":0}),
            });
            if overflow {
                commands.push(cancel(id));
            }
            if self.entries.len() == MAX_DOWNLOADS {
                commands.push(json!({"method":"Browser.setDownloadBehavior","params":{"behavior":"deny","eventsEnabled":true}}));
            }
        } else if event["method"] == "Browser.downloadProgress"
            && let Some(download) = self.entries.get_mut(id)
        {
            download.value["received_bytes"] = params["receivedBytes"].clone();
            download.value["total_bytes"] = params["totalBytes"].clone();
            let oversized = params["receivedBytes"].as_f64().unwrap_or(0.) > MAX_BYTES as f64
                || params["totalBytes"].as_f64().unwrap_or(0.) > MAX_BYTES as f64;
            if download.value["reason"].is_null() && oversized {
                download.value["reason"] = json!("size_limit");
                download.value["state"] = json!("cancel_requested");
                commands.push(cancel(id));
            }
            if params["state"] == "canceled" || params["state"] == "completed" {
                // A racing completion after cancellation must never publish bytes.
                download.value["state"] = if download.value["reason"].is_null() {
                    params["state"].clone()
                } else {
                    json!("canceled")
                };
            }
        }
        commands
    }

    pub fn expired(&mut self) -> Vec<Value> {
        self.entries
            .iter_mut()
            .filter_map(|(id, download)| {
                if download.value["state"] == "inProgress"
                    && download.started.elapsed() >= Duration::from_secs(60)
                {
                    download.value["state"] = json!("cancel_requested");
                    download.value["reason"] = json!("deadline");
                    Some(cancel(id))
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn list(&self) -> Value {
        json!({"downloads":self.entries.values().map(|d|d.value.clone()).collect::<Vec<_>>(),"capacity":MAX_DOWNLOADS,"max_bytes":MAX_BYTES})
    }

    pub fn get(&self, id: &str) -> Result<Value, Error> {
        self.entries
            .get(id)
            .map(|d| d.value.clone())
            .ok_or_else(|| Error::Invalid("unknown download ID".into()))
    }

    pub fn cancel(&mut self, id: &str) -> Result<bool, Error> {
        let download = self
            .entries
            .get_mut(id)
            .ok_or_else(|| Error::Invalid("unknown download ID".into()))?;
        if download.value["state"] != "inProgress" {
            return Ok(false);
        }
        download.value["state"] = json!("cancel_requested");
        download.value["reason"] = json!("user_cancelled");
        Ok(true)
    }
}

fn cancel(id: &str) -> Value {
    json!({"method":"Browser.cancelDownload","params":{"guid":id}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn begin(id: &str) -> Value {
        json!({"method":"Browser.downloadWillBegin","params":{"guid":id,"url":"http://fixture/download","frameId":"frame","suggestedFilename":"../../untrusted.exe"}})
    }

    #[test]
    fn limits_and_cancellation_win_over_racing_completion() {
        let mut downloads = Downloads::new(PathBuf::new());
        let id = Uuid::new_v4().to_string();
        assert!(downloads.event(&begin(&id)).is_empty());
        let progress = |bytes, state| json!({"method":"Browser.downloadProgress","params":{"guid":id,"receivedBytes":bytes,"totalBytes":bytes,"state":state}});
        assert!(
            downloads
                .event(&progress(MAX_BYTES, "inProgress"))
                .is_empty()
        );
        assert_eq!(
            downloads
                .event(&progress(MAX_BYTES + 1, "inProgress"))
                .len(),
            1
        );
        downloads.event(&progress(MAX_BYTES + 1, "completed"));
        assert_eq!(downloads.get(&id).unwrap()["state"], "canceled");
        assert_eq!(downloads.get(&id).unwrap()["reason"], "size_limit");
        let id = Uuid::new_v4().to_string();
        downloads.event(&begin(&id));
        downloads.entries.get_mut(&id).unwrap().started = Instant::now() - Duration::from_secs(61);
        assert_eq!(downloads.expired().len(), 1);
        assert!(downloads.expired().is_empty()); // No replay.
        for _ in 0..40 {
            downloads.event(&begin(&Uuid::new_v4().to_string()));
        }
        assert_eq!(downloads.entries.len(), MAX_DOWNLOADS);
        assert!(
            downloads
                .entries
                .values()
                .filter(|d| d.value["state"] == "inProgress")
                .count()
                <= MAX_ACTIVE
        );
    }
}
