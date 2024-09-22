//! Support for translating links to the standard library.

use mdbook::book::Book;
use mdbook::book::Chapter;
use mdbook::BookItem;
use once_cell::sync::Lazy;
use pulldown_cmark::{BrokenLink, CowStr, Event, LinkType, Options, Parser, Tag};
use regex::Regex;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::ops::Range;
use std::path::PathBuf;
use std::process::{self, Command};
use tempfile::TempDir;

/// The Regex used to extract the std links from the HTML generated by rustdoc.
static STD_LINK_EXTRACT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"<li>LINK: (.*)</li>"#).unwrap());

/// The Regex used to extract the URL from an HTML link.
static ANCHOR_URL: Lazy<Regex> = Lazy::new(|| Regex::new("<a href=\"([^\"]+)\"").unwrap());

/// Regex for a markdown inline link, like `[foo](bar)`.
static MD_LINK_INLINE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)(\[.+\])(\(.+\))").unwrap());
/// Regex for a markdown reference link, like `[foo][bar]`.
static MD_LINK_REFERENCE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)(\[.+\])(\[.*\])").unwrap());
/// Regex for a markdown shortcut link, like `[foo]`.
static MD_LINK_SHORTCUT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)(\[.+\])").unwrap());

/// Converts links to the standard library to the online documentation in a
/// fashion similar to rustdoc intra-doc links.
pub fn std_links(book: &mut Book) {
    // Collect all links in all chapters.
    let mut chapter_links = HashMap::new();
    for item in book.iter() {
        let BookItem::Chapter(ch) = item else {
            continue;
        };
        if ch.is_draft_chapter() {
            continue;
        }
        let key = ch.source_path.as_ref().unwrap();
        chapter_links.insert(key, collect_markdown_links(&ch));
    }
    // Write a Rust source file to use with rustdoc to generate intra-doc links.
    let tmp = TempDir::with_prefix("mdbook-spec-").unwrap();
    run_rustdoc(&tmp, &chapter_links);

    // Extract the links from the generated html.
    let generated =
        fs::read_to_string(tmp.path().join("doc/a/index.html")).expect("index.html generated");
    let mut urls: Vec<_> = STD_LINK_EXTRACT_RE
        .captures_iter(&generated)
        .map(|cap| cap.get(1).unwrap().as_str())
        .collect();
    let mut urls = &mut urls[..];
    let expected_len: usize = chapter_links.values().map(|l| l.len()).sum();
    if urls.len() != expected_len {
        eprintln!(
            "error: expected rustdoc to generate {} links, but found {}",
            expected_len,
            urls.len(),
        );
        process::exit(1);
    }
    // Unflatten the urls list so that it is split back by chapter.
    let mut ch_urls: HashMap<&PathBuf, Vec<_>> = HashMap::new();
    for (ch_path, links) in &chapter_links {
        let xs;
        (xs, urls) = urls.split_at_mut(links.len());
        ch_urls.insert(ch_path, xs.into());
    }

    // Do this in two passes to deal with lifetimes.
    let mut ch_contents = HashMap::new();
    for item in book.iter() {
        let BookItem::Chapter(ch) = item else {
            continue;
        };
        if ch.is_draft_chapter() {
            continue;
        }
        let key = ch.source_path.as_ref().unwrap();
        // Create a list of replacements to make in the raw markdown to point to the new url.
        let replacements = compute_replacements(&ch.content, &chapter_links[key], &ch_urls[key]);

        let mut new_contents = ch.content.clone();
        for (md_link, url, range) in replacements {
            // Convert links to be relative so that links work offline and
            // with the linkchecker.
            let url = relative_url(url, ch);
            // Note that this may orphan reference link definitions. This should
            // probably remove them, but pulldown_cmark doesn't give the span for
            // the reference definition.
            new_contents.replace_range(range, &format!("{md_link}({url})"));
        }
        ch_contents.insert(key.clone(), new_contents);
    }

    // Replace the content with the new content.
    book.for_each_mut(|item| {
        let BookItem::Chapter(ch) = item else {
            return;
        };
        if ch.is_draft_chapter() {
            return;
        }
        let key = ch.source_path.as_ref().unwrap();
        let content = ch_contents.remove(key).unwrap();
        ch.content = content;
    });
}

