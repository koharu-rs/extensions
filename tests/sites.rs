//! Every site that reads without an account, read as Koharu reads it: its
//! catalogs and a search, each with a cover that's a picture, then a series
//! with a free chapter, that chapter, and its first page, which must be an
//! image. Sites change their pages
//! without notice, so this runs against them, not recordings; it's ignored
//! unless asked for, and saves each page it read under the target directory.

use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use futures::future::join_all;
use koharu_extensions::{EXTENSIONS, Extension, SeriesLink};
use url::Url;

/// Each site, by its extension's id, and a search that finds series there.
const SITES: &[(&str, &str)] = &[
    ("comic-walker", "恋"),
    ("naver-webtoon", "연애"),
    ("pixiv-comic", "恋"),
    ("webtoons", "love"),
    ("shonen-jump-plus", "の"),
    ("tonarinoyj", "の"),
    ("comic-days", "の"),
    ("sunday-webry", "の"),
    ("comic-action", "の"),
    ("comic-zenon", "の"),
    ("magcomi", "の"),
    ("kuragebunch", "の"),
    ("comic-gardo", "の"),
    ("comic-earthstar", "の"),
    ("comic-trail", "の"),
];

/// How many of a site's series to try for one with a free chapter.
const TRIES: usize = 8;
/// The narrowest a cover is: an image server answers a malformed request
/// with a sliver of the cover's average color, which still decodes.
const COVER: u32 = 64;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "reads every site over the network"]
async fn every_site_reads() {
    let http = reqwest::Client::builder()
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/140.0 Safari/537.36",
        )
        .build()
        .expect("a client");
    let pages = Path::new(env!("CARGO_TARGET_TMPDIR")).join("sites");
    std::fs::create_dir_all(&pages).expect("a directory for the pages");
    let (http, pages) = (&http, &pages);
    let results = join_all(
        SITES
            .iter()
            .map(|&(id, query)| async move { (id, read(http, id, query, pages).await) }),
    )
    .await;
    let mut broken = Vec::new();
    for (id, result) in results {
        match result {
            Ok(read) => println!("{id}: {read}"),
            Err(error) => {
                println!("{id}: FAILED {error:#}");
                broken.push(id);
            }
        }
    }
    assert!(broken.is_empty(), "sites that broke: {}", broken.join(", "));
}

/// Reads the site `id` through, saving the page it read to `pages`, and
/// says what it read.
async fn read(http: &reqwest::Client, id: &str, query: &str, pages: &Path) -> Result<String> {
    let site: &dyn Extension = EXTENSIONS
        .iter()
        .copied()
        .find(|site| site.id() == id)
        .context("no extension has this id")?;
    let mut said = Vec::new();
    let mut series = Vec::new();
    let found = site.search(http, query, None).await.context("search")?;
    ensure!(!found.series.is_empty(), "searching {query} found nothing");
    cover(http, site, &found.series).await.context("search")?;
    said.push(format!("search {}", found.series.len()));
    series.extend(found.series);
    for &catalog in site.catalogs() {
        let listing = site
            .catalog(http, catalog, None)
            .await
            .with_context(|| format!("catalog {catalog:?}"))?;
        ensure!(!listing.series.is_empty(), "catalog {catalog:?} is empty");
        cover(http, site, &listing.series)
            .await
            .with_context(|| format!("catalog {catalog:?}"))?;
        said.push(format!("{catalog:?} {}", listing.series.len()));
        series.extend(listing.series);
    }
    let mut free = None;
    for link in series.iter().take(TRIES) {
        let url = Url::parse(&link.url)?;
        let Ok(opened) = site.series(http, &url).await else {
            continue;
        };
        if let Some(chapter) = opened.chapters.iter().find(|chapter| !chapter.locked) {
            free = Some((opened.title, chapter.url.clone()));
            break;
        }
    }
    let (title, chapter) = free
        .with_context(|| format!("none of the first {TRIES} series opened with a free chapter"))?;
    let read = site
        .chapter(http, &Url::parse(&chapter)?)
        .await
        .with_context(|| format!("chapter {chapter}"))?;
    let first = read.pages.first().context("the chapter has no pages")?;
    let bytes = site.page(http, first).await.context("page")?;
    let format = image::guess_format(&bytes).context("the page isn't an image")?;
    image::load_from_memory_with_format(&bytes, format).context("the page doesn't decode")?;
    let file = pages.join(id).with_extension(format.extensions_str()[0]);
    std::fs::write(&file, &bytes)?;
    said.push(format!(
        "{title} / {}: {} pages, {:?}",
        read.title,
        read.pages.len(),
        read.direction
    ));
    Ok(said.join(", "))
}

/// That the first of `series` with a cover has one that's a picture.
async fn cover(http: &reqwest::Client, site: &dyn Extension, series: &[SeriesLink]) -> Result<()> {
    let Some(url) = series.iter().find_map(|link| link.cover.as_deref()) else {
        return Ok(());
    };
    let bytes = site.cover(http, &Url::parse(url)?).await.context("cover")?;
    let cover = image::load_from_memory(&bytes).context("the cover doesn't decode")?;
    ensure!(
        cover.width() >= COVER,
        "the cover is {} pixels wide: {url}",
        cover.width()
    );
    Ok(())
}
