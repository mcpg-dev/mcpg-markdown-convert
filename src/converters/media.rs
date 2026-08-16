//! Images and audio — metadata now, model-generated description later.
//!
//! markitdown shells out to `exiftool` and, for audio, to a transcription
//! service. Both converters here produce the metadata half in pure Rust and
//! leave an [`Block::Image`] in place for the plugin's enrichment pass to
//! caption, or a marker paragraph for audio to transcribe.
//!
//! What comes out with enrichment off is a metadata sheet — honestly labelled
//! as such by a `Degraded` warning, because a caller who asked to convert a
//! photograph and received its shutter speed should be told why.

use crate::cx::ConvertCx;
use crate::error::{ConvertError, Warning, WarningKind};
use crate::ir::{Block, Document, Image, ImageRef, Inline};
use crate::registry::{Converter, PRIORITY_SPECIFIC};
use crate::stream_info::{Probe, StreamInfo};

pub struct ImageConverter;

impl Converter for ImageConverter {
    fn name(&self) -> &'static str {
        "image"
    }

    fn priority(&self) -> i32 {
        PRIORITY_SPECIFIC
    }

    fn accepts(&self, probe: &Probe<'_>, info: &StreamInfo) -> bool {
        info.mimetype
            .as_deref()
            .is_some_and(|m| m.starts_with("image/"))
            || (matches!(
                info.extension.as_deref(),
                Some(
                    "jpg"
                        | "jpeg"
                        | "png"
                        | "gif"
                        | "webp"
                        | "tif"
                        | "tiff"
                        | "heic"
                        | "heif"
                        | "avif"
                )
            ) && !probe.looks_textual())
    }

    fn convert(
        &self,
        bytes: &[u8],
        info: &StreamInfo,
        _cx: &ConvertCx<'_>,
    ) -> Result<Document, ConvertError> {
        let mut doc = Document::new();
        if let Some(f) = &info.filename {
            doc = doc.with_title(f.clone());
        }

        let fields = read_exif(bytes);
        for (k, v) in &fields {
            match k.as_str() {
                "Artist" => doc.metadata.author = Some(v.clone()),
                "DateTimeOriginal" | "DateTime" => {
                    doc.metadata.created.get_or_insert(v.clone());
                }
                _ => doc.metadata.set(k.clone(), v.clone()),
            }
        }

        if let Some(desc) = fields
            .iter()
            .find(|(k, _)| k == "ImageDescription")
            .map(|(_, v)| v.clone())
        {
            doc.push(Block::Paragraph(Inline::text(desc)));
        }

        // The enrichment pass looks for this block. With enrichment off it
        // renders as a named placeholder, which is the honest output.
        doc.push(Block::Image(Image {
            alt: info.filename.clone(),
            caption: None,
            source: match &info.url {
                Some(u) => ImageRef::Resource(u.clone()),
                None => ImageRef::None,
            },
        }));

        if fields.is_empty() {
            doc.warn(Warning::new(
                WarningKind::Degraded,
                "the image carries no EXIF metadata; without LLM enrichment there is \
                 nothing to convert but the filename",
            ));
        } else {
            doc.warn(Warning::new(
                WarningKind::Degraded,
                "image converted from metadata only; enable llm.enrich.images for a \
                 description of what it depicts",
            ));
        }
        Ok(doc)
    }
}

/// EXIF fields as `(tag, display value)`. Never fails: a JPEG with a
/// malformed APP1 segment is still an image worth reporting.
fn read_exif(bytes: &[u8]) -> Vec<(String, String)> {
    let mut cursor = std::io::Cursor::new(bytes);
    let Ok(exif) = exif::Reader::new().read_from_container(&mut cursor) else {
        return Vec::new();
    };
    exif.fields()
        .filter(|f| KEEP_TAGS.contains(&f.tag.description().unwrap_or("")))
        .map(|f| {
            (
                f.tag.description().unwrap_or("Unknown").replace(' ', ""),
                f.display_value().with_unit(&exif).to_string(),
            )
        })
        .collect()
}

