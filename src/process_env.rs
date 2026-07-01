use std::collections::BTreeMap;

pub fn apply_env_overlay(
    command: &mut tokio::process::Command,
    env_overlay: &BTreeMap<String, Option<String>>,
) {
    for (key, value) in env_overlay {
        if let Some(value) = value {
            command.env(key, value);
        } else {
            command.env_remove(key);
        }
    }
}
