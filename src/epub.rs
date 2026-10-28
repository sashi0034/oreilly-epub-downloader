// Copyright (C) 2026  A Farzat
// This program is free software: you can redistribute it and/or modify it under the terms of the
// GNU General Public License, GPLv3, attached at the root of the project.

use crate::{
    models::{Chapter, EpubResponse, FileEntry},
    xml::{build_epub_chapter, write_modified_opf},
};
use anyhow::{Context, Result, anyhow};
use futures_util::{StreamExt, TryStreamExt, stream::FuturesUnordered};
use ogrim::xml;
use relative_path::{RelativePath, RelativePathBuf};
use reqwest::{Client, StatusCode, header::RETRY_AFTER};
use std::{
    borrow::Cow,
    collections::HashMap,
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::fs::{self, File};
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep};
use tokio_util::io::StreamReader;
use zip::{CompressionMethod, ZipWriter, write::FileOptions};

/// Creates and writes container.xml.
fn write_container_xml<W: Write>(out: &mut W, opf_full_path: &RelativePathBuf) -> Result<()> {
    // Prepare file contents.
    let contents = xml!(
        <?xml version="1.0" encoding="UTF-8"?>
        <container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
          <rootfiles>
            <rootfile full-path={opf_full_path} media-type="application/oebps-package+xml"/>
          </rootfiles>
        </container>
    );

    // Write down the file.
    out.write_all(contents.as_str().as_bytes())?;
    Ok(())
}

const MIN_REQUEST_DELAY_MS: u64 = 500;
const MAX_REQUEST_DELAY_MS: u64 = 1_000;
const PROGRESS_BAR_WIDTH: usize = 24;

struct DownloadOutcome {
    full_path: RelativePathBuf,
    reused: bool,
}

struct RequestPacerState {
    next_request: Instant,
    random_state: u64,
}

/// Spaces request starts across all download workers using a randomized delay.
struct RequestPacer {
    state: Mutex<RequestPacerState>,
}

impl RequestPacer {
    fn new() -> Self {
        let random_state = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9_7f4a_7c15)
            .max(1);
        Self {
            state: Mutex::new(RequestPacerState {
                next_request: Instant::now(),
                random_state,
            }),
        }
    }

    async fn wait(&self) {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        if state.next_request > now {
            sleep(state.next_request - now).await;
        }

        // xorshift64 provides inexpensive jitter without another dependency.
        let mut value = state.random_state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        state.random_state = value;
        let range = MAX_REQUEST_DELAY_MS - MIN_REQUEST_DELAY_MS + 1;
        let delay_ms = MIN_REQUEST_DELAY_MS + value % range;
        state.next_request = Instant::now() + Duration::from_millis(delay_ms);
    }
}

fn retry_delay(attempt: u32, retry_after: Option<&reqwest::header::HeaderValue>) -> Duration {
    if let Some(seconds) = retry_after
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_secs(seconds.min(1800));
    }

    Duration::from_secs(15_u64.saturating_mul(1_u64 << attempt.min(4)))
}

fn print_download_progress(completed: usize, total: usize, outcome: &DownloadOutcome) {
    let filled = if total == 0 {
        0
    } else {
        completed * PROGRESS_BAR_WIDTH / total
    };
    let percentage = if total == 0 {
        100.0
    } else {
        completed as f64 * 100.0 / total as f64
    };
    let action = if outcome.reused {
        "Reused"
    } else {
        "Downloaded"
    };
    println!(
        "[{}{}] {}/{} ({:5.1}%) {}: {}",
        "#".repeat(filled),
        "-".repeat(PROGRESS_BAR_WIDTH - filled),
        completed,
        total,
        percentage,
        action,
        outcome.full_path
    );
}

fn is_preview_content(contents: &str) -> bool {
    let identifies_preview = contents.contains("content preview from")
        || contents.contains("unlock full access")
        || contents.contains("read now unlock full access");
    let has_membership_prompt = contents.contains("become an o’reilly member")
        || contents.contains("become an o'reilly member")
        || contents.contains("start your free trial");
    identifies_preview && has_membership_prompt
}

