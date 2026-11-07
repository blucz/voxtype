//! Pin streaming delivery to the window focused at Stop, not at Start.
//! A failed focus check withholds keystrokes; the clipboard/journal remain
//! available for recovery. No window titles or field contents are stored.
use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::process::Command;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Target {
    address: String,
    pid: u64,
}

fn target(value: &serde_json::Value) -> Option<Target> {
    let address = value.get("address")?.as_str()?;
    let pid = value.get("pid")?.as_u64()?;
    if address.is_empty() || address == "0x0" || pid == 0 {
        return None;
    }
    Some(Target {
        address: address.to_owned(),
        pid,
    })
}

async fn focused() -> Option<Target> {
    let mut cmd = Command::new("hyprctl");
    cmd.args(["-j", "activewindow"]).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_millis(200), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    target(&serde_json::from_slice::<serde_json::Value>(&out.stdout).ok()?)
}

fn path() -> std::path::PathBuf {
    Config::runtime_dir().join("stop-target.json")
}

pub fn clear() {
    let _ = std::fs::remove_file(path());
}

pub async fn capture() {
    clear();
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return;
    }
    // null means we could not identify a safe target: fail closed.
    if let Ok(bytes) = serde_json::to_vec(&focused().await) {
        let _ = std::fs::write(path(), bytes);
    }
}

pub async fn permits_delivery() -> bool {
    let bytes = match std::fs::read(path()) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false,
    };
    let expected = serde_json::from_slice::<Option<Target>>(&bytes)
        .ok()
        .flatten();
    let actual = focused().await;
    matches!((expected, actual), (Some(a), Some(b)) if a == b)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn target_rejects_desktop_and_distinguishes_windows_in_one_app() {
        assert!(target(&serde_json::json!({})).is_none());
        assert!(target(&serde_json::json!({"address":"0x0","pid":10})).is_none());
        let a = target(&serde_json::json!({"address":"0x1","pid":10})).unwrap();
        let b = target(&serde_json::json!({"address":"0x2","pid":10})).unwrap();
        assert_ne!(a, b);
    }
}
