// Copyright (C) 2026  A Farzat
// This program is free software: you can redistribute it and/or modify it under the terms of the
// GNU General Public License, GPLv3, attached at the root of the project.

use anyhow::{Context, Result};
use ogrim::{Document, xml};
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer};
use relative_path::{RelativePath, RelativePathBuf};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use url::Url;

use crate::models::{Chapter, EpubResponse, FileEntry};

/// Checks if a tag is a standard HTML void element that shouldn't have a closing tag.
fn is_html_void_tag(name: &[u8]) -> bool {
    matches!(
        name,
        b"area"
            | b"base"
            | b"br"
            | b"col"
            | b"embed"
            | b"hr"
            | b"img"
            | b"input"
            | b"link"
            | b"meta"
            | b"param"
            | b"source"
            | b"track"
            | b"wbr"
    )
}

/// Processes the fragment and outputs a complete, EPUB-ready XHTML document.
pub fn build_epub_chapter<R: BufRead, W: Write>(
    epub_data: &EpubResponse,
    chapter: &Chapter,
    chapter_dir: &RelativePath,
    fragment_input: R,
    url_to_file: &HashMap<&Url, &FileEntry>,
    url_path_to_local: &HashMap<&str, &RelativePathBuf>,
    mut out: &mut W,
) -> Result<()> {
    // EPUB XHTML Boilerplate wrapper.
    // EPUBs strictly require the w3 and idpf namespaces to validate properly.
    let wrapper_xhtml = xml!(
        <?xml version="1.0" encoding="UTF-8"?>
        <html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"
            lang={epub_data.language} xml:lang={epub_data.language}>
        <head>
            <title>{chapter.title}</title>
            {|doc| make_stylesheet_links(doc, chapter, chapter_dir, url_to_file)}
        </head>
        <body>
        </body>
        </html>
    );
    let wrapper_suffix = "</body></html>";
    let wrapper_prefix = wrapper_xhtml
        .as_str()
        .strip_suffix(wrapper_suffix)
        .context("Wrapper must end with </body></html>")?;

    // Write wrapper prefix to output first.
    out.write_all(wrapper_prefix.as_bytes())?;
    out.write_all(b"\n")?;

    // Setup the XML Reader and Writer.
    let mut reader = Reader::from_reader(fragment_input);
    // Preserve spacing for EPUB text formatting.
    reader.config_mut().trim_text(false);
    // Fragments could have unmatched tags - tell the parser not to panic if so.
    reader.config_mut().check_end_names = false;
    let mut writer = Writer::new(&mut out);

    // Loop through the XML events and rewrite tags.
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(tag_data)) => {
                // If it is a void tag, convert it to a self-closing XML tag.
                let tag_type = if is_html_void_tag(tag_data.name().as_ref()) {
                    Event::Empty
                } else {
                    Event::Start
                };
                writer.write_event(tag_type(rewrite_attributes(
                    &tag_data,
                    url_path_to_local,
                    chapter_dir,
                )?))?;
            }
            Ok(Event::Empty(tag_data)) => {
                // If tags are already empty, leave them as-is.
                writer.write_event(Event::Empty(rewrite_attributes(
                    &tag_data,
                    url_path_to_local,
                    chapter_dir,
                )?))?;
            }
            Ok(Event::End(tag_data)) => {
                // Silently drop closing tags for void elements if they exist (e.g. <img></img>).
                if !is_html_void_tag(tag_data.name().as_ref()) {
                    writer.write_event(Event::End(tag_data))?;
                }
            }
            Ok(Event::Eof) => break,
            Ok(tag_data) => writer.write_event(tag_data)?, // Pass through text, comments, etc. unmodified.
            Err(e) => anyhow::bail!(e),
        }
    }

    // Finish by flushing wrapper suffix to output.
    out.write_all(b"\n")?;
    out.write_all(wrapper_suffix.as_bytes())?;

    Ok(())
}

/// Processes the fragment and outputs a complete, EPUB-ready XHTML document.
pub fn write_modified_opf<R: BufRead, W: Write>(
    opf_input: R,
    mut out: &mut W,
    description: &str,
) -> Result<()> {
    // Setup the XML Reader and Writer.
    let mut reader = Reader::from_reader(opf_input);
    // Preserve spacing for easier diffs.
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(&mut out);

    // Loop through the XML events and check tags.
    let mut buffer = Vec::new();
    let mut desc_found = false;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(tag_data)) => {
                // Simply record if dc:description found.
                if tag_data.name().as_ref() == b"dc:description" {
                    desc_found = true;
                }
                // Then pass through unmodified.
                writer.write_event(Event::Start(tag_data))?;
            }
            Ok(Event::End(tag_data)) => {
                // Write description if end of metadata is reached without finding one.
                if tag_data.name().as_ref() == b"metadata" && !desc_found {
                    writer.write_event(Event::Start(BytesStart::new("dc:description")))?;
                    writer.write_event(Event::Text(BytesText::new(description)))?;
                    writer.write_event(Event::End(BytesEnd::new("dc:description")))?;
                }
                // Pass through unmodified.
                writer.write_event(Event::End(tag_data))?;
            }
            Ok(Event::Eof) => break,
            Ok(tag_data) => writer.write_event(tag_data)?, // Pass through text, comments, etc. unmodified.
            Err(e) => anyhow::bail!(e),
        }
    }

    Ok(())
}

