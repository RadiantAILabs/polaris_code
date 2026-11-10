//! What counts as an image, how large one may be, and whether the model takes
//! them.
//!
//! Two capabilities hand the model an image: the composer's attachments
//! ([`terminal::attachments`](crate::terminal::attachments)) and the
//! `view_image` tool ([`tools::view_image`](crate::tools::view_image)). Both
//! apply the same rules, so the rules live here and neither capability reaches
//! into the other:
//!
//! | Piece | Role |
//! |-------|------|
//! | [`sniff_image`] | The type of a file from its leading bytes, never its name. |
//! | [`IMAGE_BYTES_MAX`] | The largest file that may be sent, sized so its base64 stays under the providers' 5 MB per-image limit. |
//! | [`model_accepts_images`] | Whether this host will send images to a model. A host-owned allowlist, fail-closed. |
//! | [`ImageInput`] | The operator's `--image-input` choice and the default it falls back to for a model. |
//!
//! Nothing here reads a file or a session: the functions take bytes and ids, so
//! each rule is testable on its own.

use clap::ValueEnum;
use polaris::models::llm::ImageMediaType;

use crate::providers::Provider;

/// Largest file that may be sent as an image, in bytes.
///
/// The provider limit is on the **encoded** image, and base64 costs four bytes
/// for every three, so the raw cap is three quarters of it: 3.75 MB in gives
/// 5 MB out. Capping the raw file is what lets a refusal quote a number the
/// user can compare against what `ls` told them.
pub const IMAGE_BYTES_MAX: u64 = 3_750_000;

/// The image type `data` actually is, read from its leading bytes.
///
/// Extension is not consulted at all. A screenshot saved as `.txt` is an image;
/// an archive named `.png` is not. The second value is the MIME type.
///
/// ```
/// use polaris_code::images::sniff_image;
///
/// assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\n....").map(|(_, mime)| mime), Some("image/png"));
/// assert!(sniff_image(b"PK\x03\x04 a zip named .png").is_none());
/// ```
#[must_use]
pub fn sniff_image(data: &[u8]) -> Option<(ImageMediaType, &'static str)> {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    const GIF87: &[u8] = b"GIF87a";
    const GIF89: &[u8] = b"GIF89a";
    if data.starts_with(PNG) {
        return Some((ImageMediaType::PNG, "image/png"));
    }
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some((ImageMediaType::JPEG, "image/jpeg"));
    }
    if data.starts_with(GIF87) || data.starts_with(GIF89) {
        return Some((ImageMediaType::GIF, "image/gif"));
    }
    if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return Some((ImageMediaType::WEBP, "image/webp"));
    }
    None
}

/// The framework media type for one of the four MIME types [`sniff_image`]
/// reports, or `None` for anything else.
#[must_use]
pub fn media_type_of(mime: &str) -> Option<ImageMediaType> {
    match mime {
        "image/png" => Some(ImageMediaType::PNG),
        "image/jpeg" => Some(ImageMediaType::JPEG),
        "image/gif" => Some(ImageMediaType::GIF),
        "image/webp" => Some(ImageMediaType::WEBP),
        _ => None,
    }
}

/// The short format name a transcript line uses for `mime`: `PNG`, `JPEG`,
/// `GIF` or `WEBP`.
#[must_use]
pub fn format_name(mime: &str) -> String {
    mime.strip_prefix("image/")
        .unwrap_or(mime)
        .to_ascii_uppercase()
}

/// Whether this host will send image blocks to `model_id`.
///
/// **Fail-closed.** The framework's `ModelRegistry` carries no capability
/// metadata, so this is a host-owned allowlist, and a model it does not
/// recognise is treated as text-only. Refusing an attachment for a model that
/// could in fact have read it costs the user one message; sending one to a
/// model that cannot costs them a failed request they did not ask for and
/// cannot diagnose.
///
/// The id is the registry's `provider/model` form. A Bedrock Claude id such as
/// `aws.bedrock/global.anthropic.claude-sonnet-5-5` is read by the Claude
/// family it names, whatever inference-profile prefix (`global.`, `us.`, `eu.`)
/// it carries. Known exceptions inside an otherwise vision-capable family are
/// listed explicitly: `claude-3-5-haiku` and the `claude-2`/`claude-instant`
/// generations take text only.
///
/// ```
/// use polaris_code::images::model_accepts_images;
///
/// assert!(model_accepts_images("anthropic/claude-sonnet-4-6"));
/// assert!(model_accepts_images("aws.bedrock/global.anthropic.claude-sonnet-5-5"));
/// assert!(!model_accepts_images("anthropic/claude-3-5-haiku-latest"));
/// assert!(!model_accepts_images("somevendor/brand-new-model"));
/// ```
#[must_use]
pub fn model_accepts_images(model_id: &str) -> bool {
    let name = model_id
        .rsplit_once('/')
        .map_or(model_id, |(_, model)| model)
        .to_ascii_lowercase();
    // Bedrock names a Claude model `<profile>.anthropic.claude-…`; the family
    // starts at `claude-`.
    let model = name
        .split_once("anthropic.")
        .map_or(name.as_str(), |(_, family)| family);
    const TEXT_ONLY: &[&str] = &[
        "claude-3-5-haiku",
        "claude-3.5-haiku",
        "claude-instant",
        "claude-2",
    ];
    if TEXT_ONLY.iter().any(|name| model.starts_with(name)) {
        return false;
    }
    const VISION: &[&str] = &[
        "claude-3",
        "claude-sonnet-",
        "claude-opus-",
        "claude-haiku-",
        "claude-fable-",
        "gpt-4o",
        "gpt-4-turbo",
        "gpt-4.1",
        "gpt-5",
        "o3",
        "o4",
    ];
    VISION.iter().any(|name| model.starts_with(name))
}

