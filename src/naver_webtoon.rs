//! Naver Webtoon's free episodes, through the API its own pages read: its
//! search, the day's serials by popularity, a series' title and episodes,
//! and an episode's images from its viewer. Its image host serves only
//! requests with the site as the Referer.

use std::collections::HashMap;

use anyhow::{Context as _, Result, bail};
use chrono::{Datelike as _, FixedOffset, Utc, Weekday};
use futures::future::BoxFuture;
use reqwest::{Client, StatusCode, header::REFERER};
use scraper::{Html, Selector};
use serde::{Deserialize, de::DeserializeOwned};
use url::Url;

use crate::{
    Catalog, Chapter, ChapterLink, Extension, Listing, ReadingDirection, Series, SeriesLink, page,
    text,
};

const SITE: &str = "https://comic.naver.com/";

pub struct NaverWebtoon;

/// A series as the API lists it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Title {
    title_id: u64,
    title_name: String,
    thumbnail_url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Found {
    search_list: Vec<Title>,
    page_info: PageInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    page: u32,
    total_pages: u32,
}

/// Each weekday's serials, keyed by the day's English name in capitals.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Weekdays {
    title_list_map: HashMap<String, Vec<Title>>,
}

/// A page of a series' episodes, the first first.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Articles {
    article_list: Vec<Article>,
    page_info: PageInfo,
}

#[derive(Deserialize)]
struct Article {
    no: u32,
    subtitle: String,
    /// Paid.
    charge: bool,
}

impl Extension for NaverWebtoon {
    fn id(&self) -> &'static str {
        "naver-webtoon"
    }

    fn name(&self) -> &'static str {
        "네이버 웹툰"
    }

    fn reads(&self, url: &Url) -> bool {
        url.scheme() == "https"
            && matches!(
                url.host_str(),
                Some("comic.naver.com" | "m.comic.naver.com")
            )
    }

    fn sign_in(&self) -> Option<&'static str> {
        Some("https://nid.naver.com/nidlogin.login?url=https%3A%2F%2Fcomic.naver.com%2Findex")
    }

    fn search<'a>(
        &'a self,
        http: &'a Client,
        query: &'a str,
        next: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        Box::pin(async move {
            let mut url = endpoint("search/webtoon");
            url.query_pairs_mut()
                .append_pair("keyword", query)
                .append_pair("page", &page(next)?.to_string());
            let found: Found = api(http, url).await?;
            Ok(Listing {
                series: found.search_list.into_iter().map(link).collect(),
                next: after(&found.page_info),
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
            if catalog != Catalog::Popular {
                bail!("Naver Webtoon has no {catalog:?} catalog");
            }
            let mut url = endpoint("webtoon/titlelist/weekday");
            url.query_pairs_mut().append_pair("order", "user");
            let mut weekdays: Weekdays = api(http, url).await?;
            // The site's front page shows the serials of the day in Korea.
            let korea = FixedOffset::east_opt(9 * 3600).expect("a valid offset");
            let today = match Utc::now().with_timezone(&korea).weekday() {
                Weekday::Mon => "MONDAY",
                Weekday::Tue => "TUESDAY",
                Weekday::Wed => "WEDNESDAY",
                Weekday::Thu => "THURSDAY",
                Weekday::Fri => "FRIDAY",
                Weekday::Sat => "SATURDAY",
                Weekday::Sun => "SUNDAY",
            };
            let titles = weekdays.title_list_map.remove(today).unwrap_or_default();
            Ok(Listing {
                series: titles.into_iter().map(link).collect(),
                next: None,
            })
        })
    }

    fn series<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Series>> {
        Box::pin(async move {
            let id = title_id(url)?;
            let mut info = endpoint("article/list/info");
            info.query_pairs_mut().append_pair("titleId", &id);
            let title: Title = api(http, info).await?;
            let mut chapters = Vec::new();
            let mut number = 1;
            loop {
                let mut list = endpoint("article/list");
                list.query_pairs_mut()
                    .append_pair("titleId", &id)
                    .append_pair("page", &number.to_string())
                    .append_pair("sort", "ASC");
                let articles: Articles = api(http, list).await?;
                chapters.extend(
                    articles
                        .article_list
                        .into_iter()
                        .map(|article| ChapterLink {
                            title: article.subtitle,
                            url: format!("{SITE}webtoon/detail?titleId={id}&no={}", article.no),
                            locked: article.charge,
                        }),
                );
                if after(&articles.page_info).is_none() {
                    break;
                }
                number += 1;
            }
            Ok(Series {
                title: title.title_name,
                cover: Some(title.thumbnail_url),
                chapters,
            })
        })
    }

    fn chapter<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Chapter>> {
        Box::pin(async move {
            let page = Html::parse_document(&text(http, url).await?);
            let title = Selector::parse(r#"meta[property="og:title"]"#).expect("a valid selector");
            // The viewer's first image is the age rating, which isn't the episode's.
            let images = Selector::parse(r#"#sectionContWide img[id^="content_image_"]"#)
                .expect("a valid selector");
            Ok(Chapter {
                title: page
                    .select(&title)
                    .next()
                    .and_then(|title| title.attr("content"))
                    .context("the episode's page has no title")?
                    .to_owned(),
                series: None,
                direction: Some(ReadingDirection::Vertical),
                first_page: None,
                pages: page
                    .select(&images)
                    .filter_map(|image| image.attr("src"))
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

/// The address of the API's `path`.
fn endpoint(path: &str) -> Url {
    Url::parse(&format!("{SITE}api/{path}")).expect("an address on the site")
}

async fn api<T: DeserializeOwned>(http: &Client, url: Url) -> Result<T> {
    let response = http.get(url).send().await?;
    // It refuses an adult series to a visitor no one signed in as.
    if response.status() == StatusCode::UNAUTHORIZED {
        bail!("Naver Webtoon asks you to sign in for this series");
    }
    Ok(response.error_for_status()?.json().await?)
}

/// The page after `info`'s, when there's one.
fn after(info: &PageInfo) -> Option<String> {
    (info.page < info.total_pages).then(|| (info.page + 1).to_string())
}

fn link(title: Title) -> SeriesLink {
    SeriesLink {
        title: title.title_name,
        url: format!("{SITE}webtoon/list?titleId={}", title.title_id),
        cover: Some(title.thumbnail_url),
    }
}

/// The series `url` is the episode list or an episode of.
fn title_id(url: &Url) -> Result<String> {
    url.query_pairs()
        .find_map(|(name, value)| (name == "titleId").then(|| value.into_owned()))
        .with_context(|| format!("{url} isn't a series' episode list"))
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
