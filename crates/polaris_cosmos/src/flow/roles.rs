//! Role-book parsing shared by generic call staging and application loaders.
use super::{Micros, Role, RoleBook, io::*};
use std::{fs, path::Path};

impl Role {
    /// Exact role ceiling in millionths of a dollar.
    pub fn max_micros(&self) -> Micros {
        self.max_usd.micros()
    }
}
impl RoleBook {
    /// Reads every TOML role fragment in deterministic filename order.
    ///
    /// A role's `max_usd` may be a number or a string; strings keep amounts with
    /// many digits exact.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the directory or a fragment cannot be read,
    /// a fragment is malformed or repeats a role, or a role has no positive
    /// turn, timeout or output-token limit.
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
            let table: toml::Table = toml::from_str(&text)
                .map_err(|e| FlowError::invalid(format!("{}: {e}", path.display())))?;
            if table.keys().any(|key| key != "roles") {
                return Err(FlowError::invalid(format!(
                    "{}: expected roles table",
                    path.display()
                )));
            }
            let roles = table
                .get("roles")
                .and_then(toml::Value::as_table)
                .ok_or_else(|| {
                    FlowError::invalid(format!("{}: missing roles table", path.display()))
                })?;
            for (key, value) in roles {
                if book.roles.contains_key(key) {
                    return Err(FlowError::invalid(format!("duplicate role {key}")));
                }
                let role: Role = value
                    .clone()
                    .try_into()
                    .map_err(|e| FlowError::invalid(format!("role {key}: {e}")))?;
                if role.max_turns == 0 || role.timeout_seconds == 0 {
                    return Err(FlowError::invalid(format!(
                        "role {key}: turns and timeout must be positive"
                    )));
                }
                if role.max_output_tokens == Some(0) {
                    return Err(FlowError::invalid(format!(
                        "role {key}: max_output_tokens must be positive when set"
                    )));
                }
                book.roles.insert(key.clone(), role);
            }
        }
        Ok(book)
    }
    /// Serializes the merged role book with the same role table shape.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the book cannot be encoded or written.
    pub fn write(&self, path: &Path) -> FlowResult<()> {
        atomic_write(
            path,
            toml::to_string_pretty(self)
                .map_err(|e| FlowError::invalid(e.to_string()))?
                .as_bytes(),
        )
    }
}
