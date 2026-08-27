//! XLSX text extraction shared by read tools.
//!
//! Minimal zip + quick-xml implementation in the same shape as the PPTX
//! extractor: unzip, resolve the workbook's sheet order and names, then walk
//! each worksheet's cells. Output is one `--- Sheet: <name> ---` header per
//! sheet and one tab-separated line per row, with cells padded to their
//! spreadsheet column so a sparse row keeps its alignment. Values are the
//! stored ones: shared and inline strings resolve to text, numbers and
//! formula results print as stored (a date therefore prints as its serial
//! number — the format registry that would render it is styling, not text).

use std::io::{Cursor, Read};

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use zip::ZipArchive;

/// Cap on the decompressed size of any single XML entry we read, guarding
/// against zip bombs (the compressed input is already capped by the caller).
const MAX_XML_ENTRY_BYTES: u64 = 64 * 1024 * 1024;

/// Extract plain text from XLSX bytes.
///
/// Returns the concatenated sheet texts, or an error string suitable for
/// `ReadFileOutput::FileReadError`.
pub(crate) fn extract_xlsx_text_from_bytes(bytes: &[u8]) -> Result<String, String> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| format!("Failed to open XLSX archive: {e}"))?;

    let shared_strings = match read_entry(&mut archive, "xl/sharedStrings.xml")? {
        Some(xml) => {
            parse_shared_strings(&xml).map_err(|e| format!("Error parsing shared strings: {e}"))?
        }
        None => Vec::new(),
    };
    let sheets = sheet_entries(&mut archive)?;
    if sheets.is_empty() {
        return Err("No worksheets found in XLSX".to_string());
    }

    let mut all_text = String::new();
    for (name, entry) in sheets {
        let sheet_xml = read_entry(&mut archive, &entry)?
            .ok_or_else(|| format!("Failed to read worksheet {entry}"))?;
        let sheet_text = extract_sheet_text(&sheet_xml, &shared_strings)
            .map_err(|e| format!("Error parsing worksheet {name}: {e}"))?;
        if !all_text.is_empty() {
            all_text.push_str("\n\n");
        }
        all_text.push_str(&format!("--- Sheet: {name} ---"));
        if !sheet_text.is_empty() {
            all_text.push('\n');
            all_text.push_str(&sheet_text);
        }
    }
    Ok(all_text)
}

/// The workbook's worksheets in workbook order as `(display name, zip entry)`.
///
/// The workbook lists sheets by relationship id and the relationships file
/// maps those ids to worksheet parts. A workbook whose relationships are
/// missing or partial falls back to `xl/worksheets/sheetN.xml` in numeric
/// order, named `Sheet N`, so a slightly odd archive still reads.
fn sheet_entries(archive: &mut ZipArchive<Cursor<&[u8]>>) -> Result<Vec<(String, String)>, String> {
    let workbook = read_entry(archive, "xl/workbook.xml")?;
    let relationships = read_entry(archive, "xl/_rels/workbook.xml.rels")?;
    if let (Some(workbook), Some(relationships)) = (workbook, relationships) {
        let by_id = parse_relationships(&relationships)
            .map_err(|e| format!("Error parsing workbook relationships: {e}"))?;
        let declared =
            parse_workbook_sheets(&workbook).map_err(|e| format!("Error parsing workbook: {e}"))?;
        let resolved: Vec<(String, String)> = declared
            .into_iter()
            .filter_map(|(name, relationship_id)| {
                let target = by_id
                    .iter()
                    .find(|(id, _)| *id == relationship_id)
                    .map(|(_, target)| target)?;
                // Targets are relative to `xl/`.
                let entry = if let Some(absolute) = target.strip_prefix('/') {
                    absolute.to_string()
                } else {
                    format!("xl/{target}")
                };
                Some((name, entry))
            })
            .collect();
        if !resolved.is_empty() {
            return Ok(resolved);
        }
    }
    let mut numbers: Vec<u32> = archive
        .file_names()
        .filter_map(|name| {
            name.strip_prefix("xl/worksheets/sheet")?
                .strip_suffix(".xml")?
                .parse()
                .ok()
        })
        .collect();
    numbers.sort_unstable();
    Ok(numbers
        .into_iter()
        .map(|n| (format!("Sheet {n}"), format!("xl/worksheets/sheet{n}.xml")))
        .collect())
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

/// One attribute's unescaped value, if present.
fn attribute(element: &BytesStart<'_>, name: &[u8]) -> Result<Option<String>, String> {
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|e| e.to_string())?;
        if attribute.key.local_name().as_ref() == name {
            return Ok(Some(
                attribute
                    .unescape_value()
                    .map_err(|e| e.to_string())?
                    .into_owned(),
            ));
        }
    }
    Ok(None)
}

