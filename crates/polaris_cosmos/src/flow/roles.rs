//! Role-book parsing shared by generic call staging and application loaders.
use super::{Role, RoleBook, io::*};
use std::{fs, path::Path};
impl Role {
    /// Exact role ceiling in millionths of a dollar.
    pub fn max_micros(&self) -> FlowResult<u64> {
        let text = match &self.max_usd {
            toml::Value::String(v) => v.clone(),
            v => v.to_string(),
        };
        parse_micros(&text)
    }
}
impl RoleBook {
    /// Reads every TOML role fragment in deterministic filename order.
    pub fn load(dir: &Path) -> FlowResult<Self> {
        let mut paths = fs::read_dir(dir)?
            .map(|e| e.map(|e| e.path()))
            .collect::<Result<Vec<_>, _>>()?;
        paths.sort();
        let mut book = Self::default();
        for path in paths {
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            let text = fs::read_to_string(&path)?;
            let text = text
                .lines()
                .map(|line| {
                    if let Some((key, value)) = line.split_once('=')
                        && key.trim() == "max_usd"
                        && !value.trim().starts_with('"')
                    {
                        return format!("{key}= \"{}\"", value.split('#').next().unwrap().trim());
                    }
                    line.to_string()
                })
                .collect::<Vec<_>>()
                .join("\n");
            let table: toml::Table =
                toml::from_str(&text).map_err(|e| FlowError(format!("{}: {e}", path.display())))?;
            if table.keys().any(|key| key != "roles") {
                return Err(FlowError(format!(
                    "{}: expected roles table",
                    path.display()
                )));
            }
            let roles = table
                .get("roles")
                .and_then(toml::Value::as_table)
                .ok_or_else(|| FlowError(format!("{}: missing roles table", path.display())))?;
            for (key, value) in roles {
                if book.roles.contains_key(key) {
                    return Err(FlowError(format!("duplicate role {key}")));
                }
                let role: Role = value
                    .clone()
                    .try_into()
                    .map_err(|e| FlowError(format!("role {key}: {e}")))?;
                role.max_micros()?;
                if role.max_turns == 0 || role.timeout_seconds == 0 {
                    return Err(FlowError(format!(
                        "role {key}: turns and timeout must be positive"
                    )));
                }
                if role.max_output_tokens == Some(0) {
                    return Err(FlowError(format!(
                        "role {key}: max_output_tokens must be positive when set"
                    )));
                }
                book.roles.insert(key.clone(), role);
            }
        }
        Ok(book)
    }
    /// Serializes the merged role book with the same role table shape.
    pub fn write(&self, path: &Path) -> FlowResult<()> {
        atomic_write(
            path,
            toml::to_string_pretty(self)
                .map_err(|e| FlowError(e.to_string()))?
                .as_bytes(),
        )
    }
}
