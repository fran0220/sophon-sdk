//! DOCX text extraction shared by read tools.
//!
//! Minimal zip + quick-xml implementation in the same shape as the PPTX
//! extractor: unzip, walk the WordprocessingML text runs of
//! `word/document.xml`, one line per `<w:p>` paragraph. Explicit line breaks
//! (`<w:br/>`, `<w:cr/>`) and tabs (`<w:tab/>`) inside a run are kept, so a
//! manually broken paragraph reads the way the document shows it. Tables need
//! no special casing: every cell's content is itself a `<w:p>`.

use std::io::{Cursor, Read};

use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

/// Cap on the decompressed size of the document XML entry we read, guarding
/// against zip bombs (the compressed input is already capped by the caller).
const MAX_XML_ENTRY_BYTES: u64 = 64 * 1024 * 1024;

/// Extract plain text from DOCX bytes.
///
/// Returns the document body text, or an error string suitable for
/// `ReadFileOutput::FileReadError`.
pub(crate) fn extract_docx_text_from_bytes(bytes: &[u8]) -> Result<String, String> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| format!("Failed to open DOCX archive: {e}"))?;
    let document = read_entry(&mut archive, "word/document.xml")?
        .ok_or_else(|| "No document body found in DOCX".to_string())?;
    extract_wordprocessingml_text(&document).map_err(|e| format!("Error parsing document: {e}"))
}

/// Read a zip entry to a string, `Ok(None)` if the entry does not exist.
fn read_entry(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    name: &str,
) -> Result<Option<String>, String> {
    let file = match archive.by_name(name) {
        Ok(file) => file,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(format!("Failed to open {name}: {e}")),
    };
    let mut content = String::new();
    file.take(MAX_XML_ENTRY_BYTES)
        .read_to_string(&mut content)
        .map_err(|e| format!("Failed to read {name}: {e}"))?;
    if content.len() as u64 == MAX_XML_ENTRY_BYTES {
        return Err(format!("{name} exceeds the decompressed size limit"));
    }
    Ok(Some(content))
}