/// `xl/sharedStrings.xml`: one string per `<si>`, its `<t>` runs concatenated
/// (plain and rich-text items both).
fn parse_shared_strings(xml: &str) -> Result<Vec<String>, String> {
    let mut reader = Reader::from_str(xml);
    let mut strings = Vec::new();
    let mut current = String::new();
    let mut in_item = false;
    let mut in_text = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => match e.local_name().as_ref() {
                b"si" => {
                    in_item = true;
                    current.clear();
                }
                b"t" if in_item => in_text = true,
                _ => {}
            },
            Ok(Event::Text(e)) if in_text => {
                current.push_str(&e.xml_content().map_err(|e| e.to_string())?);
            }
            Ok(Event::GeneralRef(e)) if in_text => {
                push_reference(&mut current, &e)?;
            }
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                b"t" => in_text = false,
                b"si" => {
                    in_item = false;
                    strings.push(std::mem::take(&mut current));
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
    }
    Ok(strings)
}

/// `xl/workbook.xml`: the declared sheets as `(name, relationship id)`, in
/// workbook order.
fn parse_workbook_sheets(xml: &str) -> Result<Vec<(String, String)>, String> {
    let mut reader = Reader::from_str(xml);
    let mut sheets = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e))
                if e.local_name().as_ref() == b"sheet" =>
            {
                let name = attribute(e, b"name")?.unwrap_or_default();
                if let Some(id) = attribute(e, b"id")? {
                    sheets.push((name, id));
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
    }
    Ok(sheets)
}

/// `xl/_rels/workbook.xml.rels`: relationship id to target part.
fn parse_relationships(xml: &str) -> Result<Vec<(String, String)>, String> {
    let mut reader = Reader::from_str(xml);
    let mut relationships = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e))
                if e.local_name().as_ref() == b"Relationship" =>
            {
                if let (Some(id), Some(target)) = (attribute(e, b"Id")?, attribute(e, b"Target")?) {
                    relationships.push((id, target));
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
    }
    Ok(relationships)
}

/// One worksheet as tab-separated rows.
fn extract_sheet_text(xml: &str, shared_strings: &[String]) -> Result<String, String> {
    let mut reader = Reader::from_str(xml);
    let mut rows: Vec<String> = Vec::new();
    let mut cells: Vec<String> = Vec::new();
    let mut in_row = false;
    // The current cell: its spreadsheet column (0-based), its `t` type
    // attribute, and where its text is being collected from.
    let mut cell_column: Option<usize> = None;
    let mut cell_type = String::new();
    let mut cell_value = String::new();
    let mut in_value = false;
    let mut in_inline_text = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e))
                if e.local_name().as_ref() == b"row" =>
            {
                in_row = true;
                cells.clear();
            }
            Ok(Event::Start(ref e)) if e.local_name().as_ref() == b"c" && in_row => {
                cell_column = attribute(e, b"r")?.as_deref().and_then(column_index);
                cell_type = attribute(e, b"t")?.unwrap_or_default();
                cell_value.clear();
            }
            Ok(Event::Start(ref e)) if e.local_name().as_ref() == b"v" => in_value = true,
            Ok(Event::Start(ref e)) if e.local_name().as_ref() == b"t" => in_inline_text = true,
            Ok(Event::Text(e)) if in_value || in_inline_text => {
                cell_value.push_str(&e.xml_content().map_err(|e| e.to_string())?);
            }
            Ok(Event::GeneralRef(e)) if in_value || in_inline_text => {
                push_reference(&mut cell_value, &e)?;
            }
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                b"v" => in_value = false,
                b"t" => in_inline_text = false,
                b"c" => {
                    let resolved = match cell_type.as_str() {
                        // A shared-string cell stores an index; an index the
                        // table does not hold reads as nothing rather than a
                        // number that looks like data.
                        "s" => cell_value
                            .trim()
                            .parse::<usize>()
                            .ok()
                            .and_then(|index| shared_strings.get(index).cloned())
                            .unwrap_or_default(),
                        _ => std::mem::take(&mut cell_value),
                    };
                    let column = cell_column.take().unwrap_or(cells.len());
                    while cells.len() < column {
                        cells.push(String::new());
                    }
                    cells.push(resolved);
                    cell_value.clear();
                }
                b"row" => {
                    in_row = false;
                    while cells.last().is_some_and(String::is_empty) {
                        cells.pop();
                    }
                    rows.push(cells.join("\t"));
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
    }
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    Ok(rows.join("\n"))
}

