//! End-to-end tests: bytes in, Markdown out, through the real registry.

use crate::{ConvertOptions, Engine, Limits, RenderOptions, StreamInfo};

fn engine() -> Engine {
    Engine::new(ConvertOptions::default()).expect("default profile builds")
}

fn convert(bytes: &[u8], info: &StreamInfo) -> crate::Conversion {
    engine().convert(bytes, info).expect("converts")
}

#[test]
fn csv_reaches_the_csv_converter_and_renders_a_table() {
    let out = convert(
        b"name,age\nada,36\n",
        &StreamInfo::new().with_filename("people.csv"),
    );
    assert_eq!(out.converter, "csv");
    assert!(out.markdown.contains("| name | age |"), "{}", out.markdown);
}

#[test]
fn a_csv_mislabelled_as_plain_text_still_reaches_the_csv_converter() {
    // The extension outranks a generic declared type. This is the case
    // markitdown's guess ladder exists for.
    let info = StreamInfo::new()
        .with_mimetype("text/plain")
        .with_filename("people.csv");
    assert_eq!(convert(b"a,b\n1,2\n", &info).converter, "csv");
}

#[test]
fn an_unnamed_text_stream_falls_through_to_the_text_converter() {
    let out = convert(b"just some prose\n", &StreamInfo::new());
    assert_eq!(out.converter, "text");
    assert!(out.markdown.contains("just some prose"));
}

#[test]
fn a_json_file_with_a_lying_extension_still_converts() {
    // Content sniffing cannot help here (JSON has no magic bytes), so this
    // exercises the fall-through: the JSON converter declines, and text
    // catches it rather than the whole call failing.
    let info = StreamInfo::new().with_filename("data.docx");
    let out = engine().convert(br#"{"a":1}"#, &info);
    assert!(out.is_ok(), "{:?}", out.err());
}

#[test]
fn markdown_input_survives_a_round_trip_unescaped() {
    let src = "# Heading\n\n- one\n- two\n";
    let out = convert(src.as_bytes(), &StreamInfo::new().with_filename("in.md"));
    assert!(out.markdown.contains("# Heading"), "{}", out.markdown);
    assert!(!out.markdown.contains("\\#"), "{}", out.markdown);
}

#[test]
fn front_matter_is_emitted_when_asked_for() {
    let e = Engine::new(ConvertOptions {
        output: RenderOptions {
            front_matter: crate::FrontMatter::Yaml,
            ..RenderOptions::default()
        },
        ..ConvertOptions::default()
    })
    .unwrap();
    let out = e
        .convert(b"# T\n\nbody\n", &StreamInfo::new().with_filename("x.md"))
        .unwrap();
    assert!(out.markdown.starts_with("---\n"), "{}", out.markdown);
    assert!(out.markdown.contains("title: \"T\""), "{}", out.markdown);
}

#[test]
fn the_input_ceiling_is_enforced_before_any_parsing() {
    let e = Engine::new(ConvertOptions {
        limits: Limits {
            max_input_bytes: 16,
            ..Limits::default()
        },
        ..ConvertOptions::default()
    })
    .unwrap();
    let err = e
        .convert(&vec![b'a'; 1024], &StreamInfo::new())
        .unwrap_err();
    assert_eq!(err.code(), "limit_exceeded");
}

#[test]
fn every_compiled_converter_is_reachable_by_name() {
    // `formats.enable` is validated against this list, so a converter that is
    // registered but unnamed would be impossible to enable.
    let names = crate::available_formats();
    assert!(!names.is_empty());
    for n in &names {
        assert!(!n.is_empty());
        let e = Engine::new(ConvertOptions {
            formats: crate::FormatSelection {
                enable: Some(vec![(*n).to_owned()]),
            },
            ..ConvertOptions::default()
        });
        assert!(e.is_ok(), "{n} could not be enabled on its own");
    }
}

#[test]
fn output_is_deterministic_for_the_same_input() {
    let info = StreamInfo::new().with_filename("x.csv");
    let a = convert(b"a,b\n1,2\n", &info);
    let b = convert(b"a,b\n1,2\n", &info);
    assert_eq!(a.markdown, b.markdown);
}

#[test]
fn a_conversion_reports_which_signal_chose_the_format() {
    let out = convert(b"a,b\n1,2\n", &StreamInfo::new().with_filename("x.csv"));
    assert_eq!(out.detected_via, "extension");
}

#[cfg(feature = "web")]
#[test]
fn html_reaches_the_html_converter() {
    let out = convert(
        b"<h1>Hi</h1><p>there</p>",
        &StreamInfo::new().with_filename("page.html"),
    );
    assert_eq!(out.converter, "html");
    assert!(out.markdown.contains("# Hi"), "{}", out.markdown);
}
