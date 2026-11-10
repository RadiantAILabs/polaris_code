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
//! 1. **Containment.** The path resolves with the same containment logic as
//!    `read_file` (the one [`CodeToolsConfig`] applies), against the session's
//!    [`CallRoot`]: `..`, absolute paths and symlinks that leave the working
//!    directory are refused with `escapes sandbox`. The check runs again after
//!    the file is opened, and the opened file must be the one the path names
//!    then, so a file swapped for a symlink in between is refused.
//! 2. **Kind.** The path must be a regular file. A directory, a pipe or a
//!    device is refused before it is opened, and the opened file is judged
//!    again.
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

use std::path::{Path, PathBuf};

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
/// Session record show instead. Saved session history and the ATIF export keep
/// the whole picture.
///
/// # Examples
///
/// ```
/// use polaris_code::tools::{ViewedImage, VIEW_IMAGE};
///
/// # fn main() -> Result<(), serde_json::Error> {
/// let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0";
/// let image = ViewedImage::new("shots/a.png", "image/png", png);
/// assert_eq!(image.summary(), "[image shots/a.png, PNG, 29 B]");
///
/// // The dispatch accepts it from `view_image`, and from no other tool.
/// let value = serde_json::to_value(&image)?;
/// assert!(ViewedImage::from_result(VIEW_IMAGE, &value).is_some());
/// assert!(ViewedImage::from_result("mcp__server__shot", &value).is_none());
/// # Ok(())
/// # }
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
    ///
    /// Construction is unchecked: the type, the size and the bytes are taken
    /// as given. [`from_result`](Self::from_result) is the validating entry
    /// point, and the one the dispatch uses.
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
    /// [`from_result`](Self::from_result) already refuses. It does not check
    /// the size or the encoding; take the value from `from_result`.
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
#[derive(Debug)]
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

    /// Opens `resolved`, which [`resolve`](Self::resolve) returned for `path`,
    /// and returns the file only if it is a regular file within the size cap
    /// that `path` still names inside the sandbox.
    ///
    /// Resolving and then opening by path leaves a gap in which another process
    /// can swap the file, or a parent directory, for a symlink to an outside
    /// image. So the opened file is judged from its own handle, then `path` is
    /// resolved again and must name the same file (same device and inode on
    /// unix; the same resolved path elsewhere).
    ///
    /// This does not close every race. The last check still goes through the
    /// path, so a parent directory swapped at exactly the moment of that
    /// comparison is not caught; closing that needs `openat` with
    /// `RESOLVE_BENEATH`. And the open uses neither `O_NOFOLLOW` nor
    /// `O_NONBLOCK` (they need `libc`, which this crate does not depend on), so
    /// a pipe swapped in after the first look can block the open. In neither
    /// case is anything read before the checks pass.
    async fn open_checked(
        &self,
        call: &Option<CallRoot>,
        path: &str,
        resolved: &Path,
    ) -> Result<tokio::fs::File, ToolError> {
        // Look before opening: a pipe or device would block the open, and a
        // directory has no bytes to show. This goes by path, so it only spares
        // the open; the opened file is judged below.
        let looked = tokio::fs::metadata(resolved)
            .await
            .map_err(|err| io_failure(VIEW_IMAGE, path, &err))?;
        check_kind_and_size(path, &looked)?;
        let file = tokio::fs::File::open(resolved)
            .await
            .map_err(|err| io_failure(VIEW_IMAGE, path, &err))?;
        let opened = file
            .metadata()
            .await
            .map_err(|err| io_failure(VIEW_IMAGE, path, &err))?;
        check_kind_and_size(path, &opened)?;

        let now = self.resolve(call, path)?;
        #[cfg(unix)]
        let unchanged = {
            use std::os::unix::fs::MetadataExt as _;
            // Not following a symlink: a path that has become one is not the
            // file that was opened.
            let named = tokio::fs::symlink_metadata(&now)
                .await
                .map_err(|err| io_failure(VIEW_IMAGE, path, &err))?;
            (opened.dev(), opened.ino()) == (named.dev(), named.ino())
        };
        #[cfg(not(unix))]
        let unchanged = now == resolved;
        if !unchanged {
            return Err(refusal(
                path,
                "changed while it was being opened, so it was not read. Call view_image again.",
            ));
        }
        Ok(file)
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
        let file = self.open_checked(&call_root, &path, &resolved).await?;
        let data = read_capped(file, &path).await?;
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

/// Refuses what is not a regular file, or is over [`IMAGE_BYTES_MAX`].
fn check_kind_and_size(path: &str, metadata: &std::fs::Metadata) -> Result<(), ToolError> {
    if metadata.is_dir() {
        return Err(refusal(
            path,
            "is a directory, not an image file. Call list_files to see what it holds.",
        ));
    }
    if !metadata.is_file() {
        return Err(refusal(
            path,
            "is not a regular file, so it cannot be read.",
        ));
    }
    if metadata.len() > IMAGE_BYTES_MAX {
        return Err(too_large(path, metadata.len()));
    }
    Ok(())
}

/// Reads `file` to its end, but not past one byte over [`IMAGE_BYTES_MAX`]: a
/// file that grew since the size check is refused instead of read in full.
async fn read_capped(file: tokio::fs::File, path: &str) -> Result<Vec<u8>, ToolError> {
    let mut data = Vec::new();
    file.take(IMAGE_BYTES_MAX + 1)
        .read_to_end(&mut data)
        .await
        .map_err(|err| io_failure(VIEW_IMAGE, path, &err))?;
    if data.len() as u64 > IMAGE_BYTES_MAX {
        return Err(too_large(path, data.len() as u64));
    }
    Ok(data)
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
#[derive(Debug)]
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

        // A file of exactly the cap is fine.
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

    /// Makes a named pipe at `path`, or says on stderr that this host cannot
    /// and returns `false`, so a caller skips instead of passing silently.
    #[cfg(unix)]
    fn make_fifo(path: &Path) -> bool {
        let made = std::process::Command::new("mkfifo").arg(path).status();
        let ok = made.is_ok_and(|status| status.success());
        if !ok {
            eprintln!("skipped: mkfifo is not available, so no pipe could be made");
        }
        ok
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_pipe_is_refused_without_being_opened() {
        let dir = workspace();
        let fifo = dir.path().join("pipe.png");
        if !make_fifo(&fifo) {
            return; // the check is the same one a directory takes
        }
        let refused =
            tokio::time::timeout(std::time::Duration::from_secs(5), view(&dir, "pipe.png"))
                .await
                .expect("a pipe must not block the tool")
                .unwrap_err();
        assert!(refused.contains("not a regular file"), "{refused}");
    }

    // The next tests stand in for the race: the tool has resolved a path, and
    // another process then replaces what it names. `open_checked` is the code
    // that runs next, so it is called with the path resolved beforehand.

    #[cfg(unix)]
    #[tokio::test]
    async fn a_file_swapped_for_a_symlink_out_of_the_working_directory_is_refused() {
        let outer = tempfile::tempdir().unwrap();
        let inside = outer.path().join("work");
        std::fs::create_dir_all(inside.join("sub")).unwrap();
        std::fs::write(outer.path().join("secret.png"), PNG_1PX).unwrap();
        std::fs::write(inside.join("shot.png"), PNG_1PX).unwrap();
        std::fs::write(inside.join("sub/shot.png"), PNG_1PX).unwrap();
        let toolset = ViewImageTools::new(CodeToolsConfig::new(&inside));
        let call = Some(CallRoot::new(&inside));

        // The file itself becomes a link to an image outside.
        let resolved = toolset.resolve(&call, "shot.png").unwrap();
        std::fs::remove_file(&resolved).unwrap();
        std::os::unix::fs::symlink(outer.path().join("secret.png"), &resolved).unwrap();
        let refused = toolset
            .open_checked(&call, "shot.png", &resolved)
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains("escapes sandbox"), "{refused}");

        // A parent directory becomes a link to a directory holding such an image.
        std::fs::write(outer.path().join("shot.png"), PNG_1PX).unwrap();
        let resolved = toolset.resolve(&call, "sub/shot.png").unwrap();
        std::fs::rename(inside.join("sub"), inside.join("sub-moved")).unwrap();
        std::os::unix::fs::symlink(outer.path(), inside.join("sub")).unwrap();
        let refused = toolset
            .open_checked(&call, "sub/shot.png", &resolved)
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains("escapes sandbox"), "{refused}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_file_swapped_for_a_pipe_is_refused_without_blocking() {
        let dir = workspace();
        let toolset = tools(&dir);
        let call = root(&dir);
        let resolved = toolset.resolve(&call, "shots/a.png").unwrap();
        std::fs::remove_file(&resolved).unwrap();
        if !make_fifo(&resolved) {
            return;
        }
        let refused = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            toolset.open_checked(&call, "shots/a.png", &resolved),
        )
        .await
        .expect("a pipe must not block the open")
        .unwrap_err()
        .to_string();
        assert!(refused.contains("not a regular file"), "{refused}");
    }

    #[tokio::test]
    async fn an_opened_file_that_the_path_no_longer_names_is_refused() {
        let dir = workspace();
        std::fs::write(dir.path().join("shots/b.png"), PNG_1PX).unwrap();
        let toolset = tools(&dir);
        let call = root(&dir);
        let opened = toolset.resolve(&call, "shots/a.png").unwrap();

        // Same bytes, different file: the path now names b.png.
        let refused = toolset
            .open_checked(&call, "shots/b.png", &opened)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("changed while it was being opened"),
            "{refused}"
        );
        // Nothing changed: the file is returned.
        toolset
            .open_checked(&call, "shots/a.png", &opened)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_read_stops_past_the_cap_for_a_file_that_grew_after_the_size_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grown.png");
        let mut data = PNG_1PX.to_vec();

        data.resize(IMAGE_BYTES_MAX as usize + 4096, 0);
        std::fs::write(&path, &data).unwrap();
        let file = tokio::fs::File::open(&path).await.unwrap();
        let refused = read_capped(file, "grown.png")
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains("images are capped at 3.8 MB"), "{refused}");

        data.resize(IMAGE_BYTES_MAX as usize, 0);
        std::fs::write(&path, &data).unwrap();
        let file = tokio::fs::File::open(&path).await.unwrap();
        let read = read_capped(file, "grown.png").await.unwrap();
        assert_eq!(
            read.len() as u64,
            IMAGE_BYTES_MAX,
            "exactly the cap is read"
        );
    }

    #[test]
    fn an_image_of_exactly_the_cap_is_believed() {
        let bytes = vec![0_u8; IMAGE_BYTES_MAX as usize];
        let image = ViewedImage::new("a.png", "image/png", &bytes);
        assert_eq!(
            image.data.len(),
            ENCODED_BYTES_MAX,
            "the byte cap encodes to the provider's limit exactly"
        );
        let value = serde_json::to_value(&image).unwrap();
        assert!(ViewedImage::from_result(VIEW_IMAGE, &value).is_some());
    }

    #[test]
    fn definitions_expose_view_image_without_its_call_context() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = ToolRegistry::new();
        registry.register_toolset(tools(&dir));
        let defs = registry.definitions();
        let def = defs
            .iter()
            .find(|d| d.name == VIEW_IMAGE)
            .unwrap_or_else(|| panic!("missing tool {VIEW_IMAGE}"));

        let props = &def.parameters["properties"];
        assert_eq!(props["path"]["type"], "string", "{:?}", def.parameters);
        let required = def.parameters["required"]
            .as_array()
            .expect("required list");
        assert!(required.iter().any(|name| name == "path"), "{required:?}");
        assert!(
            props.get("call_root").is_none(),
            "the call context is not a model parameter: {:?}",
            def.parameters
        );
        assert!(
            def.description.contains("read_file") && def.description.contains("3.8 MB"),
            "{}",
            def.description
        );
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