/// The 0-based column of an `A1`-style cell reference.
fn column_index(reference: &str) -> Option<usize> {
    let letters: String = reference
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
    if letters.is_empty() {
        return None;
    }
    let mut index = 0usize;
    for c in letters.chars() {
        index = index
            .checked_mul(26)?
            .checked_add(c.to_ascii_uppercase() as usize - 'A' as usize + 1)?;
    }
    Some(index - 1)
}

/// Append a general entity/character reference the way text content does.
fn push_reference(text: &mut String, e: &quick_xml::events::BytesRef<'_>) -> Result<(), String> {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    /// Build an in-memory XLSX-shaped zip from (entry name, XML) pairs.
    fn build_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, content) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(content.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    const WORKBOOK: &str = r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Budget" sheetId="1" r:id="rId1"/><sheet name="Notes" sheetId="2" r:id="rId2"/></sheets></workbook>"#;
    const RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="w" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="w" Target="worksheets/sheet2.xml"/></Relationships>"#;
    const SHARED: &str = r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>Item</t></si><si><r><t>Unit </t></r><r><t>price</t></r></si></sst>"#;

    #[test]
    fn sheets_read_in_workbook_order_with_names() {
        let sheet1 = r#"<worksheet><sheetData>
            <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row>
            <row r="2"><c r="A2" t="inlineStr"><is><t>Bolt</t></is></c><c r="B2"><v>2.5</v></c></row>
        </sheetData></worksheet>"#;
        let sheet2 = r#"<worksheet><sheetData>
            <row r="1"><c r="A1" t="str"><v>total &amp; tax</v></c></row>
        </sheetData></worksheet>"#;
        let bytes = build_zip(&[
            ("xl/workbook.xml", WORKBOOK),
            ("xl/_rels/workbook.xml.rels", RELS),
            ("xl/sharedStrings.xml", SHARED),
            ("xl/worksheets/sheet1.xml", sheet1),
            ("xl/worksheets/sheet2.xml", sheet2),
        ]);
        let text = extract_xlsx_text_from_bytes(&bytes).unwrap();
        assert_eq!(
            text,
            "--- Sheet: Budget ---\nItem\tUnit price\nBolt\t2.5\n\n--- Sheet: Notes ---\ntotal & tax"
        );
    }

    #[test]
    fn sparse_rows_keep_their_columns() {
        // C1 holds a value while A1/B1 are absent: the row pads to column C.
        let sheet = r#"<worksheet><sheetData>
            <row r="1"><c r="C1"><v>9</v></c></row>
        </sheetData></worksheet>"#;
        let bytes = build_zip(&[("xl/worksheets/sheet1.xml", sheet)]);
        let text = extract_xlsx_text_from_bytes(&bytes).unwrap();
        assert_eq!(text, "--- Sheet: Sheet 1 ---\n\t\t9");
    }

    #[test]
    fn a_missing_relationship_file_falls_back_to_numeric_order() {
        let sheet = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#;
        let bytes = build_zip(&[
            ("xl/workbook.xml", WORKBOOK),
            ("xl/worksheets/sheet2.xml", sheet),
            ("xl/worksheets/sheet10.xml", sheet),
        ]);
        let text = extract_xlsx_text_from_bytes(&bytes).unwrap();
        assert_eq!(
            text,
            "--- Sheet: Sheet 2 ---\n1\n\n--- Sheet: Sheet 10 ---\n1"
        );
    }

    #[test]
    fn an_out_of_range_shared_string_reads_as_nothing() {
        let sheet = r#"<worksheet><sheetData><row r="1"><c r="A1" t="s"><v>99</v></c><c r="B1"><v>7</v></c></row></sheetData></worksheet>"#;
        let bytes = build_zip(&[
            ("xl/sharedStrings.xml", SHARED),
            ("xl/worksheets/sheet1.xml", sheet),
        ]);
        let text = extract_xlsx_text_from_bytes(&bytes).unwrap();
        assert_eq!(text, "--- Sheet: Sheet 1 ---\n\t7");
    }

    #[test]
    fn column_references_resolve_beyond_z() {
        assert_eq!(column_index("A1"), Some(0));
        assert_eq!(column_index("Z9"), Some(25));
        assert_eq!(column_index("AA10"), Some(26));
        assert_eq!(column_index("AB1"), Some(27));
        assert_eq!(column_index("1"), None);
    }

    #[test]
    fn not_a_zip_is_an_error() {
        let err = extract_xlsx_text_from_bytes(b"plainly not a zip").unwrap_err();
        assert!(err.contains("Failed to open XLSX archive"), "{err}");
    }

    #[test]
    fn zip_without_worksheets_is_an_error() {
        let bytes = build_zip(&[("[Content_Types].xml", "<Types/>")]);
        let err = extract_xlsx_text_from_bytes(&bytes).unwrap_err();
        assert_eq!(err, "No worksheets found in XLSX");
    }
}
