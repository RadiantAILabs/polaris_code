//! The `view_image` tool: let the model look at an image file in the working
//! directory.
//!
//! The file tools read UTF-8 text, so a task that ships a screenshot, a diagram
//! or a chart gives the model no way to see it. [`ViewImageTools`] adds one
//! read-only tool, `view_image`, that opens a PNG, JPEG, GIF or WebP file and
//! hands it back as a picture. It is a plugin of its own
//! ([`ViewImageToolsPlugin`]) because whether it exists depends on whether the
//! model takes images: a text-only model rejects a request that carries one, so
//! the host registers the plugin only when
//! [`ImageInput`](crate::images::ImageInput) says so, and a model that cannot
//! use the tool is never offered it.
//!
//! # What the tool checks, in order
//!
//! 1. **Containment.** The path resolves through the same code as `read_file`
//!    (`CodeTools::resolve_call`), against the session's
//!    [`CallRoot`]: `..`, absolute paths and symlinks that leave the working
//!    directory are refused with `escapes sandbox`.
//! 2. **Kind.** The path must be a regular file. A directory, a pipe or a
//!    device is refused before it is opened.
//! 3. **Size.** [`IMAGE_BYTES_MAX`] on the file, checked before reading, and
//!    the read itself is capped one byte past it, so a file that grows between
//!    the check and the read is still refused rather than loaded.
//! 4. **Content.** The type comes from the file's magic bytes, never its name:
//!    `notes.png` that is really text is refused, and a screenshot saved as
//!    `.txt` is shown.
//!
//! Every refusal is an error whose text says what to do next.
//!
//! # How the picture reaches the model
//!
//! A toolset answers with JSON, and the agent's dispatch turns that into text.
//! `view_image` answers with a [`ViewedImage`] instead, and the dispatch turns
//! exactly that into an image block ([`ViewedImage::from_result`]). The dispatch
//! requires **both** the tool's name and the payload's shape, so no other tool,
//! an MCP server included, can make the model receive an image by returning JSON
//! that looks like one. Everything else keeps the text path and its secret
//! redaction.
//!
//! # Not covered
//!
//! Secret redaction scans text. A picture is not scanned, so a screenshot of a
//! secret reaches the provider. The tool is read-only and asks nothing, like
//! `read_file`, and has the same reach: any image file inside the working
//! directory.

use std::path::PathBuf;

use polaris::models::llm::{DocumentSource, ImageBlock};
use polaris::system::plugin::{
    Contract, DefaultDependencies, Plugin, PluginAccess, PluginId, Version, VersionReq,
};
use polaris::system::server::Server;
use polaris::tools::{ToolError, ToolPermission, ToolRegistry, ToolsPlugin, toolset};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncReadExt as _;

use super::code::{CallRoot, CodeToolsConfig, io_failure};
use super::policy::{ToolEffect, ToolPolicy, ToolPolicyPlugin, ToolPolicyRegistry};
use crate::images::{IMAGE_BYTES_MAX, base64, fmt_bytes, format_name, media_type_of, sniff_image};

/// The tool's name, as the model calls it and as the dispatch recognises it.
pub const VIEW_IMAGE: &str = "view_image";

/// The longest base64 text a provider accepts for one image: 5 MB.
///
/// [`IMAGE_BYTES_MAX`] is three quarters of this, which base64 grows back to it.
const ENCODED_BYTES_MAX: usize = 5_000_000;

/// The tag that makes a JSON value a [`ViewedImage`].
///
/// A one-variant enum rather than a string field, so a value whose tag is
/// anything else fails to deserialize and is never read as an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Kind {
    #[serde(rename = "polaris_code.viewed_image")]
    ViewedImage,
}

/// What `view_image` returns: one image, encoded, with what is needed to show
/// a short description of it.
///
/// This is the typed payload the agent's dispatch looks for. It carries the
/// whole picture as base64, so nothing but the dispatch may turn it into text:
/// [`summary`](Self::summary) is the line the transcript, the log and the
/// trajectory show instead.
///
/// # Examples
///
/// ```
/// use polaris_code::tools::{ViewedImage, VIEW_IMAGE};
///
/// let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0";
/// let image = ViewedImage::new("shots/a.png", "image/png", png);
/// assert_eq!(image.summary(), "[image shots/a.png, PNG, 29 B]");
///
/// // The dispatch accepts it from `view_image`, and from no other tool.
/// let value = serde_json::to_value(&image).unwrap();
/// assert!(ViewedImage::from_result(VIEW_IMAGE, &value).is_some());
/// assert!(ViewedImage::from_result("mcp__server__shot", &value).is_none());
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewedImage {
    kind: Kind,
    path: String,
    media_type: String,
    bytes: u64,
    data: String,
}

