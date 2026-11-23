//! WEBTOON's free episodes: a series' title and cover from its page, its
//! episodes from the list the mobile site reads, and an episode's images
//! from its viewer, which their host serves only with the site as the
//! Referer.

use anyhow::{Context as _, Result};
use futures::future::BoxFuture;
use reqwest::{Client, header::REFERER};
use scraper::{Html, Selector};
use serde::Deserialize;
use url::Url;

use crate::{Chapter, ChapterLink, Extension, ReadingDirection, Series, text};

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
                pages: page
                    .select(&images)
                    .filter_map(|image| image.attr("data-url"))
                    .map(str::to_owned)
                    .collect(),
            })
        })
    }

    fn page<'a>(&'a self, http: &'a Client, page: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            let response = http
                .get(page)
                .header(REFERER, SITE)
                .send()
                .await?
                .error_for_status()?;
            Ok(response.bytes().await?.to_vec())
        })
    }
}

/// The content of the page's `<meta property>` named `property`.
fn meta(page: &Html, property: &str) -> Option<String> {
    let selector = Selector::parse(&format!(r#"meta[property="{property}"]"#)).ok()?;
    let content = page.select(&selector).next()?.attr("content")?;
    Some(content.to_owned())
}