/// Whether the model takes images, as the operator states it with
/// `--image-input`, `POLARIS_CODE_IMAGE_INPUT` or the `image_input` key.
///
/// The setting decides whether the `view_image` tool is registered. It does not
/// affect the composer's attachments, which check the model they are sent to.
///
/// # Examples
///
/// ```
/// use polaris_code::images::ImageInput;
///
/// // Unset, it follows the model: on for Claude over Anthropic or Bedrock.
/// assert!(ImageInput::Auto.accepts("anthropic/claude-sonnet-5-5"));
/// assert!(!ImageInput::Auto.accepts("openai-chat/gpt-5.5"));
/// // Stated, it wins whatever the model.
/// assert!(ImageInput::On.accepts("openai-chat/gpt-5.5"));
/// assert!(!ImageInput::Off.accepts("anthropic/claude-sonnet-5-5"));
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, ValueEnum)]
pub enum ImageInput {
    /// On for a Claude model reached through Anthropic or Bedrock, off for
    /// every other route. A text-only model rejects a request that carries an
    /// image, so the default turns the tool on only where it is known to work.
    #[default]
    Auto,
    /// The model takes images. The tool is registered whatever the route.
    On,
    /// The model takes text only. The tool is not registered.
    Off,
}

impl ImageInput {
    /// Whether images may go to `model_id`, a `provider/model` id.
    ///
    /// `Auto` is on only where both the route and the model are known to take
    /// image tool results. The `OpenAI` Responses route and Chat Completions
    /// routes stay off until stated: a Chat Completions server can serve any
    /// model, most of them text-only.
    #[must_use]
    pub fn accepts(self, model_id: &str) -> bool {
        match self {
            Self::On => true,
            Self::Off => false,
            Self::Auto => {
                matches!(
                    Provider::of_model(model_id),
                    Some(Provider::Anthropic | Provider::Bedrock)
                ) && model_accepts_images(model_id)
            }
        }
    }
}