#[derive(Debug)]
struct Link<'a> {
    link_type: LinkType,
    /// Where the link is going to, for example `std::ffi::OsString`.
    dest_url: CowStr<'a>,
    /// The span in the original markdown where the link is located.
    ///
    /// Note that during translation, all links will be converted to inline
    /// links. That means that for reference-style links, the link reference
    /// definition will end up being ignored in the final markdown. For
    /// example, a link like ``[`OsString`]`` with a definition
    /// ``[`OsString`]: std::ffi::OsString`` will convert the link to
    /// ``[`OsString`](https://doc.rust-lang.org/std/ffi/struct.OsString.html)`.
    range: Range<usize>,
}

/// Collects all markdown links that look like they might be standard library links.
fn collect_markdown_links(chapter: &Chapter) -> Vec<Link<'_>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_HEADING_ATTRIBUTES);
    opts.insert(Options::ENABLE_SMART_PUNCTUATION);

    let mut broken_links = Vec::new();
    let mut links = Vec::new();

    // Broken links are collected so that you can write something like
    // `[std::option::Option]` which in pulldown_cmark's eyes is a broken
    // link. However, that is the normal syntax for rustdoc.
    let broken_link = |broken_link: BrokenLink<'_>| {
        broken_links.push(Link {
            link_type: broken_link.link_type,
            // Necessary due to lifetime issues.
            dest_url: CowStr::Boxed(broken_link.reference.into_string().into()),
            range: broken_link.span.clone(),
        });
        None
    };

    let parser = Parser::new_with_broken_link_callback(&chapter.content, opts, Some(broken_link))
        .into_offset_iter();
    for (event, range) in parser {
        match event {
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id: _,
            }) => {
                // Only collect links that are for the standard library.
                if matches!(link_type, LinkType::Autolink | LinkType::Email) {
                    continue;
                }
                if dest_url.starts_with("http")
                    || dest_url.contains(".md")
                    || dest_url.contains(".html")
                    || dest_url.starts_with('#')
                {
                    continue;
                }
                if !title.is_empty() {
                    eprintln!(
                        "error: titles in links are not supported\n\
                         Link {dest_url} has title `{title}` found in chapter {} ({:?})",
                        chapter.name,
                        chapter.source_path.as_ref().unwrap()
                    );
                    process::exit(1);
                }
                links.push(Link {
                    link_type,
                    dest_url,
                    range: range.clone(),
                });
            }
            _ => {}
        }
    }
    links.extend(broken_links);
    links
}

