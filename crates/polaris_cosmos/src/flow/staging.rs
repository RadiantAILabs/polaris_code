//! Strict prompt interpolation and read-only input snapshots.
use super::{AgentCall, AgentLimits, AgentRequest, Micros, Role, RunHandle, ToolGrant, io::*};
use polaris::system::resource::GlobalResource;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
/// Global immutable filesystem and model settings for call staging.
#[derive(Clone)]
///
/// # Scope
/// Global because every role call uses the same immutable asset and task roots.
/// # Provided By
/// Workflow boot wiring through the documented constructor or initializer.
/// # Access
/// Systems read `Res<FlowSettings>`; mutable books serialize updates internally.
/// # Alternatives
/// Use a local resource when branches need independent state.
/// # Example
/// ```
/// use polaris::system::{system,param::Res};
/// use polaris_cosmos::flow::FlowSettings;
/// #[system]
/// async fn inspect(value:Res<FlowSettings>)->std::path::PathBuf { value.task_dir.clone() }
/// ```
pub struct FlowSettings {
    /// Asset root containing prompts and skills.
    pub assets: PathBuf,
    /// Frozen task directory exposed as a standard prompt variable.
    pub task_dir: PathBuf,
    /// Configured model identity for unmeasured accounting.
    pub model_id: String,
}
impl GlobalResource for FlowSettings {}
/// Renders required placeholders and rejects unused explicit variables.
/// Standard variables may be omitted by a prompt.
pub fn render(
    role: &str,
    text: &str,
    vars: &BTreeMap<String, String>,
    standard: &BTreeMap<String, String>,
) -> FlowResult<String> {
    let mut used = std::collections::BTreeSet::new();
    let output = interpolate(role, text, vars, standard, &mut used)?;
    check_unused(role, vars, &used)?;
    Ok(output)
}
fn check_unused(
    role: &str,
    vars: &BTreeMap<String, String>,
    used: &std::collections::BTreeSet<String>,
) -> FlowResult<()> {
    for name in vars.keys() {
        if !used.contains(name) {
            return Err(FlowError(format!("role {role}: unused variable {name}")));
        }
    }
    Ok(())
}
fn interpolate(
    role: &str,
    text: &str,
    vars: &BTreeMap<String, String>,
    standard: &BTreeMap<String, String>,
    used: &mut std::collections::BTreeSet<String>,
) -> FlowResult<String> {
    let mut output = String::new();
    let mut rest = text;
    while let Some((before, after)) = rest.split_once("{{") {
        output.push_str(before);
        let (name, tail) = after
            .split_once("}}")
            .ok_or_else(|| FlowError(format!("role {role}: unclosed placeholder")))?;
        let name = name.trim();
        let value = vars
            .get(name)
            .or_else(|| standard.get(name))
            .ok_or_else(|| FlowError(format!("role {role}: missing placeholder {name}")))?;
        used.insert(name.to_owned());
        output.push_str(value);
        rest = tail;
    }
    output.push_str(rest);
    Ok(output)
}
/// Copies a file or directory without following any symbolic links.
pub fn copy_snapshot(source: &Path, dest: &Path, read_only: bool) -> FlowResult<()> {
    let meta = fs::symlink_metadata(source)?;
    if meta.file_type().is_symlink() {
        return Err(FlowError(format!(
            "symlink input refused: {}",
            source.display()
        )));
    }
    if meta.is_dir() {
        fs::create_dir_all(dest)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_snapshot(&entry.path(), &dest.join(entry.file_name()), read_only)?;
        }
    } else if meta.is_file() {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, dest)?;
    } else {
        return Err(FlowError("special file refused during staging".into()));
    }
    let mut permissions = fs::metadata(dest)?.permissions();
    if read_only {
        permissions.set_readonly(true);
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(permissions.mode() | if meta.is_dir() { 0o700 } else { 0o600 });
        }
        #[cfg(not(unix))]
        permissions.set_readonly(false);
    }
    fs::set_permissions(dest, permissions)?;
    Ok(())
}
/// Stages one bounded request and persists exactly the prompts sent to the runner.
pub fn stage(
    call: &AgentCall,
    role: &Role,
    workspace: &Path,
    granted: Micros,
    handle: &RunHandle,
    settings: &FlowSettings,
) -> FlowResult<AgentRequest> {
    fs::create_dir_all(workspace.join("inputs"))?;
    fs::create_dir_all(workspace.join("outputs"))?;
    for (source, name) in &call.stage.copy_in {
        copy_snapshot(
            source,
            &workspace.join("inputs").join(relative(name)?),
            true,
        )?;
    }
    let standard = BTreeMap::from([
        (
            "TASK_DIR".into(),
            workspace.join("inputs/task").display().to_string(),
        ),
        ("WORKSPACE".into(), workspace.display().to_string()),
        ("STEP_KEY".into(), call.step.to_string()),
        ("MAX_TURNS".into(), role.max_turns.to_string()),
        ("RUN_ROOT".into(), handle.root.display().to_string()),
    ]);
    let text = fs::read_to_string(
        settings
            .assets
            .join("prompts")
            .join(relative(&role.prompt)?)
            .with_extension("md"),
    )?;
    let mut used = std::collections::BTreeSet::new();
    let system_prompt = interpolate(&call.role, &text, &call.stage.vars, &standard, &mut used)?;
    let mut staged_names = std::collections::BTreeSet::new();
    for skill in &role.skills {
        let rel = relative(skill)?;
        let name = rel
            .file_name()
            .ok_or_else(|| FlowError("skill needs a filename".into()))?;
        if !staged_names.insert(name.to_owned()) {
            return Err(FlowError(format!(
                "role {}: duplicate staged skill {}",
                call.role,
                name.to_string_lossy()
            )));
        }
        let destination = workspace.join("skills").join(name);
        copy_snapshot(
            &settings.assets.join("prompts").join(rel),
            &destination,
            false,
        )?;
        render_skills(
            &destination,
            &call.role,
            &call.stage.vars,
            &standard,
            &mut used,
        )?;
    }
    let turn_prompt = call.stage.turn_prompt.clone().unwrap_or_else(|| {
        "Do the task in your instructions. Write your results to outputs/ and then stop.".into()
    });
    let turn_prompt = interpolate(
        &call.role,
        &turn_prompt,
        &call.stage.vars,
        &standard,
        &mut used,
    )?;
    check_unused(&call.role, &call.stage.vars, &used)?;
    atomic_write(&workspace.join("role_prompt.md"), system_prompt.as_bytes())?;
    atomic_write(&workspace.join("task_prompt.md"), turn_prompt.as_bytes())?;
    Ok(AgentRequest {
        step: call.step.clone(),
        role: call.role.clone(),
        workspace: workspace.into(),
        system_prompt,
        turn_prompt,
        limits: AgentLimits {
            max_turns: role.max_turns,
            max_micros: granted,
            timeout: Duration::from_secs(role.timeout_seconds),
            max_output_tokens: role.max_output_tokens,
        },
        grants: ToolGrant {
            edit: !role.read_only,
            commands: role.shell.clone(),
        },
    })
}

fn render_skills(
    path: &Path,
    role: &str,
    vars: &BTreeMap<String, String>,
    standard: &BTreeMap<String, String>,
    used: &mut std::collections::BTreeSet<String>,
) -> FlowResult<()> {
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            render_skills(&entry?.path(), role, vars, standard, used)?;
        }
    } else if path.extension().is_some_and(|ext| ext == "md") {
        let text = fs::read_to_string(path)?;
        atomic_write(
            path,
            interpolate(role, &text, vars, standard, used)?.as_bytes(),
        )?;
    }
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)?;
    Ok(())
}
