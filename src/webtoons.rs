//! WEBTOON's free episodes: its originals' search and popularity ranking
//! from the site's pages, a series' title and cover from its page, its
//! episodes from the list the mobile site reads, and an episode's images
//! from its viewer. Its image host serves only requests with the site as
//! the Referer.

use anyhow::{Context as _, Result, bail};
use futures::future::BoxFuture;
use reqwest::{Client, header::REFERER};
use scraper::{Html, Selector};
use serde::Deserialize;
use url::Url;

use crate::{
    Catalog, Chapter, ChapterLink, Extension, Listing, ReadingDirection, Series, SeriesLink, page,
    text,
};

const SITE: &str = "https://www.webtoons.com/";

pub struct Webtoons;

/// The mobile site's episode list.
#[derive(Deserialize)]
struct List {
    result: Listed,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Listed {
    episode_list: Vec<Episode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Episode {
    episode_title: String,
    /// The episode's viewer, from the site's root.
    viewer_link: String,
}

impl Extension for Webtoons {
    fn id(&self) -> &'static str {
        "webtoons"
    }

    fn name(&self) -> &'static str {
        "WEBTOON"
    }

    fn reads(&self, url: &Url) -> bool {
        url.scheme() == "https" && url.host_str() == Some("www.webtoons.com")
    }

    fn search<'a>(
        &'a self,
        http: &'a Client,
        query: &'a str,
        next: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        Box::pin(async move {
            let page = page(next)?;
            let mut url = Url::parse(SITE)?.join("en/search/originals")?;
            url.query_pairs_mut()
                .append_pair("keyword", query)
                .append_pair("page", &page.to_string());
            let found = text(http, &url).await?;
            // The results link the pages after this one.
            let more = found.contains(&format!("page={}\"", page + 1));
            Ok(Listing {
                series: cards(&found),
                next: more.then(|| (page + 1).to_string()),
            })
        })
    }

    fn catalogs(&self) -> &'static [Catalog] {
        &[Catalog::Popular]
    }

    fn catalog<'a>(
        &'a self,
        http: &'a Client,
        catalog: Catalog,
        _next: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        Box::pin(async move {
            let path = match catalog {
                Catalog::Popular => "en/ranking/popular",
                Catalog::Latest | Catalog::Library | Catalog::Ongoing => {
                    bail!("WEBTOON has no {catalog:?} catalog")
                }
            };
            let ranking = text(http, &Url::parse(SITE)?.join(path)?).await?;
            Ok(Listing {
                series: cards(&ranking),
                next: None,
            })
        })
    }

    fn series<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Series>> {
        Box::pin(async move {
            let number = url
                .query_pairs()
                .find_map(|(name, value)| (name == "title_no").then(|| value.into_owned()))
                .with_context(|| format!("{url} isn't a series' episode list"))?;
            let page = text(http, url).await?;
            // A parsed page can't be held across an await.
            let (title, cover) = {
                let page = Html::parse_document(&page);
                (meta(&page, "og:title"), meta(&page, "og:image"))
            };
            let list: List = http
                .get(format!(
                    "https://m.webtoons.com/api/v1/webtoon/{number}/episodes?pageSize=99999"
                ))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let chapters = list
                .result
                .episode_list
                .into_iter()
                .map(|episode| {
                    Ok(ChapterLink {
                        title: episode.episode_title,
                        url: url.join(&episode.viewer_link)?.to_string(),
                        locked: false,
                    })
                })
                .collect::<Result<_>>()?;
            Ok(Series {
                title: title.context("the series' page has no title")?,
                cover,
                chapters,
            })
        })
    }

    fn chapter<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Chapter>> {
        Box::pin(async move {
            let page = Html::parse_document(&text(http, url).await?);
            let images = Selector::parse("#_imageList img._images").expect("a valid selector");
            Ok(Chapter {
                title: meta(&page, "og:title").context("the episode's page has no title")?,
                series: None,
                direction: Some(ReadingDirection::Vertical),
                first_page: None,
                pages: page
                    .select(&images)
                    .filter_map(|image| image.attr("data-url"))
                    .map(str::to_owned)
                    .collect(),
            })
        })
    }

    fn page<'a>(&'a self, http: &'a Client, page: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(image(http, page))
    }

    fn cover<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(image(http, url.as_str()))
    }
}

/// An image from the site's image host.
async fn image(http: &Client, url: &str) -> Result<Vec<u8>> {
    let response = http
        .get(url)
        .header(REFERER, SITE)
        .send()
        .await?
        .error_for_status()?;
    Ok(response.bytes().await?.to_vec())
}

/// The series cards of a search's or a ranking's page.
fn cards(page: &str) -> Vec<SeriesLink> {
    let page = Html::parse_document(page);
    let card = Selector::parse("ul.webtoon_list > li > a").expect("a valid selector");
    let title = Selector::parse("strong.title").expect("a valid selector");
    let cover = Selector::parse("img").expect("a valid selector");
    page.select(&card)
        .filter_map(|card| {
            Some(SeriesLink {
                title: card
                    .select(&title)
                    .next()?
                    .text()
                    .collect::<String>()
                    .trim()
                    .to_owned(),
                url: card
                    .attr("href")
                    .filter(|href| href.contains("list?title_no="))?
                    .to_owned(),
                cover: card
                    .select(&cover)
                    .next()
                    .and_then(|cover| cover.attr("src"))
                    .map(str::to_owned),
            })
        })
        .collect()
}

/// The content of the page's `<meta property>` named `property`.
fn meta(page: &Html, property: &str) -> Option<String> {
    let selector = Selector::parse(&format!(r#"meta[property="{property}"]"#)).ok()?;
    let content = page.select(&selector).next()?.attr("content")?;
    Some(content.to_owned())
}