/// Standard base64, no line breaks — the encoding the provider block wants.
///
/// Hand-rolled to avoid a dependency for one alphabet lookup.
#[must_use]
pub(crate) fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// A byte count as a refusal or a transcript line quotes it.
#[must_use]
pub(crate) fn fmt_bytes(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let value = bytes as f64;
    if value >= 1_000_000.0 {
        format!("{:.1} MB", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.1} KB", value / 1_000.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_1PX: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // Every byte value, so a lookup off by one in the high half shows up.
        let all: Vec<u8> = (0..=255u8).collect();
        let encoded = base64(&all);
        assert_eq!(encoded.len(), 344);
        assert!(
            encoded.starts_with("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKiss")
        );
        assert!(encoded.ends_with("+/w=="));
    }

    #[test]
    fn content_decides_the_type_not_the_extension() {
        assert!(matches!(
            sniff_image(PNG_1PX),
            Some((ImageMediaType::PNG, "image/png"))
        ));
        assert!(matches!(
            sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some((ImageMediaType::JPEG, "image/jpeg"))
        ));
        assert!(matches!(
            sniff_image(b"GIF87a....."),
            Some((ImageMediaType::GIF, "image/gif"))
        ));
        assert!(matches!(
            sniff_image(b"GIF89a....."),
            Some((ImageMediaType::GIF, "image/gif"))
        ));
        assert!(matches!(
            sniff_image(b"RIFF\0\0\0\0WEBPVP8 "),
            Some((ImageMediaType::WEBP, "image/webp"))
        ));
        assert!(sniff_image(b"PK\x03\x04").is_none());
        assert!(sniff_image(b"").is_none());
        // A text file, and a truncated PNG signature, are not images.
        assert!(sniff_image(b"fn main() {}\n").is_none());
        assert!(sniff_image(&PNG_1PX[..7]).is_none());
        // RIFF with a different payload (a WAV) is not an image.
        assert!(sniff_image(b"RIFF\0\0\0\0WAVEfmt ").is_none());
        // A short RIFF header cannot be a WebP.
        assert!(sniff_image(b"RIFF\0\0\0\0WEB").is_none());
    }

    #[test]
    fn a_mime_type_maps_back_to_its_media_type_and_short_name() {
        for (mime, name) in [
            ("image/png", "PNG"),
            ("image/jpeg", "JPEG"),
            ("image/gif", "GIF"),
            ("image/webp", "WEBP"),
        ] {
            assert!(media_type_of(mime).is_some(), "{mime}");
            assert_eq!(format_name(mime), name);
        }
        assert!(media_type_of("image/svg+xml").is_none());
        assert!(media_type_of("text/plain").is_none());
    }

    #[test]
    fn an_unknown_model_is_treated_as_text_only() {
        assert!(model_accepts_images("anthropic/claude-sonnet-4-6"));
        assert!(model_accepts_images("anthropic/claude-opus-4-1"));
        assert!(model_accepts_images("anthropic/claude-3-7-sonnet-latest"));
        assert!(model_accepts_images("anthropic/claude-fable-5-1"));
        assert!(model_accepts_images("openai/gpt-4o-mini"));
        assert!(!model_accepts_images("anthropic/claude-3-5-haiku-latest"));
        assert!(!model_accepts_images("anthropic/claude-2.1"));
        assert!(!model_accepts_images("anthropic/claude-instant-1.2"));
        assert!(
            !model_accepts_images("somevendor/brand-new-model"),
            "a model this host has never heard of fails closed"
        );
    }

    #[test]
    fn a_bedrock_claude_id_is_read_by_the_family_it_names() {
        for id in [
            "aws.bedrock/global.anthropic.claude-opus-5-5",
            "aws.bedrock/global.anthropic.claude-sonnet-5-5",
            "aws.bedrock/us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "aws.bedrock/eu.anthropic.claude-haiku-4-5-20251001-v1:0",
            "aws.bedrock/anthropic.claude-3-haiku-20240307-v1:0",
        ] {
            assert!(model_accepts_images(id), "{id}");
        }
        for id in [
            "aws.bedrock/anthropic.claude-3-5-haiku-20241022-v1:0",
            "aws.bedrock/us.anthropic.claude-3-5-haiku-20241022-v1:0",
            "aws.bedrock/amazon.nova-pro-v1:0",
            "aws.bedrock/openai.gpt-oss-120b-1:0",
        ] {
            assert!(!model_accepts_images(id), "{id}");
        }
    }

    #[test]
    fn auto_is_on_for_claude_over_anthropic_or_bedrock_and_off_elsewhere() {
        for id in [
            "anthropic/claude-sonnet-5-5",
            "anthropic/claude-haiku-4-5",
            "aws.bedrock/global.anthropic.claude-sonnet-5-5",
        ] {
            assert!(ImageInput::Auto.accepts(id), "{id}");
        }
        for id in [
            // Chat Completions serves arbitrary models; most are text-only.
            "openai-chat/gpt-5.5",
            "openai-chat/vendor/some-model:route",
            // The Responses route does not carry image tool results yet.
            "openai/gpt-5.5",
            // A text-only Claude, and a Bedrock model that is not Claude.
            "anthropic/claude-3-5-haiku-latest",
            "aws.bedrock/amazon.nova-pro-v1:0",
            // No known route.
            "somevendor/brand-new-model",
            "bare-model-name",
        ] {
            assert!(!ImageInput::Auto.accepts(id), "{id}");
        }
    }

    #[test]
    fn an_explicit_choice_wins_over_the_model() {
        assert!(ImageInput::On.accepts("openai-chat/gpt-5.5"));
        assert!(ImageInput::On.accepts("somevendor/brand-new-model"));
        assert!(!ImageInput::Off.accepts("anthropic/claude-sonnet-5-5"));
        assert_eq!(ImageInput::default(), ImageInput::Auto);
    }

    #[test]
    fn byte_counts_read_the_way_ls_would() {
        assert_eq!(fmt_bytes(900), "900 B");
        assert_eq!(fmt_bytes(999), "999 B");
        assert_eq!(fmt_bytes(1_000), "1.0 KB");
        assert_eq!(fmt_bytes(34_000), "34.0 KB");
        assert_eq!(fmt_bytes(IMAGE_BYTES_MAX), "3.8 MB");
    }
}