/// Generates links using rustdoc.
///
/// This takes the given links and creates a temporary Rust source file
/// containing those links within doc-comments, and then runs rustdoc to
/// generate intra-doc links on them.
///
/// The output will be in the given `tmp` directory.
fn run_rustdoc(tmp: &TempDir, chapter_links: &HashMap<&PathBuf, Vec<Link<'_>>>) {
    let src_path = tmp.path().join("a.rs");
    // Allow redundant since there could some in-scope things that are
    // technically not necessary, but we don't care about (like
    // [`Option`](std::option::Option)).
    let mut src = format!(
        "#![deny(rustdoc::broken_intra_doc_links)]\n\
         #![allow(rustdoc::redundant_explicit_links)]\n"
    );
    // This uses a list to make easy to pull the links out of the generated HTML.
    for (_ch_path, links) in chapter_links {
        for link in links {
            match link.link_type {
                LinkType::Inline
                | LinkType::Reference
                | LinkType::Collapsed
                | LinkType::Shortcut => {
                    writeln!(src, "//! - LINK: [{}]", link.dest_url).unwrap();
                }
                LinkType::ReferenceUnknown
                | LinkType::CollapsedUnknown
                | LinkType::ShortcutUnknown => {
                    // These should only happen due to broken link replacements.
                    panic!("unexpected link type unknown {link:?}");
                }
                LinkType::Autolink | LinkType::Email => {
                    panic!("link type should have been filtered {link:?}");
                }
            }
        }
    }
    // Put some common things into scope so that links to them work.
    writeln!(
        src,
        "extern crate alloc;\n\
         extern crate proc_macro;\n\
         extern crate test;\n"
    )
    .unwrap();
    fs::write(&src_path, &src).unwrap();
    let rustdoc = std::env::var("RUSTDOC").unwrap_or_else(|_| "rustdoc".into());
    let output = Command::new(rustdoc)
        .arg("--edition=2021")
        .arg(&src_path)
        .current_dir(tmp.path())
        .output()
        .expect("rustdoc installed");
    if !output.status.success() {
        eprintln!("error: failed to extract std links ({:?})\n", output.status,);
        io::stderr().write_all(&output.stderr).unwrap();
        process::exit(1);
    }
}

static DOC_URL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^https://doc.rust-lang.org/(?:nightly|beta|stable|dev|1\.[0-9]+\.[0-9]+)").unwrap()
});

/// Converts a URL to doc.rust-lang.org to be relative.
fn relative_url(url: &str, chapter: &Chapter) -> String {
    // Set SPEC_RELATIVE=0 to disable this, which can be useful for working locally.
    if std::env::var("SPEC_RELATIVE").as_deref() != Ok("0") {
        let Some(url_start) = DOC_URL.shortest_match(url) else {
            eprintln!("error: expected rustdoc URL to start with {DOC_URL:?}, got {url}");
            std::process::exit(1);
        };
        let url_path = &url[url_start..];
        let num_dots = chapter.path.as_ref().unwrap().components().count();
        let dots = vec![".."; num_dots].join("/");
        format!("{dots}{url_path}")
    } else {
        url.to_string()
    }
}

/// Computes the replacements to make in the markdown content.
///
/// Returns a `Vec` of `(md_link, url, range)` where:
///
/// - `md_link` is the markdown link string to show to the user (like `[foo]`).
/// - `url` is the URL to the standard library.
/// - `range` is the range in the original markdown to replace with the new link.
fn compute_replacements<'a>(
    contents: &'a str,
    links: &[Link<'_>],
    urls: &[&'a str],
) -> Vec<(&'a str, &'a str, Range<usize>)> {
    let mut replacements = Vec::new();

    for (url, link) in urls.iter().zip(links) {
        let Some(cap) = ANCHOR_URL.captures(url) else {
            eprintln!("error: could not find anchor in:\n{url}\nlink={link:#?}");
            process::exit(1);
        };
        let url = cap.get(1).unwrap().as_str();
        let md_link = &contents[link.range.clone()];

        let range = link.range.clone();
        let add_link = |re: &Regex| {
            let Some(cap) = re.captures(md_link) else {
                eprintln!(
                    "error: expected link `{md_link}` of type {:?} to match regex {re}",
                    link.link_type
                );
                process::exit(1);
            };
            let md_link = cap.get(1).unwrap().as_str();
            replacements.push((md_link, url, range));
        };

        match link.link_type {
            LinkType::Inline => {
                add_link(&MD_LINK_INLINE);
            }
            LinkType::Reference | LinkType::Collapsed => {
                add_link(&MD_LINK_REFERENCE);
            }
            LinkType::Shortcut => {
                add_link(&MD_LINK_SHORTCUT);
            }
            _ => {
                panic!("unexpected link type: {link:#?}");
            }
        }
    }
    // Sort and reverse (so that it can replace bottom-up so ranges don't shift).
    replacements.sort_by(|a, b| b.2.clone().partial_cmp(a.2.clone()).unwrap());
    replacements
}