/// Extract text from WordprocessingML: the character content of `<w:t>` runs,
/// concatenated per paragraph, one line per `<w:p>` paragraph, with in-run
/// breaks and tabs preserved.
fn extract_wordprocessingml_text(xml: &str) -> Result<String, String> {
    // No `trim_text`: whitespace inside `<w:t>` runs is significant (runs are
    // frequently split mid-sentence), and text outside runs is already
    // excluded by the `in_text_run` gate below.
    let mut reader = Reader::from_str(xml);

    let mut text = String::new();
    let mut in_text_run = false;
    // `<w:tab/>` outside a run is a tab-stop definition (`<w:pPr><w:tabs>`),
    // not a tab character; only a run's own breaks and tabs are content.
    let mut in_run = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => match e.local_name().as_ref() {
                b"t" => in_text_run = true,
                b"r" => in_run = true,
                _ => {}
            },
            Ok(Event::Text(e)) if in_text_run => {
                let content = e.xml_content().map_err(|e| e.to_string())?;
                text.push_str(&content);
            }
            // quick-xml ≥0.37 emits `&amp;` / `&#233;` as separate events
            // instead of unescaping them inside `Event::Text`.
            Ok(Event::GeneralRef(e)) if in_text_run => {
                if let Some(ch) = e.resolve_char_ref().map_err(|e| e.to_string())? {
                    text.push(ch);
                } else {
                    let name = e.decode().map_err(|e| e.to_string())?;
                    match quick_xml::escape::resolve_predefined_entity(&name) {
                        Some(resolved) => text.push_str(resolved),
                        // Unknown entity: keep the raw reference visible.
                        None => {
                            text.push('&');
                            text.push_str(&name);
                            text.push(';');
                        }
                    }
                }
            }
            // Explicit breaks and tabs are empty elements inside a run.
            Ok(Event::Empty(ref e)) if in_run => match e.local_name().as_ref() {
                b"br" | b"cr" => text.push('\n'),
                b"tab" => text.push('\t'),
                _ => {}
            },
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                b"t" => in_text_run = false,
                b"r" => in_run = false,
                // End of paragraph: line break.
                b"p" if !text.is_empty() && !text.ends_with('\n') => text.push('\n'),
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
    }
    Ok(text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    /// Build an in-memory DOCX-shaped zip from (entry name, XML) pairs.
    fn build_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, content) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(content.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn document_xml(body: &str) -> String {
        format!(
            r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
        )
    }

    #[test]
    fn extracts_paragraphs_one_per_line() {
        let body = "<w:p><w:r><w:t>Title</w:t></w:r></w:p>\
                    <w:p><w:r><w:t>First </w:t></w:r><w:r><w:t>sentence.</w:t></w:r></w:p>";
        let bytes = build_zip(&[
            ("[Content_Types].xml", "<Types/>"),
            ("word/document.xml", &document_xml(body)),
        ]);
        let text = extract_docx_text_from_bytes(&bytes).unwrap();
        assert_eq!(text, "Title\nFirst sentence.");
    }

    #[test]
    fn breaks_tabs_and_entities_survive_extraction() {
        let body =
            "<w:p><w:r><w:t>a</w:t><w:tab/><w:t>b&amp;c</w:t><w:br/><w:t>d</w:t></w:r></w:p>";
        let bytes = build_zip(&[("word/document.xml", &document_xml(body))]);
        let text = extract_docx_text_from_bytes(&bytes).unwrap();
        assert_eq!(text, "a\tb&c\nd");
    }

    #[test]
    fn table_cell_paragraphs_read_in_order() {
        let body = "<w:tbl><w:tr>\
                    <w:tc><w:p><w:r><w:t>Cell one</w:t></w:r></w:p></w:tc>\
                    <w:tc><w:p><w:r><w:t>Cell two</w:t></w:r></w:p></w:tc>\
                    </w:tr></w:tbl>";
        let bytes = build_zip(&[("word/document.xml", &document_xml(body))]);
        let text = extract_docx_text_from_bytes(&bytes).unwrap();
        assert_eq!(text, "Cell one\nCell two");
    }

    #[test]
    fn tab_stop_definitions_are_not_tab_characters() {
        // <w:tab/> under <w:pPr><w:tabs> defines a tab stop; only a run's
        // own <w:tab/> is content.
        let body = "<w:p><w:pPr><w:tabs><w:tab w:val=\"left\" w:pos=\"720\"/></w:tabs></w:pPr>\
                    <w:r><w:t>plain</w:t></w:r></w:p>";
        let bytes = build_zip(&[("word/document.xml", &document_xml(body))]);
        let text = extract_docx_text_from_bytes(&bytes).unwrap();
        assert_eq!(text, "plain");
    }

    #[test]
    fn empty_text_elements_do_not_leak_surrounding_text() {
        // A self-closing <w:t/> must not flip the in-run flag on.
        let body = "<w:p><w:r><w:t/></w:r>stray<w:r><w:t>kept</w:t></w:r></w:p>";
        let bytes = build_zip(&[("word/document.xml", &document_xml(body))]);
        let text = extract_docx_text_from_bytes(&bytes).unwrap();
        assert_eq!(text, "kept");
    }

    #[test]
    fn not_a_zip_is_an_error() {
        let err = extract_docx_text_from_bytes(b"plainly not a zip").unwrap_err();
        assert!(err.contains("Failed to open DOCX archive"), "{err}");
    }

    #[test]
    fn zip_without_document_is_an_error() {
        let bytes = build_zip(&[("[Content_Types].xml", "<Types/>")]);
        let err = extract_docx_text_from_bytes(&bytes).unwrap_err();
        assert_eq!(err, "No document body found in DOCX");
    }
}
