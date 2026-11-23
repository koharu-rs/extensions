//! Sites on GigaViewer, Hatena's manga viewer, which many Japanese
//! publishers run: an episode's data from the JSON its page carries, a
//! series' episodes from the list its viewer pages through, and pages,
//! shuffled on a grid, put back as the viewer's puzzle solver does. That
//! follows the viewer's code as read on 2026-11-23. Each site keeps its own
//! search and series list pages, whose markup here is Shōnen Jump+'s.

use std::io::Cursor;

use anyhow::{Context as _, Result, bail};
use chrono::{Datelike as _, FixedOffset, Utc, Weekday};
use futures::future::BoxFuture;
use image::{GenericImage as _, GenericImageView as _, ImageFormat, RgbaImage};
use reqwest::Client;
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    Catalog, Chapter, ChapterLink, Extension, Listing, PageSide, ReadingDirection, Series,
    SeriesLink, text,
};

/// A site on GigaViewer.
pub struct GigaViewer {
    id: &'static str,
    name: &'static str,
    /// The site's root, ending in a slash.
    site: &'static str,
}

pub static SHONEN_JUMP_PLUS: GigaViewer = GigaViewer {
    id: "shonen-jump-plus",
    name: "少年ジャンプ＋",
    site: "https://shonenjumpplus.com/",
};

/// What an episode's page carries in its `#episode-json`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Episode {
    readable_product: Product,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Product {
    title: String,
    series: Option<Aggregate>,
    page_structure: Option<Structure>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Aggregate {
    id: String,
    title: String,
    thumbnail_uri: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Structure {
    /// `baku` when its pages are shuffled.
    cho_ju_giga: Option<String>,
    reading_direction: Option<String>,
    /// Which of a spread's two sides its first page takes, in reading
    /// order: `former` or `latter`.
    start_position: Option<String>,
    pages: Vec<StructurePage>,
}

#[derive(Deserialize)]
struct StructurePage {
    /// `main` for the episode's own pages, besides its ads and links.
    #[serde(rename = "type")]
    kind: String,
    src: Option<String>,
}

/// An episode in its series' list.
#[derive(Deserialize)]
struct Listed {
    title: String,
    viewer_uri: String,
    purchase_info: Purchase,
}

#[derive(Deserialize)]
struct Purchase {
    can_read: bool,
}

/// A page as the chapter hands it to `page`.
#[derive(Deserialize, Serialize)]
struct Page {
    src: String,
    shuffled: bool,
}

/// How many episodes the viewer's list gives at once.
const PER_PAGE: usize = 50;

impl Extension for GigaViewer {
    fn id(&self) -> &'static str {
        self.id
    }

    fn name(&self) -> &'static str {
        self.name
    }

    /// Its front page, whose header opens the site's sign-in and sign-up
    /// dialog; it has no page of its own for them.
    fn sign_in(&self) -> Option<&'static str> {
        Some(self.site)
    }

    fn reads(&self, url: &Url) -> bool {
        url.scheme() == "https"
            && Url::parse(self.site).is_ok_and(|site| url.host_str() == site.host_str())
    }

    fn search<'a>(
        &'a self,
        http: &'a Client,
        query: &'a str,
        _next: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        Box::pin(async move {
            let mut url = Url::parse(self.site)?.join("search")?;
            url.query_pairs_mut().append_pair("q", query);
            let page = Html::parse_document(&text(http, &url).await?);
            let item = selector("ul.search-series-list > li");
            let link = selector("a[test-thumbnail-permalink]");
            let title = selector(".series-title");
            let series = page
                .select(&item)
                .filter_map(|item| {
                    let link = item.select(&link).next()?;
                    Some(SeriesLink {
                        title: inner(item.select(&title).next()?),
                        url: link.attr("href")?.to_owned(),
                        cover: link
                            .select(&selector("img"))
                            .next()
                            .and_then(|image| image.attr("src"))
                            .map(str::to_owned),
                    })
                })
                .collect();
            Ok(Listing { series, next: None })
        })
    }

    fn catalogs(&self) -> &'static [Catalog] {
        &[Catalog::Latest]
    }

    fn catalog<'a>(
        &'a self,
        http: &'a Client,
        catalog: Catalog,
        _next: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        Box::pin(async move {
            if catalog != Catalog::Latest {
                bail!("{} has no {catalog:?} catalog", self.name);
            }
            // The series list groups serials by the weekday they update on in
            // Japan: today's are the latest.
            let japan = FixedOffset::east_opt(9 * 3600).expect("a valid offset");
            let today = match Utc::now().with_timezone(&japan).weekday() {
                Weekday::Mon => "monday",
                Weekday::Tue => "tuesday",
                Weekday::Wed => "wednesday",
                Weekday::Thu => "thursday",
                Weekday::Fri => "friday",
                Weekday::Sat => "saturday",
                Weekday::Sun => "sunday",
            };
            let url = Url::parse(self.site)?.join("series")?;
            let page = Html::parse_document(&text(http, &url).await?);
            let item = selector(&format!(
                "h2.series-list-date-week.{today} + ul.series-list > li.series-list-item > a"
            ));
            let title = selector(".series-list-title");
            let cover = selector(".series-list-thumb img");
            let series = page
                .select(&item)
                .filter_map(|item| {
                    Some(SeriesLink {
                        title: inner(item.select(&title).next()?),
                        url: item.attr("href")?.to_owned(),
                        cover: item
                            .select(&cover)
                            .next()
                            .and_then(|image| image.attr("data-src"))
                            .map(str::to_owned),
                    })
                })
                .collect();
            Ok(Listing { series, next: None })
        })
    }

    fn series<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Series>> {
        Box::pin(async move {
            let series = episode(http, url)
                .await?
                .readable_product
                .series
                .with_context(|| format!("{url} isn't a series' episode"))?;
            let mut chapters = Vec::new();
            loop {
                let mut list =
                    Url::parse(self.site)?.join("api/viewer/pagination_readable_products")?;
                list.query_pairs_mut()
                    .append_pair("type", "episode")
                    .append_pair("aggregate_id", &series.id)
                    .append_pair("sort_order", "asc")
                    .append_pair("offset", &chapters.len().to_string());
                let listed: Vec<Listed> = http
                    .get(list)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                let last = listed.len() < PER_PAGE;
                chapters.extend(listed.into_iter().map(|listed| ChapterLink {
                    title: listed.title,
                    url: listed.viewer_uri,
                    locked: !listed.purchase_info.can_read,
                }));
                if last {
                    break;
                }
            }
            Ok(Series {
                title: series.title,
                cover: series.thumbnail_uri,
                chapters,
            })
        })
    }

    fn chapter<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Chapter>> {
        Box::pin(async move {
            let product = episode(http, url).await?.readable_product;
            let structure = product
                .page_structure
                .with_context(|| format!("{} isn't readable", product.title))?;
            let shuffled = structure.cho_ju_giga.as_deref() == Some("baku");
            let direction = match structure.reading_direction.as_deref() {
                Some("ltr") => ReadingDirection::Ltr,
                Some("vertical") => ReadingDirection::Vertical,
                _ => ReadingDirection::Rtl,
            };
            // A spread's former side is its right one when it reads right to
            // left.
            let first_page = match (structure.start_position.as_deref(), direction) {
                (Some("former"), ReadingDirection::Ltr)
                | (Some("latter"), ReadingDirection::Rtl) => Some(PageSide::Left),
                (Some("former"), ReadingDirection::Rtl)
                | (Some("latter"), ReadingDirection::Ltr) => Some(PageSide::Right),
                _ => None,
            };
            let pages = structure
                .pages
                .into_iter()
                .filter(|page| page.kind == "main")
                .filter_map(|page| page.src)
                .map(|src| serde_json::to_string(&Page { src, shuffled }))
                .collect::<serde_json::Result<_>>()?;
            Ok(Chapter {
                title: product.title,
                series: product.series.map(|series| series.title),
                direction: Some(direction),
                first_page,
                pages,
            })
        })
    }

    fn page<'a>(&'a self, http: &'a Client, page: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            let page: Page = serde_json::from_str(page)?;
            let bytes = http
                .get(&page.src)
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?;
            if !page.shuffled {
                return Ok(bytes.to_vec());
            }
            let image = solve(&image::load_from_memory(&bytes)?.into_rgba8())?;
            let mut png = Vec::new();
            image.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
            Ok(png)
        })
    }
}

