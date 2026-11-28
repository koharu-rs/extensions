//! E-Hentai's galleries, and ExHentai's: a search and the site's popular and
//! newest lists from its pages, a gallery's name and cover from its page, its
//! images from the thumbnail pages the gallery spreads them over, and each
//! image from the viewer page that holds it, which shows it at the size the
//! signed-in account's viewer is set to. Its image hosts serve only requests
//! that carry the site as the Referer.

use anyhow::{Context as _, Result, bail};
use futures::future::BoxFuture;
use reqwest::{Client, header::REFERER};
use scraper::{ElementRef, Html, Selector};
use url::Url;

use crate::{Account, Catalog, Chapter, ChapterLink, Extension, Listing, Series, SeriesLink, text};

const SITE: &str = "https://e-hentai.org/";
const PAGES: usize = 200;

pub struct EHentai;

impl Extension for EHentai {
    fn id(&self) -> &'static str {
        "e-hentai"
    }

    fn name(&self) -> &'static str {
        "E-Hentai"
    }

    fn reads(&self, url: &Url) -> bool {
        url.scheme() == "https" && matches!(url.host_str(), Some("e-hentai.org" | "exhentai.org"))
    }

    fn sign_in(&self) -> Option<&'static str> {
        Some("https://e-hentai.org/bounce_login.php")
    }

    fn account<'a>(&'a self, http: &'a Client) -> BoxFuture<'a, Result<Option<Account>>> {
        Box::pin(async move {
            let home = http
                .get(format!("{SITE}home.php"))
                .send()
                .await?
                .error_for_status()?;
            Ok((home.url().path() == "/home.php").then_some(Account { name: None }))
        })
    }

    fn search<'a>(
        &'a self,
        http: &'a Client,
        query: &'a str,
        next: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        Box::pin(async move {
            let page = match next {
                Some(next) => Url::parse(next)?,
                None => Url::parse_with_params(SITE, [("f_search", query)])?,
            };
            listing(http, &page).await
        })
    }

    fn catalogs(&self) -> &'static [Catalog] {
        &[Catalog::Popular, Catalog::Latest]
    }

    fn catalog<'a>(
        &'a self,
        http: &'a Client,
        catalog: Catalog,
        next: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        Box::pin(async move {
            let page = match (catalog, next) {
                (_, Some(next)) => Url::parse(next)?,
                (Catalog::Popular, None) => Url::parse(&format!("{SITE}popular"))?,
                (Catalog::Latest, None) => Url::parse(SITE)?,
                (Catalog::Library | Catalog::Ongoing, None) => {
                    bail!("E-Hentai has no {catalog:?} catalog")
                }
            };
            listing(http, &page).await
        })
    }

    fn series<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Series>> {
        Box::pin(async move {
            let gallery = gallery(url);
            let page = GalleryPage::read(http, &gallery).await?;
            let title = page.title(&gallery)?;
            Ok(Series {
                chapters: vec![ChapterLink {
                    title: title.clone(),
                    url: gallery.to_string(),
                    locked: false,
                }],
                title,
                cover: page.cover,
            })
        })
    }

    fn chapter<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Chapter>> {
        Box::pin(async move {
            let gallery = gallery(url);
            let first = GalleryPage::read(http, &gallery).await?;
            let title = first.title(&gallery)?;
            let (mut shown, total) = first
                .showing
                .with_context(|| format!("{gallery} doesn't say how many images it has"))?;
            let mut pages = first.images;
            for number in 1.. {
                if shown >= total {
                    break;
                }
                if number == PAGES {
                    bail!("{gallery} shows more than {PAGES} pages of thumbnails");
                }
                let mut next = gallery.clone();
                next.query_pairs_mut().append_pair("p", &number.to_string());
                let page = GalleryPage::read(http, &next).await?;
                // The site answers past the last page with the last one.
                match page.showing {
                    Some((last, _)) if last > shown => shown = last,
                    _ => break,
                }
                pages.extend(page.images);
            }
            Ok(Chapter {
                title,
                series: None,
                direction: None,
                first_page: None,
                pages,
            })
        })
    }

    fn page<'a>(&'a self, http: &'a Client, page: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            let viewer = Url::parse(page)?;
            let src =
                viewed(&Html::parse_document(&text(http, &viewer).await?)).with_context(|| {
                    format!(
                        "{viewer} shows no image, as an expired key or a page that needs a \
                         sign-in does"
                    )
                })?;
            let image = http
                .get(src)
                .header(REFERER, viewer.origin().ascii_serialization())
                .send()
                .await?
                .error_for_status()?;
            Ok(image.bytes().await?.to_vec())
        })
    }
}

/// What a gallery's page of thumbnails says about it, read out of the parsed
/// page, which can't be held across an await.
struct GalleryPage {
    title: Option<String>,
    cover: Option<String>,
    showing: Option<(usize, usize)>,
    images: Vec<String>,
}

impl GalleryPage {
    async fn read(http: &Client, url: &Url) -> Result<Self> {
        let page = Html::parse_document(&text(http, url).await?);
        Ok(Self {
            title: title(&page),
            cover: cover(&page),
            showing: showing(&page),
            images: images(&page),
        })
    }

