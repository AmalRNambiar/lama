//! lama's settings file, `~/.config/lama/config.json`: the few choices you
//! make inside lama that it keeps — who conducts tasks, which theme. One flat
//! JSON object; a key lama doesn't know is left as it is.

use std::path::PathBuf;

/// `$XDG_CONFIG_HOME/lama/config.json`, else `~/.config/lama/config.json`.
pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("lama").join("config.json"))
}

fn read() -> serde_json::Map<String, serde_json::Value> {
    path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// The setting `key`, as a string.
pub fn get(key: &str) -> Option<String> {
    read().get(key)?.as_str().map(String::from)
}

/// Keep `value` as `key`, leaving the file's other settings as they are.
pub fn set(key: &str, value: &str) -> Result<(), String> {
    let path = path().ok_or("no home directory")?;
    let mut config = read();
    config.insert(key.into(), value.into());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?;
    std::fs::write(&path, text + "\n").map_err(|e| format!("can't write {}: {e}", path.display()))
}