impl ViewedImage {
    /// An image read from `path`, with its MIME type and raw bytes.
    #[must_use]
    pub fn new(path: impl Into<String>, media_type: &str, data: &[u8]) -> Self {
        Self {
            kind: Kind::ViewedImage,
            path: path.into(),
            media_type: media_type.to_owned(),
            bytes: data.len() as u64,
            data: base64(data),
        }
    }

    /// The image `tool` returned in `value`, or `None` when it did not return
    /// one.
    ///
    /// `Some` only when `tool` is [`VIEW_IMAGE`], the value has exactly this
    /// payload's shape and tag, and its type and size are ones the tool
    /// produces. Name and shape are both required, so a different tool returning
    /// the same JSON is not believed, and `view_image` returning anything else
    /// is an ordinary text result.
    #[must_use]
    pub fn from_result(tool: &str, value: &Value) -> Option<Self> {
        if tool != VIEW_IMAGE {
            return None;
        }
        let image = Self::deserialize(value).ok()?;
        (media_type_of(&image.media_type).is_some()
            && image.bytes <= IMAGE_BYTES_MAX
            && image.data.len() <= ENCODED_BYTES_MAX)
            .then_some(image)
    }

    /// The model-facing image block.
    ///
    /// `None` only when the payload names a type this host does not send, which
    /// [`from_result`](Self::from_result) already refuses.
    #[must_use]
    pub fn into_block(self) -> Option<ImageBlock> {
        Some(ImageBlock {
            media_type: media_type_of(&self.media_type)?,
            data: DocumentSource::Base64(self.data),
            additional_params: None,
        })
    }

    /// The one line shown wherever the result is displayed or recorded:
    /// `[image a.png, PNG, 34.0 KB]`. Never the encoded picture.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "[image {}, {}, {}]",
            self.path,
            format_name(&self.media_type),
            fmt_bytes(self.bytes)
        )
    }
}

/// The `view_image` toolset.
pub struct ViewImageTools {
    config: CodeToolsConfig,
}

impl ViewImageTools {
    /// A toolset that reads from the directory `config` names, contained the
    /// way [`CodeTools`](super::CodeTools) is.
    #[must_use]
    pub fn new(config: CodeToolsConfig) -> Self {
        Self { config }
    }

    /// The path the tool will open, or the refusal that explains why not.
    fn resolve(&self, call: &Option<CallRoot>, path: &str) -> Result<PathBuf, ToolError> {
        self.config.resolve_call(call, path)
    }
}

#[toolset]
impl ViewImageTools {
    /// Look at an image file (PNG, JPEG, GIF or WebP) in the working directory.
    ///
    /// The picture is shown to you directly, so use it for the screenshots,
    /// diagrams, charts and photos a task ships as files. The type is read from
    /// the file's contents, not its name. A file larger than 3.8 MB is refused:
    /// make a smaller copy and view that. For a text file use `read_file`.
    #[tool]
    async fn view_image(
        &self,
        /// Image path relative to the working directory.
        path: String,
        #[context] call_root: Option<CallRoot>,
    ) -> Result<ViewedImage, ToolError> {
        let resolved = self.resolve(&call_root, &path)?;
        // Look before opening: a pipe or device would block the open, and a
        // directory has no bytes to show.
        let metadata = tokio::fs::metadata(&resolved)
            .await
            .map_err(|err| io_failure(VIEW_IMAGE, &path, &err))?;
        if metadata.is_dir() {
            return Err(refusal(
                &path,
                "is a directory, not an image file. Call list_files to see what it holds.",
            ));
        }
        if !metadata.is_file() {
            return Err(refusal(
                &path,
                "is not a regular file, so it cannot be read.",
            ));
        }
        if metadata.len() > IMAGE_BYTES_MAX {
            return Err(too_large(&path, metadata.len()));
        }
        let file = tokio::fs::File::open(&resolved)
            .await
            .map_err(|err| io_failure(VIEW_IMAGE, &path, &err))?;
        // One byte past the cap: a file that grew since the check above is
        // refused instead of read to the end.
        let mut data = Vec::new();
        file.take(IMAGE_BYTES_MAX + 1)
            .read_to_end(&mut data)
            .await
            .map_err(|err| io_failure(VIEW_IMAGE, &path, &err))?;
        if data.len() as u64 > IMAGE_BYTES_MAX {
            return Err(too_large(&path, data.len() as u64));
        }
        let Some((_, mime)) = sniff_image(&data) else {
            return Err(refusal(
                &path,
                "is not a PNG, JPEG, GIF or WebP image; the file's contents were \
                 checked, not its name. For a text file call read_file.",
            ));
        };
        Ok(ViewedImage::new(path, mime, &data))
    }
}

