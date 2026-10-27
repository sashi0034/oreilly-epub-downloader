// Copyright (C) 2026  A Farzat
// This program is free software: you can redistribute it and/or modify it under the terms of the
// GNU General Public License, GPLv3, attached at the root of the project.

mod epub;
mod http_client;
mod models;
mod xml;

use std::collections::HashMap;
use std::env::current_dir;
use std::path::PathBuf;

use crate::epub::{create_epub_archive, download_all_files};
use crate::http_client::build_authenticated_client;
use crate::models::{Chapter, EpubResponse, FileEntry, Paginated};
use anyhow::{Context, Result, anyhow};
use clap::{Parser, value_parser};
use directories::{BaseDirs, UserDirs};
use reqwest::Client;

/// Download and generate an EPUB from Safari Books Online.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The Book digits ID that you want to download.
    #[arg(required = true)]
    bookid: String,
    /// Path to the cookies.json file.
    #[arg(long)]
    cookies: Option<String>,
    /// Do not download files. Use if they were already downloaded in a previous run.
    #[arg(long = "skip-download")]
    skip_download: bool,
    /// Write the generated EPUB to this path.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Number of files to download in parallel. Limit is 8 (be polite).
    #[arg(long, value_parser=value_parser!(u32).range(1..=8), default_value_t = 1)]
    parallel: u32,
    /// Number of retries for rate limits and temporary server/network errors.
    #[arg(long, value_parser=value_parser!(u32).range(0..=20), default_value_t = 7)]
    retry_count: u32,
}

/// Fetches EPUB structural data (like the chapters URL).
async fn fetch_epub_data(client: &Client, bookid: &str) -> Result<EpubResponse> {
    let url = format!("https://learning.oreilly.com/api/v2/epubs/urn:orm:book:{bookid}/");
    let response = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json::<EpubResponse>()
        .await
        .context("Failed to deserialize EPUB API response")?;
    Ok(response)
}

/// Fetch a paginated API.
async fn fetch_all_pages<T>(client: &reqwest::Client, mut url: url::Url) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned,
{
    let mut items = Vec::new();
    loop {
        // GET current URL and deserialize into Paginated<T>.
        let response = client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json::<Paginated<T>>()
            .await
            .context("Failed to deserialize API response.")?;
        // Extend items with the page's results.
        items.extend(response.results);
        // Set url to next page if available, else break.
        if let Some(next) = response.next {
            url = next;
        } else {
            break;
        }
    }
    Ok(items)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Obtain relevant XDG base directories.
    let base_dirs = BaseDirs::new().context("Could not get XDG base directories.")?;
    let data_root = base_dirs.data_dir().join("oreilly-epub");
    let config_root = base_dirs.config_dir().join("oreilly-epub");

    // Parse the command line arguments
    let args = Args::parse();

    // Obtain the path to the destination EPUB file.
    let user_dirs = UserDirs::new().context("Could not get XDG user directories.")?;
    let fallback_download_dir = user_dirs.home_dir().join("Downloads");
    let epub_path = args.output.clone().unwrap_or_else(|| {
        user_dirs
            .download_dir()
            .unwrap_or(&fallback_download_dir)
            .join("oreilly-epub")
            .join(format!("{}.epub", args.bookid))
    });

    println!("Welcome to SafariBooks Rust Port!");
    println!("Target Book ID: {}", args.bookid);
    println!(
        "Download pacing: {} parallel request(s), 500-1000 ms apart, {} retries",
        args.parallel, args.retry_count
    );

    // Initialise the HTTP client.
    println!("Loading cookies and initialising the HTTP client...");
    let candidate_cookies_paths = [
        args.cookies.map(PathBuf::from),
        Some(config_root.join("cookies.json")),
        Some(current_dir()?.join("cookies.json")),
    ];
    let cookies_file = candidate_cookies_paths
        .into_iter()
        .flatten()
        .find(|path| path.exists())
        .ok_or_else(|| {
            anyhow!(
                "No cookies.json found. {}, {} {:?}.",
                "Either provide one through the --cookies option",
                "or create one in the current directory or at",
                config_root
            )
        })?;
    let client = build_authenticated_client(&cookies_file)?;

    println!("Fetching book metadata...");
    // Fetch from the EPUB API.
    let epub_data = fetch_epub_data(&client, &args.bookid).await?;
    println!("Publication date: {}", epub_data.publication_date);
    println!("Title: {}", epub_data.title);
    println!("Chapters URL: {}", epub_data.chapters);
    println!("Resources URL: {}", epub_data.files);
    println!("------------------\n");

    println!("Fetching book structure...");
    let chapters: Vec<Chapter> = fetch_all_pages(&client, epub_data.chapters.clone()).await?;
    let chapters: HashMap<String, Chapter> =
        chapters.into_iter().map(|c| (c.ourn.clone(), c)).collect();
    let file_entries: Vec<FileEntry> = fetch_all_pages(&client, epub_data.files.clone()).await?;

    let epub_root = data_root.join("files").join(&args.bookid);
    if !args.skip_download {
        println!(
            "Downloading {} files from the server...",
            file_entries.len()
        );
        download_all_files(
            &client,
            &file_entries,
            &epub_root,
            args.parallel.try_into()?, // Will work as 1..=8 will fit into any usize.
            args.retry_count,
        )
        .await?;
    }
    println!("Generating the EPUB file...");
    create_epub_archive(&epub_data, &epub_root, &epub_path, &file_entries, &chapters)?;

    Ok(())
}
