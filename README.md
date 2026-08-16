# `mcpg-markdown-convert`

> `rust-lib` · package `mcpg-markdown-convert` · Apache-2.0

The document-to-Markdown engine behind the `dev.mcpg.backend.markdown` plugin.
Given bytes and whatever the caller believes about them, it picks a converter,
parses into a small IR, and renders CommonMark + GFM — optionally through
operator MiniJinja templates.

```rust
use mcpg_markdown_convert::{ConvertOptions, Engine, StreamInfo};

let engine = Engine::new(ConvertOptions::default())?;
let info = StreamInfo::new().with_filename("data.csv");
let out = engine.convert(b"name,age\nada,36\n", &info)?;

assert_eq!(out.converter, "csv");
assert!(out.markdown.contains("| name | age |"));
```

## Why it is a separate crate

Nothing here touches the outside world: no filesystem, no network, no host
handle, no clock. Fetching the bytes, calling a model to caption an image and
stamping a timestamp into front matter all belong to the plugin. That boundary
is what makes every converter unit-testable without a gateway, and it is the
structural reason an image reference inside a document is rendered as a link
and never followed — this crate has no way to fetch it.

## Shape

```
bytes + StreamInfo
  → detect()            prioritised guesses: magic bytes, extension, declared MIME
  → ConverterRegistry   priority-ordered; accepts() then convert()
  → Document            the IR: blocks, metadata, warnings
  → render()            CommonMark + GFM, or a template
```

The guess-then-try loop is markitdown's, and it is the reason a mislabelled
file still converts: a converter that would have rejected the caller's declared
type gets another look under the content-derived guess.

`accepts()` receives a `Probe` — a fixed prefix of the bytes — rather than a
seekable stream. markitdown can only *document* that `accepts` must restore the
cursor; here it is unrepresentable to break.

## Formats

| Group (cargo feature) | Converters |
|---|---|
| `text` (always) | plain text, Markdown, CSV/TSV, JSON, NDJSON, Jupyter notebooks, XML, RSS/Atom |
| `web` | HTML (via `htmd`) |
| `office` | DOCX, PPTX, XLSX/XLSM/XLSB/XLS/ODS (via `calamine`), EPUB, ZIP |
| `pdf` | PDF (text only — see below) |
| `media` | images (EXIF), audio (tags) |
| `email` | Outlook `.msg` |

Features decide what is *compiled in*; the plugin's `formats.enable` decides
what an operator has *turned on*. Both exist so a size- or audit-constrained
build can be narrower than the default, and so CI can compile each group in
isolation.

DOCX, PPTX and EPUB are hand-written walkers on `zip` + `quick-xml`, both
already in the workspace. That adds no dependency and keeps the OOXML→IR
mapping — which paragraph styles become headings, whether a cell keeps its line
breaks, what happens to a hyperlink — under our control.

## Degradation is never silent

markitdown degrades quietly: an absent optional dependency unregisters a
converter, and the caller gets a worse result from some other one with no
signal. Every degradation here lands in `Document::warnings` and reaches the
caller: truncation, a skipped archive member, a PDF page with no text layer, a
low-confidence structural guess, a type mismatch between content and label.

## Safety posture

- **XML external entities are never resolved**, and DOCTYPE declarations are
  read and discarded. One entry point (`converters::xml::read_events`) serves
  every XML-derived format, so the posture is stated and enforced in one place.
- **Archives are metered as they decompress**, never trusted by their declared
  size. `max_expanded_bytes`, `max_depth` and `max_embedded_documents` are
  shared with the parent conversion, so nesting cannot multiply the total.
- **Every converter runs under a panic guard**, so one malformed file fails one
  attempt and the loop falls through rather than failing the request.
- **`#![forbid(unsafe_code)]`.**
- **Escaping is a renderer invariant, not a converter habit.** Converters put
  text into `Span::Text`; only the renderer writes Markdown syntax. A cell
  containing `|` cannot break a GFM table because no converter is in a position
  to let it.

## PDF, honestly

`pdf-extract` returns text in content-stream order. There is no multi-column
reading order, no table reconstruction, no heading inference from font metrics
and no OCR. Structure is guessed from line shape — a short standalone line
with no terminal punctuation, or a `3.2 Results` prefix — and every such guess
raises a `heuristic_applied` warning. Pages with no extractable text raise
`no_text_layer`, which is the signal that a document was scanned.

## Testing

```sh
./dev test mcpg-markdown-convert
```

Three layers:

**Unit tests** per converter, plus the escaping invariants and a hostile
corpus — zip bombs, XXE, billion laughs, deep nesting, lying size headers,
truncated containers, compound files that are not messages. Each asserts a
clean error and a bounded process, never a panic escaping the guard.

**The golden corpus** (`src/corpus.rs`, `tests/golden/`) records what the
engine actually emits, one file per case, compared byte-for-byte. Unit tests
assert *properties* — the table has two columns, the pipe is escaped — which
means a change to blank-line handling or heading spacing can alter every
document while the suite stays green. The corpus turns that into a diff.

A test asserts every compiled converter has a case, so a new format cannot
land without one. `spreadsheet` is the single documented exception: `calamine`
reads but does not write, so a fixture would mean either a committed binary
blob or a writer dependency.

To change recorded output:

```sh
UPDATE_GOLDEN=1 cargo test -p mcpg-markdown-convert --features all
```

Then **read the diff**. A golden updated without reading it is worse than no
golden, because it turns a signal into a ritual. On its first run the corpus
found two real defects that the property tests had missed: DOCX losing the
spaces at run boundaries (`Revenue**rose**`), and the PDF heading heuristic
being unreachable because it was tested per paragraph on output that has no
blank lines.

**Fixtures are built, not committed** (`src/fixtures.rs`). A checked-in
`.docx` is an opaque blob nobody reviews, and it would ride into the public
mirror; a builder keeps the input readable in the diff. The builders are
deterministic, because the corpus compares bytes.
