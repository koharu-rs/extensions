//! Sites on GigaViewer, Hatena's manga viewer, which many Japanese
//! publishers run: an episode's data from the JSON its page carries, a
//! series' episodes from the list its viewer pages through, and pages,
//! shuffled on a grid, put back as the viewer's puzzle solver does. That
//! follows the viewer's code as read on 2026-11-23. Every site's search page
//! lists its results alike; their series list pages differ in markup but
//! hold the same cards, which `cards` reads.

use std::{collections::HashSet, io::Cursor};

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
    /// Whether it has accounts, which its pages' header signs in to.
    accounts: bool,
    /// Whether its series list groups serials by the weekday they update on
    /// in Shōnen Jump+'s markup, which makes its latest catalog.
    weekdays: bool,
    /// Its series list, from its root.
    list: &'static str,
}

/// A site that has accounts, and its series list at `series`.
const fn site(id: &'static str, name: &'static str, site: &'static str) -> GigaViewer {
    GigaViewer {
        id,
        name,
        site,
        accounts: true,
        weekdays: false,
        list: "series",
    }
}

pub static SHONEN_JUMP_PLUS: GigaViewer = GigaViewer {
    weekdays: true,
    ..site(
        "shonen-jump-plus",
        "少年ジャンプ＋",
        "https://shonenjumpplus.com/",
    )
};
pub static TONARI_NO_YOUNG_JUMP: GigaViewer = GigaViewer {
    accounts: false,
    ..site(
        "tonarinoyj",
        "となりのヤングジャンプ",
        "https://tonarinoyj.jp/",
    )
};
pub static COMIC_DAYS: GigaViewer = site("comic-days", "コミックDAYS", "https://comic-days.com/");
pub static SUNDAY_WEBRY: GigaViewer = GigaViewer {
    accounts: false,
    ..site(
        "sunday-webry",
        "サンデーうぇぶり",
        "https://www.sunday-webry.com/",
    )
};
pub static COMIC_ACTION: GigaViewer =
    site("comic-action", "webアクション", "https://comic-action.com/");
pub static ZENON_PLUS: GigaViewer = site("comic-zenon", "ゼノンプラス", "https://comic-zenon.com/");
pub static MAGCOMI: GigaViewer = site("magcomi", "マグコミ", "https://magcomi.com/");
pub static KURAGE_BUNCH: GigaViewer = GigaViewer {
    list: "series/kuragebunch",
    ..site("kuragebunch", "くらげバンチ", "https://kuragebunch.com/")
};
pub static COMIC_GARDO: GigaViewer =
    site("comic-gardo", "コミックガルド", "https://comic-gardo.com/");
pub static COMIC_EARTH_STAR: GigaViewer = site(
    "comic-earthstar",
    "コミック アース・スター",
    "https://comic-earthstar.com/",
);
pub static COMIC_TRAIL: GigaViewer = GigaViewer {
    accounts: false,
    ..site(
        "comic-trail",
        "コミックトレイル",
        "https://comic-trail.com/",
    )
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
        self.accounts.then_some(self.site)
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
            // Each result's thumbnail links its first episode, and its image
            // is named after the series; the older pages and the newer ones
            // share only that.
            let link = selector(r#".search-container section li a[href*="/episode/"]"#);
            let image = selector("img");
            let series = page
                .select(&link)
                .filter_map(|link| {
                    let image = link.select(&image).next()?;
                    Some(SeriesLink {
                        title: image.attr("alt").filter(|alt| !alt.is_empty())?.to_owned(),
                        url: link.attr("href")?.to_owned(),
                        cover: image
                            .attr("data-src")
                            .or_else(|| image.attr("src"))
                            .map(str::to_owned),
                    })
                })
                .collect();
            Ok(Listing { series, next: None })
        })
    }

    fn catalogs(&self) -> &'static [Catalog] {
        if self.weekdays {
            &[Catalog::Latest, Catalog::Ongoing]
        } else {
            &[Catalog::Ongoing]
        }
    }

    fn catalog<'a>(
        &'a self,
        http: &'a Client,
        catalog: Catalog,
        _next: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        Box::pin(async move {
            let url = Url::parse(self.site)?.join(self.list)?;
            if catalog == Catalog::Ongoing {
                let page = Html::parse_document(&text(http, &url).await?);
                return Ok(Listing {
                    series: cards(&page),
                    next: None,
                });
            }
            if catalog != Catalog::Latest || !self.weekdays {
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

/// The series cards of a series list: each a list item, or an element of
/// class `series-item` or a `-series-item`, holding a link to one of its episodes, its
/// thumbnail, and its title, which the sites put in a data attribute, a
/// heading, or the thumbnail's alt text. A series listed under several
/// weekdays is kept where it's first.
fn cards(page: &Html) -> Vec<SeriesLink> {
    let link = selector(r#"a[href*="/episode/"]"#);
    let named = selector("[data-series-title], [data-series-name]");
    let heading = selector("h2, h3, h4");
    let image = selector("img");
    // A card's thumbnail is in its link; a badge may come before it.
    let linked = selector(r#"a[href*="/episode/"] img"#);
    let mut cards = HashSet::new();
    let mut titles = HashSet::new();
    let mut series = Vec::new();
    for link in page.select(&link) {
        let Some(card) = link
            .ancestors()
            .filter_map(ElementRef::wrap)
            .find(|element| {
                element.value().name() == "li"
                    || element
                        .value()
                        .classes()
                        .any(|class| class == "series-item" || class.ends_with("-series-item"))
            })
        else {
            continue;
        };
        if !cards.insert(card.id()) {
            continue;
        }
        let thumbnail = card
            .select(&linked)
            .next()
            .or_else(|| card.select(&image).next());
        let title = card
            .select(&named)
            .next()
            .and_then(|named| {
                named
                    .attr("data-series-title")
                    .or_else(|| named.attr("data-series-name"))
                    .map(str::to_owned)
            })
            .or_else(|| card.select(&heading).next().map(inner))
            .or_else(|| {
                thumbnail
                    .and_then(|image| image.attr("alt"))
                    .map(str::to_owned)
            })
            .filter(|title| !title.is_empty());
        let (Some(title), Some(href)) = (title, link.attr("href")) else {
            continue;
        };
        if !titles.insert(title.clone()) {
            continue;
        }
        series.push(SeriesLink {
            title,
            url: href.to_owned(),
            cover: cover(card, thumbnail),
        });
    }
    series
}

/// A card's cover: its thumbnail's lazy or plain source, past spacers and
/// inline placeholders, or else the first source of the copy a Next.js page
/// keeps in `<noscript>`, which the parser leaves as text.
fn cover(card: ElementRef<'_>, thumbnail: Option<ElementRef<'_>>) -> Option<String> {
    let real = |src: &&str| src.starts_with("https://") && !src.contains("spacer");
    if let Some(src) = thumbnail.and_then(|image| {
        [image.attr("data-src"), image.attr("src")]
            .into_iter()
            .flatten()
            .find(real)
    }) {
        return Some(src.to_owned());
    }
    let html = card.html();
    let (_, sources) = html
        .split_once("srcSet=\"")
        .or_else(|| html.split_once("srcset=\""))?;
    // An entry ends at the space before its density; its address may hold
    // commas.
    let first = sources.split([' ', '"']).next()?;
    real(&first).then(|| first.to_owned())
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