/// A curated set rather than every tag. A full EXIF dump is hundreds of
/// fields of camera internals, which would drown the document it describes.
const KEEP_TAGS: &[&str] = &[
    "Image title",
    "Image description",
    "Manufacturer of image input equipment",
    "Model of image input equipment",
    "Date and time of image creation",
    "Date and time of original data generation",
    "Person who created the image",
    "Copyright holder",
    "Image width",
    "Image height",
    "Orientation of image",
    "Exposure time",
    "F number",
    "ISO speed",
    "Lens model",
    "GPS latitude",
    "GPS longitude",
];

pub struct AudioConverter;

impl Converter for AudioConverter {
    fn name(&self) -> &'static str {
        "audio"
    }

    fn priority(&self) -> i32 {
        PRIORITY_SPECIFIC
    }

    fn accepts(&self, probe: &Probe<'_>, info: &StreamInfo) -> bool {
        info.mimetype
            .as_deref()
            .is_some_and(|m| m.starts_with("audio/"))
            || (matches!(
                info.extension.as_deref(),
                Some("mp3" | "wav" | "flac" | "m4a" | "aac" | "ogg" | "opus")
            ) && !probe.looks_textual())
    }

    fn convert(
        &self,
        bytes: &[u8],
        info: &StreamInfo,
        _cx: &ConvertCx<'_>,
    ) -> Result<Document, ConvertError> {
        use lofty::file::{AudioFile, TaggedFileExt};
        use lofty::prelude::ItemKey;

        let mut doc = Document::new();
        if let Some(f) = &info.filename {
            doc = doc.with_title(f.clone());
        }

        let mut cursor = std::io::Cursor::new(bytes);
        match lofty::probe::Probe::new(&mut cursor).guess_file_type() {
            // `guess_file_type` succeeds with no verdict on bytes it does not
            // recognise. Treating that as readable would silently emit a
            // metadata sheet for a file that is not audio at all, so it is an
            // error here and the guess ladder moves on.
            Ok(probed) if probed.file_type().is_none() => {
                return Err(ConvertError::Malformed {
                    format: "audio",
                    message: "no recognisable audio container".to_owned(),
                });
            }
            Ok(probed) => match probed.read() {
                Ok(tagged) => {
                    let props = tagged.properties();
                    doc.metadata
                        .set("duration_seconds", props.duration().as_secs().to_string());
                    if let Some(b) = props.audio_bitrate() {
                        doc.metadata.set("bitrate_kbps", b.to_string());
                    }
                    if let Some(r) = props.sample_rate() {
                        doc.metadata.set("sample_rate_hz", r.to_string());
                    }
                    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
                        for (key, label) in [
                            (ItemKey::TrackTitle, "track_title"),
                            (ItemKey::TrackArtist, "artist"),
                            (ItemKey::AlbumTitle, "album"),
                            (ItemKey::RecordingDate, "recorded"),
                            (ItemKey::Comment, "comment"),
                        ] {
                            if let Some(v) = tag.get_string(key) {
                                if label == "artist" {
                                    doc.metadata.author = Some(v.to_owned());
                                } else {
                                    doc.metadata.set(label, v);
                                }
                            }
                        }
                        if let Some(t) = tag.get_string(ItemKey::TrackTitle) {
                            doc.title = Some(t.to_owned());
                        }
                    }
                }
                Err(e) => {
                    doc.warn(Warning::new(
                        WarningKind::Degraded,
                        format!("audio tags could not be read: {e}"),
                    ));
                }
            },
            Err(e) => {
                return Err(ConvertError::Malformed {
                    format: "audio",
                    message: e.to_string(),
                });
            }
        }

        // The enrichment pass replaces this with the transcript.
        doc.push(Block::Paragraph(Inline::text(TRANSCRIPT_PLACEHOLDER)));
        doc.warn(Warning::new(
            WarningKind::Degraded,
            "audio converted from metadata only; enable llm.enrich.audio for a transcript",
        ));
        Ok(doc)
    }
}