/// A refusal of `path`, phrased `view_image 'path' <reason>`.
fn refusal(path: &str, reason: &str) -> ToolError {
    ToolError::execution_error(format!("{VIEW_IMAGE} '{path}' {reason}"))
}

/// The refusal for a file over [`IMAGE_BYTES_MAX`], quoting both numbers.
fn too_large(path: &str, bytes: u64) -> ToolError {
    refusal(
        path,
        &format!(
            "is {}; images are capped at {}. Make a smaller copy (for example by \
             resizing or compressing it with a shell command) and call view_image \
             on that, or tell the user the image is too large to look at.",
            fmt_bytes(bytes),
            fmt_bytes(IMAGE_BYTES_MAX)
        ),
    )
}

/// Plugin that registers [`ViewImageTools`] with the [`ToolRegistry`].
///
/// Add it only for a model that takes images; the composition root decides
/// from [`ImageInput`](crate::images::ImageInput). An assembly that omits it
/// loses nothing else: `read_file` still answers an image file, saying that
/// the model takes text only.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_ | — | Registers a tool on the existing [`ToolRegistry`]; provides no resource of its own. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | Registers a tool on the existing [`ToolRegistry`]; provides no API of its own. |
///
/// # Dependencies
///
/// - [`ToolsPlugin`] — owns the [`ToolRegistry`] this plugin populates.
/// - [`ToolPolicyPlugin`] — owns the [`ToolPolicyRegistry`] this plugin declares
///   into; added automatically when missing.
///
/// # Tools Provided
///
/// | Tool | Permission | Description |
/// |------|------------|-------------|
/// | `view_image` | [`ToolPermission::Allow`] | Opens a PNG, JPEG, GIF or WebP file inside the working directory and returns it as an image; refuses anything else, and anything over 3.75 MB, with text saying what to do next. |
///
/// # Permissions
///
/// `view_image` only reads, like `read_file`: it registers as
/// [`ToolPermission::Allow`] and declares [`ToolEffect::ReadOnly`], so it runs
/// without asking in every mode, `plan` and unattended runs included. It
/// resolves paths through the same sandbox as the file tools, and in the
/// explicitly unrestricted container mode shares their wider reach.
///
/// # Lifecycle
///
/// - **`build()`** — registers the toolset, its permission and its policy
///   fact. No `ready()` override, no tick schedules and no `cleanup()`:
///   nothing outlives a call.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris::tools::ToolsPlugin;
/// use polaris_code::tools::{CodeToolsConfig, ViewImageToolsPlugin};
///
/// let mut server = Server::new();
/// server
///     .add_plugins(ToolsPlugin)
///     .add_plugins(ViewImageToolsPlugin::new(CodeToolsConfig::new(".")));
/// ```
pub struct ViewImageToolsPlugin {
    config: CodeToolsConfig,
}

impl ViewImageToolsPlugin {
    /// A plugin whose tool reads from `config.working_dir`, contained unless
    /// `config.unrestricted_paths` is set. Pass the same configuration as
    /// [`CodeToolsPlugin`](super::CodeToolsPlugin) so both tools see one
    /// sandbox.
    #[must_use]
    pub fn new(config: CodeToolsConfig) -> Self {
        Self { config }
    }
}