/// What the episode page at `url` carries.
async fn episode(http: &Client, url: &Url) -> Result<Episode> {
    let page = Html::parse_document(&text(http, url).await?);
    let data = page
        .select(&selector("script#episode-json"))
        .next()
        .and_then(|script| script.attr("data-value"))
        .with_context(|| format!("{url} isn't an episode"))?;
    Ok(serde_json::from_str(data)?)
}

/// The viewer's puzzle solver: a page's 4 × 4 grid of cells, each a
/// multiple of 8 pixels, transposed, with what's right of and below the
/// grid where it is.
fn solve(puzzled: &RgbaImage) -> Result<RgbaImage> {
    const DIVIDE_NUM: u32 = 4;
    const MULTIPLE: u32 = 8;
    let (width, height) = puzzled.dimensions();
    let cell_width = width / (DIVIDE_NUM * MULTIPLE) * MULTIPLE;
    let cell_height = height / (DIVIDE_NUM * MULTIPLE) * MULTIPLE;
    let mut solved = puzzled.clone();
    for e in 0..DIVIDE_NUM * DIVIDE_NUM {
        let (row, column) = (e / DIVIDE_NUM, e % DIVIDE_NUM);
        let to = column * DIVIDE_NUM + row;
        let cell = puzzled.view(
            column * cell_width,
            row * cell_height,
            cell_width,
            cell_height,
        );
        solved.copy_from(
            &*cell,
            to % DIVIDE_NUM * cell_width,
            to / DIVIDE_NUM * cell_height,
        )?;
    }
    Ok(solved)
}

fn selector(selector: &str) -> Selector {
    Selector::parse(selector).expect("a valid selector")
}

fn inner(element: ElementRef<'_>) -> String {
    element.text().collect::<String>().trim().to_owned()
}