/// Helper function add link elements for stylesheets to an xml Document.
fn make_stylesheet_links(
    doc: &mut Document,
    chapter: &Chapter,
    chapter_dir: &RelativePath,
    url_to_file: &HashMap<&Url, &FileEntry>,
) {
    chapter
        .related_assets
        .stylesheets
        .iter()
        .filter_map(|u| url_to_file.get(u))
        .for_each(|e| {
            let rel_path = chapter_dir.relative(&e.full_path);
            xml!(doc,
                <link rel="stylesheet" type={e.media_type} href={rel_path} />
            );
        })
}

/// Helper function to inspect tags and rewrite the elements' attributes.
fn rewrite_attributes<'a>(
    tag_data: &BytesStart<'a>,
    url_path_to_local: &HashMap<&str, &RelativePathBuf>,
    chapter_dir: &RelativePath,
) -> Result<BytesStart<'static>> {
    let name = String::from_utf8_lossy(tag_data.name().as_ref()).into_owned();
    let mut new_elem = BytesStart::new(name);

    for attr in tag_data.attributes() {
        let attr = attr?;
        let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
        // Parsed attributes contain the raw escaped bytes from the source. Decode them
        // before inserting them into a new element so quick-xml can safely escape the
        // value for the double-quoted attribute it writes.
        let value = attr.unescape_value()?.into_owned();

        // Intercept element attributes which could contain links to resources.
        if matches!(
            (tag_data.name().as_ref(), key.as_bytes()),
            (b"img", b"src")
                | (b"source", b"src")
                | (b"video", b"src")
                | (b"audio", b"src")
                | (b"a", b"href")
                | (b"link", b"href")
                | (b"object", b"data")
                | (b"embed", b"src")
                | (b"iframe", b"src")
                | (b"track", b"src")
        ) {
            // If we have a local path, inject it instead of the absolute URL.
            if let Some(local_path) = url_path_to_local.get(value.as_str()) {
                new_elem.push_attribute((key.as_ref(), chapter_dir.relative(local_path).as_str()));
                continue;
            }
        }

        // Re-serialize every value instead of copying the raw attribute. In particular,
        // this turns quotes nested in a single-quoted source value into &quot;.
        new_elem.push_attribute((key.as_str(), value.as_str()));
    }

    Ok(new_elem)
}

#[cfg(test)]
mod tests {
    use super::rewrite_attributes;
    use quick_xml::events::Event;
    use quick_xml::{Reader, Writer};
    use relative_path::{RelativePath, RelativePathBuf};
    use std::collections::HashMap;

    #[test]
    fn reescapes_quotes_nested_in_single_quoted_attribute() {
        let input = r#"<img alt='Figure (&lt;span class="equation"&gt;&lt;img src="images/missing.svg" /&gt;&lt;/span&gt;)' src="images/main.png" />"#;
        let mut reader = Reader::from_str(input);
        let tag = match reader.read_event().unwrap() {
            Event::Empty(tag) => tag,
            event => panic!("expected an empty element, got {event:?}"),
        };
        let paths: HashMap<&str, &RelativePathBuf> = HashMap::new();
        let rewritten = rewrite_attributes(&tag, &paths, RelativePath::new("")).unwrap();

        let mut output = Vec::new();
        Writer::new(&mut output)
            .write_event(Event::Empty(rewritten))
            .unwrap();
        let output = String::from_utf8(output).unwrap();

        assert!(output.contains("class=&quot;equation&quot;"));
        assert!(output.contains("src=&quot;images/missing.svg&quot;"));

        // The rewritten element must remain valid XML and preserve the decoded alt text.
        let mut verifier = Reader::from_str(&output);
        let verified_tag = match verifier.read_event().unwrap() {
            Event::Empty(tag) => tag,
            event => panic!("expected an empty element, got {event:?}"),
        };
        let alt = verified_tag
            .try_get_attribute("alt")
            .unwrap()
            .unwrap()
            .unescape_value()
            .unwrap();
        assert_eq!(
            alt,
            r#"Figure (<span class="equation"><img src="images/missing.svg" /></span>)"#
        );
    }
}