impl Plugin for ViewImageToolsPlugin {
    const ID: &'static str = "polaris_code::view_image_tools";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![
            PluginId::of::<ToolsPlugin>(),
            PluginId::of::<ToolPolicyPlugin>(),
        ]
    }

    fn default_dependencies(&self) -> DefaultDependencies {
        DefaultDependencies::new().add::<ToolPolicyPlugin>()
    }

    fn access(&self) -> PluginAccess {
        PluginAccess::new()
            .extends::<ToolRegistry>(VersionReq::caret(ToolRegistry::CONTRACT_VERSION))
            .extends::<ToolPolicyRegistry>(VersionReq::caret(ToolPolicyRegistry::CONTRACT_VERSION))
    }

    fn build(&self, server: &mut Server) {
        let mut registry = server
            .get_resource_mut::<ToolRegistry>()
            .expect("ToolsPlugin must be added before ViewImageToolsPlugin");
        registry.register_toolset(ViewImageTools::new(self.config.clone()));
        registry
            .set_permission(VIEW_IMAGE, ToolPermission::Allow)
            .expect("ViewImageTools registers view_image");
        drop(registry);
        server
            .get_resource_mut::<ToolPolicyRegistry>()
            .expect("ToolPolicyPlugin must be added before ViewImageToolsPlugin")
            .declare(
                VIEW_IMAGE,
                ToolPolicy::default().with_effect(ToolEffect::ReadOnly),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_1PX: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00,
    ];

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("shots")).unwrap();
        std::fs::write(dir.path().join("shots/a.png"), PNG_1PX).unwrap();
        // The name lies in both directions.
        std::fs::write(dir.path().join("fake.png"), b"just some notes\n").unwrap();
        std::fs::write(dir.path().join("really-a-png.txt"), PNG_1PX).unwrap();
        std::fs::write(dir.path().join("empty.png"), b"").unwrap();
        dir
    }

    fn tools(dir: &tempfile::TempDir) -> ViewImageTools {
        ViewImageTools::new(CodeToolsConfig::new(dir.path()))
    }

    fn root(dir: &tempfile::TempDir) -> Option<CallRoot> {
        Some(CallRoot::new(dir.path()))
    }

    async fn view(dir: &tempfile::TempDir, path: &str) -> Result<ViewedImage, String> {
        tools(dir)
            .view_image(path.to_string(), root(dir))
            .await
            .map_err(|err| err.to_string())
    }

    #[tokio::test]
    async fn an_image_inside_the_working_directory_comes_back_as_an_image() {
        let dir = workspace();
        let image = view(&dir, "shots/a.png").await.unwrap();
        let value = serde_json::to_value(&image).unwrap();
        let seen = ViewedImage::from_result(VIEW_IMAGE, &value).expect("the dispatch accepts it");
        assert_eq!(seen.summary(), "[image shots/a.png, PNG, 29 B]");
        let block = seen.into_block().expect("a sent type");
        assert!(matches!(
            block.media_type,
            polaris::models::llm::ImageMediaType::PNG
        ));
        let DocumentSource::Base64(encoded) = block.data;
        assert_eq!(encoded, base64(PNG_1PX), "the bytes are the file's, whole");
    }

    #[tokio::test]
    async fn the_type_comes_from_the_contents_not_the_name() {
        let dir = workspace();
        let shown = view(&dir, "really-a-png.txt").await.unwrap();
        assert_eq!(shown.summary(), "[image really-a-png.txt, PNG, 29 B]");

        for path in ["fake.png", "empty.png"] {
            let refused = view(&dir, path).await.unwrap_err();
            assert!(
                refused.contains("is not a PNG, JPEG, GIF or WebP image"),
                "{path}: {refused}"
            );
            assert!(
                refused.contains("read_file"),
                "it names the way on: {refused}"
            );
        }
    }

    #[tokio::test]
    async fn every_supported_type_is_shown() {
        let dir = tempfile::tempdir().unwrap();
        for (name, head, label) in [
            ("a.jpg", &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0][..], "JPEG"),
            ("a.gif", &b"GIF89a\x01\0\x01\0"[..], "GIF"),
            ("a.webp", &b"RIFF\x04\0\0\0WEBPVP8 "[..], "WEBP"),
            ("a.png", PNG_1PX, "PNG"),
        ] {
            std::fs::write(dir.path().join(name), head).unwrap();
            let image = view(&dir, name).await.unwrap();
            assert!(
                image.summary().contains(label),
                "{name}: {}",
                image.summary()
            );
        }
    }

    #[tokio::test]
    async fn a_path_that_leaves_the_working_directory_is_refused() {
        let outer = tempfile::tempdir().unwrap();
        let inside = outer.path().join("work");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::write(outer.path().join("secret.png"), PNG_1PX).unwrap();
        let tools = ViewImageTools::new(CodeToolsConfig::new(&inside));
        let call = Some(CallRoot::new(&inside));

        for path in [
            "../secret.png".to_string(),
            outer.path().join("secret.png").display().to_string(),
        ] {
            let refused = tools
                .view_image(path.clone(), call.clone())
                .await
                .unwrap_err()
                .to_string();
            assert!(refused.contains("escapes sandbox"), "{path}: {refused}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_out_of_the_working_directory_is_refused() {
        let outer = tempfile::tempdir().unwrap();
        let inside = outer.path().join("work");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::write(outer.path().join("secret.png"), PNG_1PX).unwrap();
        std::os::unix::fs::symlink(outer.path().join("secret.png"), inside.join("link.png"))
            .unwrap();
        std::os::unix::fs::symlink(outer.path(), inside.join("dir")).unwrap();
        let tools = ViewImageTools::new(CodeToolsConfig::new(&inside));
        let call = Some(CallRoot::new(&inside));

        for path in ["link.png", "dir/secret.png"] {
            let refused = tools
                .view_image(path.to_string(), call.clone())
                .await
                .unwrap_err()
                .to_string();
            assert!(refused.contains("escapes sandbox"), "{path}: {refused}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_explicit_unrestricted_mode_shares_the_file_tools_wider_reach() {
        let outer = tempfile::tempdir().unwrap();
        let inside = outer.path().join("work");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::write(outer.path().join("shot.png"), PNG_1PX).unwrap();
        let tools =
            ViewImageTools::new(CodeToolsConfig::new(&inside).with_unrestricted_paths(true));
        let image = tools
            .view_image("../shot.png".into(), Some(CallRoot::new(&inside)))
            .await
            .unwrap();
        assert_eq!(image.summary(), "[image ../shot.png, PNG, 29 B]");
    }

    #[tokio::test]
    async fn the_call_root_decides_where_a_relative_path_starts() {
        let one = workspace();
        let other = tempfile::tempdir().unwrap();
        // The toolset was built on `other`, the call carries `one`'s root.
        let tools = ViewImageTools::new(CodeToolsConfig::new(other.path()));
        let image = tools
            .view_image("shots/a.png".into(), root(&one))
            .await
            .unwrap();
        assert!(image.summary().starts_with("[image shots/a.png"));
    }

    #[tokio::test]
    async fn an_oversized_image_is_refused_with_both_numbers_and_the_next_step() {
        let dir = workspace();
        let mut big = PNG_1PX.to_vec();
        big.resize(IMAGE_BYTES_MAX as usize + 1, 0);
        std::fs::write(dir.path().join("huge.png"), &big).unwrap();

        let refused = view(&dir, "huge.png").await.unwrap_err();
        assert!(
            refused.contains("is 3.8 MB; images are capped at 3.8 MB"),
            "{refused}"
        );
        assert!(
            refused.contains("smaller copy"),
            "it says what to do: {refused}"
        );

        // One byte under the cap is fine.
        let mut limit = PNG_1PX.to_vec();
        limit.resize(IMAGE_BYTES_MAX as usize, 0);
        std::fs::write(dir.path().join("limit.png"), &limit).unwrap();
        assert!(view(&dir, "limit.png").await.is_ok());
    }

    #[tokio::test]
    async fn a_directory_and_a_missing_file_are_refused_with_the_path() {
        let dir = workspace();
        let refused = view(&dir, "shots").await.unwrap_err();
        assert!(refused.contains("is a directory"), "{refused}");
        assert!(refused.contains("list_files"), "{refused}");

        let refused = view(&dir, "nothing-here.png").await.unwrap_err();
        assert!(refused.contains("nothing-here.png"), "{refused}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_pipe_is_refused_without_being_opened() {
        let dir = workspace();
        let fifo = dir.path().join("pipe.png");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !made.is_ok_and(|status| status.success()) {
            return; // no mkfifo here; the check is the same one a directory takes
        }
        let refused =
            tokio::time::timeout(std::time::Duration::from_secs(5), view(&dir, "pipe.png"))
                .await
                .expect("a pipe must not block the tool")
                .unwrap_err();
        assert!(refused.contains("not a regular file"), "{refused}");
    }

    #[test]
    fn only_view_image_with_its_own_payload_is_believed() {
        let image = ViewedImage::new("a.png", "image/png", PNG_1PX);
        let value = serde_json::to_value(&image).unwrap();

        assert!(ViewedImage::from_result(VIEW_IMAGE, &value).is_some());
        // The same payload from any other name, however plausible.
        for tool in [
            "read_file",
            "web_fetch",
            "mcp__server__view_image",
            "view_image ",
            "View_Image",
            "",
        ] {
            assert!(ViewedImage::from_result(tool, &value).is_none(), "{tool:?}");
        }
        // The right name with the wrong shape.
        for forged in [
            serde_json::json!("[image a.png]"),
            serde_json::json!({"path": "a.png", "data": "AAAA"}),
            serde_json::json!({"kind": "image", "path": "a.png", "media_type": "image/png",
                               "bytes": 1, "data": "AAAA"}),
            serde_json::json!({"kind": "polaris_code.viewed_image", "path": "a.png",
                               "media_type": "image/svg+xml", "bytes": 1, "data": "AAAA"}),
            serde_json::json!({"kind": "polaris_code.viewed_image", "path": "a.png",
                               "media_type": "image/png", "bytes": IMAGE_BYTES_MAX + 1,
                               "data": "AAAA"}),
            serde_json::json!({"kind": "polaris_code.viewed_image", "path": "a.png",
                               "media_type": "image/png", "bytes": 1, "data": "AAAA",
                               "extra": true}),
        ] {
            assert!(
                ViewedImage::from_result(VIEW_IMAGE, &forged).is_none(),
                "{forged}"
            );
        }
        // An encoded body past the provider's limit is not sent.
        let mut oversized = value.clone();
        oversized["data"] = Value::String("A".repeat(ENCODED_BYTES_MAX + 1));
        assert!(ViewedImage::from_result(VIEW_IMAGE, &oversized).is_none());
    }

    #[test]
    fn the_summary_never_carries_the_picture() {
        let image = ViewedImage::new("a.png", "image/png", &[0x41; 34_000]);
        let summary = image.summary();
        assert_eq!(summary, "[image a.png, PNG, 34.0 KB]");
        assert!(summary.len() < 40);
    }

    #[tokio::test]
    async fn the_plugin_registers_a_read_only_tool_that_asks_nothing() {
        let mut server = Server::new();
        server
            .add_plugins(ToolsPlugin)
            .add_plugins(ViewImageToolsPlugin::new(CodeToolsConfig::new(
                std::env::temp_dir(),
            )));
        server.finish().await.unwrap();

        let ctx = server.create_context();
        let registry = ctx.get_resource::<ToolRegistry>().unwrap();
        assert!(registry.has(VIEW_IMAGE));
        assert_eq!(registry.permission(VIEW_IMAGE), Some(ToolPermission::Allow));
        let policy = server.get_global::<ToolPolicyRegistry>().unwrap();
        assert_eq!(policy.get(VIEW_IMAGE).effect(), Some(ToolEffect::ReadOnly));
        assert_eq!(
            policy.get(VIEW_IMAGE).attended(),
            super::super::policy::AttendedApproval::ByPolicy
        );
    }

    #[tokio::test]
    async fn the_registry_answers_with_the_typed_payload() {
        let dir = workspace();
        let mut server = Server::new();
        server
            .add_plugins(ToolsPlugin)
            .add_plugins(ViewImageToolsPlugin::new(CodeToolsConfig::new(dir.path())));
        server.finish().await.unwrap();

        let ctx = server.create_context();
        let registry = ctx.get_resource::<ToolRegistry>().unwrap();
        let value = registry
            .execute_with(
                VIEW_IMAGE,
                &serde_json::json!({"path": "shots/a.png"}),
                &polaris::tools::ToolContext::new().with(CallRoot::new(dir.path())),
            )
            .await
            .unwrap();
        let image = ViewedImage::from_result(VIEW_IMAGE, &value).expect("typed payload");
        assert_eq!(image.summary(), "[image shots/a.png, PNG, 29 B]");
    }
}