    /// The gallery's name, or why `gallery` has none.
    fn title(&self, gallery: &Url) -> Result<String> {
        self.title
            .clone()
            .with_context(|| format!("{gallery} isn't a gallery"))
    }
}

/// The page of results at `url`, and where the next one starts.
async fn listing(http: &Client, url: &Url) -> Result<Listing> {
    let page = Html::parse_document(&text(http, url).await?);
    Ok(Listing {
        series: cards(&page),
        next: more(&page),
    })
}

/// Where a page of results links the next one, when it isn't the last.
fn more(page: &Html) -> Option<String> {
    let next = Selector::parse("a#unext[href]").expect("a valid selector");
    first_attr(page, &next, "href")
}

/// The results a page of the site's listings shows, in each of the layouts
/// it offers.
fn cards(page: &Html) -> Vec<SeriesLink> {
    let names = Selector::parse("div.glink").expect("a valid selector");
    let pictures = Selector::parse("img").expect("a valid selector");
    page.select(&names)
        .filter_map(|name| card(name, &pictures))
        .collect()
}

/// The result naming `name`, which every layout puts inside its link and
/// beside its cover.
fn card(name: ElementRef<'_>, pictures: &Selector) -> Option<SeriesLink> {
    let link = upwards(name, 2).find(|element| element.value().name() == "a")?;
    let cover = upwards(name, 4).find_map(|block| {
        block.select(pictures).find_map(|picture| {
            // A lazily loaded cover holds a placeholder until it's scrolled to.
            let src = picture.attr("data-src").or_else(|| picture.attr("src"))?;
            (!src.starts_with("data:")).then(|| src.to_owned())
        })
    });
    Some(SeriesLink {
        title: trimmed_text(name),
        url: link.attr("href")?.to_owned(),
        cover,
    })
}

/// The elements above `element`, nearest first, `levels` at most.
fn upwards(element: ElementRef<'_>, levels: usize) -> impl Iterator<Item = ElementRef<'_>> {
    std::iter::successors(element.parent(), |node| node.parent())
        .filter_map(ElementRef::wrap)
        .take(levels)
}

/// The gallery's name, as its page gives it.
fn title(page: &Html) -> Option<String> {
    let name = Selector::parse("#gn").expect("a valid selector");
    Some(trimmed_text(page.select(&name).next()?)).filter(|title| !title.is_empty())
}

/// The gallery's cover, as its page shows it, in the image the box holds or
/// in the box's background.
fn cover(page: &Html) -> Option<String> {
    let image = Selector::parse("#gd1 img[src]").expect("a valid selector");
    let styled = Selector::parse("#gd1 [style]").expect("a valid selector");
    first_attr(page, &image, "src")
        .or_else(|| background(&first_attr(page, &styled, "style")?).map(str::to_owned))
}

/// The image a `style` sets as its background: `url("…")`, quoted or not.
fn background(style: &str) -> Option<&str> {
    let (_, rest) = style.split_once("url(")?;
    let (url, _) = rest.split_once(')')?;
    Some(url.trim_matches(['"', '\'']))
}

/// The gallery `url` opens, whatever page of it it starts at.
fn gallery(url: &Url) -> Url {
    let mut gallery = url.clone();
    gallery.set_query(None);
    gallery.set_fragment(None);
    gallery
}

/// The last image a gallery page shows, and how many the gallery has, as its
/// pager says: "Showing 1 - 20 of 1,022 images".
fn showing(page: &Html) -> Option<(usize, usize)> {
    let pager = Selector::parse("p.gpc").expect("a valid selector");
    let text = trimmed_text(page.select(&pager).next()?);
    let (shown, all) = text.strip_prefix("Showing ")?.split_once(" of ")?;
    let count = |number: &str| number.trim().replace(',', "").parse().ok();
    let last = count(shown.rsplit('-').next()?)?;
    let total = count(all.split_whitespace().next()?)?;
    Some((last, total))
}

/// The viewer pages of the thumbnails a gallery page shows, in order.
fn images(page: &Html) -> Vec<String> {
    let thumbnails = Selector::parse(r#"#gdt a[href*="/s/"]"#).expect("a valid selector");
    page.select(&thumbnails)
        .filter_map(|thumbnail| thumbnail.attr("href"))
        .map(str::to_owned)
        .collect()
}

/// The image a viewer page shows.
fn viewed(page: &Html) -> Option<String> {
    let image = Selector::parse("img#img[src]").expect("a valid selector");
    first_attr(page, &image, "src")
}

/// `attribute` of the first element on `page` that `selector` matches.
fn first_attr(page: &Html, selector: &Selector, attribute: &str) -> Option<String> {
    page.select(selector)
        .next()?
        .attr(attribute)
        .map(str::to_owned)
}

/// The text inside `element`, without the space around it.
fn trimmed_text(element: ElementRef<'_>) -> String {
    element.text().collect::<String>().trim().to_owned()
}