/// The marker the enrichment pass swaps for a transcript. A converter cannot
/// call a model — it has no host — so it leaves a place for one.
pub const TRANSCRIPT_PLACEHOLDER: &str = "[audio: no transcript]";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cx::{Budget, Limits};

    fn convert(c: &dyn Converter, bytes: &[u8], info: &StreamInfo) -> Document {
        let b = Budget::new(Limits::default());
        let cx = ConvertCx::new(&b);
        c.convert(bytes, info, &cx).expect("converts")
    }

    /// A 1×1 PNG. No EXIF, which is the common case for generated images.
    fn png() -> Vec<u8> {
        vec![
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H',
            b'D', b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x00, b'I', b'E', b'N', b'D', 0xAE,
            0x42, 0x60, 0x82,
        ]
    }

    #[test]
    fn an_image_yields_an_image_block_for_enrichment_to_find() {
        let info = StreamInfo::new().with_filename("photo.png");
        let doc = convert(&ImageConverter, &png(), &info);
        assert!(
            doc.blocks.iter().any(|b| matches!(b, Block::Image(_))),
            "{:?}",
            doc.blocks
        );
    }

    #[test]
    fn a_resource_uri_reaches_the_image_block_so_enrichment_can_fetch_it() {
        let info = StreamInfo::new()
            .with_filename("photo.png")
            .with_url("mcpg-resource://hash:abc");
        let doc = convert(&ImageConverter, &png(), &info);
        match doc.blocks.iter().find_map(|b| match b {
            Block::Image(i) => Some(&i.source),
            _ => None,
        }) {
            Some(ImageRef::Resource(u)) => assert_eq!(u, "mcpg-resource://hash:abc"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_metadata_only_conversion_says_so() {
        let info = StreamInfo::new().with_filename("photo.png");
        let doc = convert(&ImageConverter, &png(), &info);
        assert!(
            doc.warnings.iter().any(|w| w.kind == WarningKind::Degraded),
            "a metadata-only image conversion must not look like a full one"
        );
    }

    #[test]
    fn broken_exif_does_not_fail_the_conversion() {
        let mut bytes = png();
        bytes.extend_from_slice(b"\xFF\xE1\x00\x10Exif\x00\x00garbage");
        let info = StreamInfo::new().with_filename("photo.png");
        let doc = convert(&ImageConverter, &bytes, &info);
        assert!(doc.blocks.iter().any(|b| matches!(b, Block::Image(_))));
    }

    #[test]
    fn images_are_matched_by_mime_or_by_extension_plus_binary_content() {
        let bytes = png();
        let bin = Probe::new(&bytes);
        assert!(ImageConverter.accepts(&bin, &StreamInfo::new().with_mimetype("image/png")));
        assert!(ImageConverter.accepts(&bin, &StreamInfo::new().with_extension("png")));
        // A text file named .png is not an image.
        assert!(!ImageConverter.accepts(
            &Probe::new(b"plain text"),
            &StreamInfo::new().with_extension("png")
        ));
    }

    #[test]
    fn audio_declines_text_and_accepts_by_mime() {
        assert!(AudioConverter.accepts(
            &Probe::new(&[0xFF, 0xFB, 0x00]),
            &StreamInfo::new().with_mimetype("audio/mpeg")
        ));
        assert!(!AudioConverter.accepts(
            &Probe::new(b"not audio"),
            &StreamInfo::new().with_extension("mp3")
        ));
    }

    #[test]
    fn unreadable_audio_is_malformed_not_a_panic() {
        let b = Budget::new(Limits::default());
        let cx = ConvertCx::new(&b);
        let e = AudioConverter.convert(&[0u8; 32], &StreamInfo::new().with_extension("mp3"), &cx);
        assert!(e.is_err(), "garbage must not convert");
    }
}