fn is_truncated_preview_document(contents: &str) -> bool {
    if !contents.contains("sbo-rt-content") || contents.len() >= 12_000 {
        return false;
    }
    let bytes = contents.as_bytes();
    let tail = &bytes[bytes.len().saturating_sub(1_024)..];
    tail.windows(4).any(|window| window == b" ...")
}

fn strip_injected_security_overlay(contents: &str) -> Cow<'_, str> {
    let overlay = contents
        .find("id=\"sec-overlay\"")
        .or_else(|| contents.find("id='sec-overlay'"));
    let Some(overlay) = overlay else {
        return Cow::Borrowed(contents);
    };
    let Some(nav_end) = contents[..overlay].rfind("</nav>") else {
        return Cow::Borrowed(contents);
    };
    let nav_end = nav_end + "</nav>".len();
    let Some(body_end) = contents[overlay..].find("</body>") else {
        return Cow::Borrowed(contents);
    };
    let body_end = overlay + body_end;

    let mut cleaned = String::with_capacity(contents.len());
    cleaned.push_str(&contents[..nav_end]);
    cleaned.push_str(&contents[body_end..]);
    Cow::Owned(cleaned)
}

fn reject_preview_content(file_entries: &[FileEntry], epub_root: &Path) -> Result<()> {
    let mut content_paths = Vec::new();
    let mut truncated_count = 0;
    let mut marketing_preview = false;

    for entry in file_entries.iter().filter(|entry| {
        entry.media_type == "application/xhtml+xml" || entry.media_type == "text/html"
    }) {
        let path = entry.full_path.to_path(epub_root);
        let contents = std::fs::read(&path)
            .with_context(|| format!("Failed to inspect downloaded content at {path:?}"))?;
        let contents = String::from_utf8_lossy(&contents).to_lowercase();

        if contents.contains("sbo-rt-content") {
            content_paths.push(path);
            if is_truncated_preview_document(&contents) {
                truncated_count += 1;
            }
        }
        if is_preview_content(&contents) {
            marketing_preview = true;
        }
    }

    let repeatedly_truncated = content_paths.len() >= 5
        && truncated_count >= 3
        && truncated_count * 2 >= content_paths.len();
    if marketing_preview || repeatedly_truncated {
        // Do not let cached preview chapters survive into a later authenticated run.
        for path in &content_paths {
            std::fs::remove_file(path).with_context(|| {
                format!("Detected preview content but failed to remove {path:?}")
            })?;
        }
        let reason = if repeatedly_truncated {
            format!(
                "{} of {} chapter files were truncated",
                truncated_count,
                content_paths.len()
            )
        } else {
            "an O'Reilly preview access prompt was found".to_string()
        };
        return Err(anyhow!(
            "Detected O'Reilly preview-only content ({reason}). Cached chapter files were removed; authenticate and run the download again"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_preview_content, strip_injected_security_overlay};
    use std::borrow::Cow;

    #[test]
    fn detects_oreilly_preview_prompt() {
        let contents =
            "Content preview from Example. Become an O’Reilly member and unlock full access.";
        assert!(is_preview_content(&contents.to_lowercase()));
    }

    #[test]
    fn does_not_reject_a_book_that_only_mentions_a_free_trial() {
        let contents = "This product offers a start your free trial button.";
        assert!(!is_preview_content(contents));
    }

    #[test]
    fn detects_a_short_chapter_cut_off_with_three_dots() {
        let contents = r#"<div id="sbo-rt-content"><p>The chapter starts here ...</p></div>"#;
        assert!(super::is_truncated_preview_document(contents));
    }

    #[test]
    fn does_not_treat_a_long_chapter_as_a_truncated_preview() {
        let contents = format!(
            r#"<div id="sbo-rt-content"><p>{} ...</p></div>"#,
            "complete chapter text ".repeat(1_000)
        );
        assert!(!super::is_truncated_preview_document(&contents));
    }

    #[test]
    fn strips_injected_security_overlay_after_navigation() {
        let contents = r#"<html><body><nav><a href="chapter.html">Chapter</a></nav><script src="/security.js"></script><link rel="stylesheet" href="/security.css"><script async defer src="/challenge.js"></script><div id="sec-overlay"><div id="sec-container"></div></div><script defer src="/overlay.js"></script></body></html>"#;
        let cleaned = strip_injected_security_overlay(contents);

        assert_eq!(
            cleaned,
            r#"<html><body><nav><a href="chapter.html">Chapter</a></nav></body></html>"#
        );
    }

    #[test]
    fn leaves_normal_navigation_content_unchanged() {
        let contents = r#"<html><body><nav>Contents</nav><p>Afterword</p></body></html>"#;
        assert!(matches!(
            strip_injected_security_overlay(contents),
            Cow::Borrowed(_)
        ));
    }
}

/// Downloads files in parallel to the relative location specified in full_path.
pub async fn download_all_files(
    client: &Client,
    file_entries: &[FileEntry],
    dest_root: &Path,
    max_concurrent: usize,
    max_retries: u32,
) -> Result<()> {
    let mut downloading = FuturesUnordered::new();
    let mut files_iter = file_entries.iter();
    let pacer = Arc::new(RequestPacer::new());
    let total = file_entries.len();
    let mut completed = 0;

    // Start downloading the first n files.
    for entry in files_iter.by_ref().take(max_concurrent) {
        downloading.push(download_one_file(
            client,
            entry,
            dest_root,
            Arc::clone(&pacer),
            max_retries,
        ));
    }

    // Obtain completed files from the list as they finish downloading, until empty.
    while let Some(result) = downloading.next().await {
        // Make sure they didn't fail first. Propagate any errors immediately.
        let outcome = result?;
        completed += 1;
        print_download_progress(completed, total, &outcome);
        // Refill the slot (if there are any remaining).
        if let Some(entry) = files_iter.next() {
            downloading.push(download_one_file(
                client,
                entry,
                dest_root,
                Arc::clone(&pacer),
                max_retries,
            ));
        }
    }

    Ok(())
}

/// Downloads the given file to the relative location specified in full_path.
async fn download_one_file(
    client: &Client,
    file_entry: &FileEntry,
    dest_root: &Path,
    pacer: Arc<RequestPacer>,
    max_retries: u32,
) -> Result<DownloadOutcome> {
    let dest_path = file_entry.full_path.to_path(dest_root);

    // Completed files survive a failed run, so a retry can continue where it stopped.
    if fs::try_exists(&dest_path).await? {
        if fs::metadata(&dest_path).await?.len() > 0 {
            return Ok(DownloadOutcome {
                full_path: file_entry.full_path.clone(),
                reused: true,
            });
        }
        // The previous downloader created an empty destination before checking
        // an HTTP error, so do not mistake a rate-limited file for a completed one.
        fs::remove_file(&dest_path).await?;
    }

    // Ensure the directory exists. A .part file is only renamed after a complete response.
    if let Some(parent_dir) = dest_path.parent() {
        fs::create_dir_all(parent_dir).await?;
    }
    let mut part_name = dest_path.as_os_str().to_os_string();
    part_name.push(".part");
    let part_path = PathBuf::from(part_name);

    for attempt in 0..=max_retries {
        pacer.wait().await;
        let response = match client.get(file_entry.url.clone()).send().await {
            Ok(response) => response,
            Err(error) if attempt < max_retries => {
                let delay = retry_delay(attempt, None);
                eprintln!(
                    "Temporary network error for {}: {}. Retrying in {} seconds ({}/{})...",
                    file_entry.full_path,
                    error,
                    delay.as_secs(),
                    attempt + 1,
                    max_retries
                );
                sleep(delay).await;
                continue;
            }
            Err(error) => return Err(error.into()),
        };

        let status = response.status();
        let retryable = status == StatusCode::TOO_MANY_REQUESTS
            || status == StatusCode::FORBIDDEN
            || status.is_server_error();
        if retryable && attempt < max_retries {
            let delay = retry_delay(attempt, response.headers().get(RETRY_AFTER));
            eprintln!(
                "Server returned {} for {}. Retrying in {} seconds ({}/{})...",
                status,
                file_entry.full_path,
                delay.as_secs(),
                attempt + 1,
                max_retries
            );
            sleep(delay).await;
            continue;
        }
        let response = response
            .error_for_status()
            .with_context(|| format!("Failed to download {}", file_entry.url))?;

        let mut file = File::create(&part_path).await?;
        let bytes_stream = response.bytes_stream();
        let mut reader = StreamReader::new(bytes_stream.map_err(std::io::Error::other));
        match tokio::io::copy(&mut reader, &mut file).await {
            Ok(_) => {
                drop(file);
                fs::rename(&part_path, &dest_path).await?;
                return Ok(DownloadOutcome {
                    full_path: file_entry.full_path.clone(),
                    reused: false,
                });
            }
            Err(error) if attempt < max_retries => {
                let delay = retry_delay(attempt, None);
                eprintln!(
                    "Interrupted download for {}: {}. Retrying in {} seconds ({}/{})...",
                    file_entry.full_path,
                    error,
                    delay.as_secs(),
                    attempt + 1,
                    max_retries
                );
                sleep(delay).await;
            }
            Err(error) => return Err(error.into()),
        }
    }

    Err(anyhow!("Download retries exhausted for {}", file_entry.url))
}

/// Creates the EPUB archive (creates zip and includes all files in it).
pub fn create_epub_archive(
    epub_data: &EpubResponse,
    epub_root: &Path,
    output_epub: &Path,
    file_entries: &[FileEntry],
    chapters: &HashMap<String, Chapter>,
) -> Result<()> {
    reject_preview_content(file_entries, epub_root)?;

    if let Some(parent_dir) = output_epub.parent() {
        std::fs::create_dir_all(parent_dir)?;
    }
    let out_file = std::fs::File::create(output_epub)?;
    let mut zip = ZipWriter::new(out_file);

    // Write mimetype to zip first. It must be uncompressed.
    let options: FileOptions<()> =
        FileOptions::default().compression_method(CompressionMethod::Stored);
    zip.start_file("mimetype", options)?;
    zip.write_all(b"application/epub+zip")?;

    // Find the OPF file entry to reference it in "container.xml".
    let opf_entry = file_entries
        .iter()
        .find(|f| f.filename_ext == ".opf" && f.media_type == "application/oebps-package+xml")
        .context("No OPF file with the correct MIME type was found.")?;
    // Write down the "container.xml" to zip.
    let options: FileOptions<()> =
        FileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("META-INF/container.xml", options)?;
    write_container_xml(&mut zip, &opf_entry.full_path)?;

    // Prepare url path to local path mapping to clean xhtml files from external dependencies.
    let url_path_to_local = file_entries
        .iter()
        .map(|e| (e.url.path(), &e.full_path))
        .collect::<HashMap<_, _>>();
    // Prepare url to local path mapping to insert related assets based on their URL.
    let url_to_file = file_entries
        .iter()
        .map(|e| (&e.url, e))
        .collect::<HashMap<_, _>>();

    // Add the rest of the files according to file_entries.
    // The `options` variable remains unchanged from "container.xml".
    for entry in file_entries {
        zip.start_file(&entry.full_path, options)?;
        let src_file = std::fs::File::open(entry.full_path.to_path(epub_root))?;
        let mut buf_reader = BufReader::new(src_file);
        if let Some(chapter) = chapters.get(&entry.ourn) {
            let chapter_dir = entry.full_path.parent().unwrap_or(RelativePath::new(""));
            build_epub_chapter(
                epub_data,
                chapter,
                chapter_dir,
                buf_reader,
                &url_to_file,
                &url_path_to_local,
                &mut zip,
            )?;
        } else if entry.ourn == opf_entry.ourn {
            write_modified_opf(buf_reader, &mut zip, &epub_data.descriptions.plain)?;
        } else if matches!(
            entry.media_type.as_str(),
            "text/html" | "application/xhtml+xml"
        ) {
            let mut contents = String::new();
            buf_reader.read_to_string(&mut contents)?;
            zip.write_all(strip_injected_security_overlay(&contents).as_bytes())?;
        } else {
            std::io::copy(&mut buf_reader, &mut zip)?;
        }
    }

    zip.finish()?;

    Ok(())
}
