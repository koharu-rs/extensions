//! KADOKAWA's カドコミ (コミックウォーカー), through the API its own pages
//! read: its search, ranking, and day's updates, a series' episodes, and an
//! episode's pages, each XORed with the key its viewer reply gives, which
//! `page` undoes as the viewer does.

use anyhow::{Context as _, Result, bail, ensure};
use futures::future::BoxFuture;
use reqwest::Client;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use url::Url;

use crate::{
    Catalog, Chapter, ChapterLink, Extension, Listing, PageSide, ReadingDirection, Series,
    SeriesLink,
};

const SITE: &str = "https://comic-walker.com/";
/// How many series a page of a list holds.
const LIMIT: u32 = 30;

pub struct ComicWalker;

/// A series as the API lists it.
#[derive(Deserialize)]
struct Work {
    code: String,
    title: String,
    thumbnail: Option<String>,
}

/// A page of search results.
#[derive(Deserialize)]
struct Found {
    pagination: Total,
    result: Vec<Work>,
}

#[derive(Deserialize)]
struct Total {
    total: u32,
}

/// A page of the ranking.
#[derive(Deserialize)]
struct Ranking {
    total: u32,
    result: Vec<Work>,
}

/// The series updated each day, the latest first.
#[derive(Deserialize)]
struct Daily {
    resources: Vec<Day>,
}

#[derive(Deserialize)]
struct Day {
    works: Vec<Work>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Detail {
    work: Work,
    first_episodes: Episodes,
}

#[derive(Deserialize)]
struct Episodes {
    result: Vec<Episode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Episode {
    id: String,
    code: String,
    title: String,
    sub_title: String,
    /// Free to read now.
    is_active: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Viewer {
    scroll_direction: Option<String>,
    start_position: Option<String>,
    manuscripts: Vec<Manuscript>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manuscript {
    drm_image_url: String,
    drm_mode: Option<String>,
    drm_hash: Option<String>,
}

/// A page as the chapter hands it to `page`.
#[derive(Deserialize, Serialize)]
struct Page {
    url: String,
    /// The key it's XORed with, in hex.
    key: Option<String>,
}

impl Extension for ComicWalker {
    fn id(&self) -> &'static str {
        "comic-walker"
    }

    fn name(&self) -> &'static str {
        "カドコミ"
    }

    fn sign_in(&self) -> Option<&'static str> {
        Some("https://comic-walker.com/auth/signin?callbackUrl=https%3A%2F%2Fcomic-walker.com%2F")
    }

    fn reads(&self, url: &Url) -> bool {
        url.scheme() == "https" && url.host_str() == Some("comic-walker.com")
    }

    fn search<'a>(
        &'a self,
        http: &'a Client,
        query: &'a str,
        next: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        Box::pin(async move {
            let offset = offset(next)?;
            let mut url = endpoint("search/keywords");
            url.query_pairs_mut()
                .append_pair("keywords", query)
                .append_pair("limit", &LIMIT.to_string())
                .append_pair("offset", &offset.to_string());
            let found: Found = api(http, url).await?;
            Ok(listing(found.result, offset, found.pagination.total))
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
            if catalog == Catalog::Popular {
                let offset = offset(next)?;
                let mut url = endpoint("ranking");
                url.query_pairs_mut()
                    .append_pair("limit", &LIMIT.to_string())
                    .append_pair("offset", &offset.to_string());
                let ranking: Ranking = api(http, url).await?;
                return Ok(listing(ranking.result, offset, ranking.total));
            }
            if catalog != Catalog::Latest {
                bail!("カドコミ has no {catalog:?} catalog");
            }
            // The first day's are today's.
            let daily: Daily = api(http, endpoint("daily/new")).await?;
            let works = daily.resources.into_iter().next().map(|day| day.works);
            Ok(Listing {
                series: works.unwrap_or_default().into_iter().map(link).collect(),
                next: None,
            })
        })
    }

    fn series<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Series>> {
        Box::pin(async move {
            let (work, _) = codes(url)?;
            let detail = detail(http, work).await?;
            let chapters = detail
                .first_episodes
                .result
                .into_iter()
                .map(|episode| ChapterLink {
                    title: title(&episode),
                    url: format!("{SITE}detail/{work}/episodes/{}", episode.code),
                    locked: !episode.is_active,
                })
                .collect();
            Ok(Series {
                title: detail.work.title,
                cover: detail.work.thumbnail,
                chapters,
            })
        })
    }

    fn chapter<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Chapter>> {
        Box::pin(async move {
            let (work, code) = codes(url)?;
            let code = code.with_context(|| format!("{url} isn't an episode"))?;
            let detail = detail(http, work).await?;
            let episode = detail
                .first_episodes
                .result
                .into_iter()
                .find(|episode| episode.code == code)
                .with_context(|| format!("{work} has no episode {code}"))?;
            let mut viewer = endpoint("contents/viewer");
            viewer
                .query_pairs_mut()
                .append_pair("episodeId", &episode.id)
                .append_pair("imageSizeType", "width:1284");
            let viewer: Viewer = api(http, viewer).await?;
            let direction = match viewer.scroll_direction.as_deref() {
                Some("ltr") => ReadingDirection::Ltr,
                Some("vertical" | "ttb") => ReadingDirection::Vertical,
                _ => ReadingDirection::Rtl,
            };
            let pages = viewer
                .manuscripts
                .into_iter()
                .map(|manuscript| {
                    let xored = manuscript.drm_mode.as_deref() == Some("xor");
                    serde_json::to_string(&Page {
                        url: manuscript.drm_image_url,
                        key: manuscript.drm_hash.filter(|_| xored),
                    })
                })
                .collect::<serde_json::Result<_>>()?;
            Ok(Chapter {
                title: title(&episode),
                series: Some(detail.work.title),
                direction: Some(direction),
                first_page: match viewer.start_position.as_deref() {
                    Some("left") => Some(PageSide::Left),
                    Some("right") => Some(PageSide::Right),
                    _ => None,
                },
                pages,
            })
        })
    }

    fn page<'a>(&'a self, http: &'a Client, page: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            let page: Page = serde_json::from_str(page)?;
            let mut bytes = http
                .get(&page.url)
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?
                .to_vec();
            if let Some(key) = page.key {
                let key = hex::decode(key).context("a page's key isn't hex")?;
                ensure!(!key.is_empty(), "a page's key is empty");
                for (byte, key) in bytes.iter_mut().zip(key.iter().cycle()) {
                    *byte ^= key;
                }
            }
            Ok(bytes)
        })
    }
}

/// The address of the API's `path`.
fn endpoint(path: &str) -> Url {
    Url::parse(&format!("{SITE}api/{path}")).expect("an address on the site")
}

async fn api<T: DeserializeOwned>(http: &Client, url: Url) -> Result<T> {
    Ok(http
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// A series and all its episodes.
async fn detail(http: &Client, work: &str) -> Result<Detail> {
    let mut url = endpoint("contents/details/work");
    url.query_pairs_mut().append_pair("workCode", work);
    api(http, url).await
}

/// The series code and, for an episode, the episode code of an address on
/// the site, `detail/<series>` or `detail/<series>/episodes/<episode>`.
fn codes(url: &Url) -> Result<(&str, Option<&str>)> {
    let mut segments = url
        .path_segments()
        .context("an https address has a path")?
        .filter(|segment| !segment.is_empty());
    let (Some("detail"), Some(work)) = (segments.next(), segments.next()) else {
        bail!("{url} isn't a series");
    };
    let episode = match (segments.next(), segments.next()) {
        (Some("episodes"), Some(episode)) => Some(episode),
        _ => None,
    };
    Ok((work, episode))
}

fn title(episode: &Episode) -> String {
    if episode.sub_title.is_empty() {
        episode.title.clone()
    } else {
        format!("{} {}", episode.title, episode.sub_title)
    }
}

fn link(work: Work) -> SeriesLink {
    SeriesLink {
        title: work.title,
        url: format!("{SITE}detail/{}", work.code),
        cover: work.thumbnail,
    }
}

/// The list's series from `offset`, of `total`.
fn listing(works: Vec<Work>, offset: u32, total: u32) -> Listing {
    let after = offset + LIMIT;
    Listing {
        series: works.into_iter().map(link).collect(),
        next: (after < total).then(|| after.to_string()),
    }
}

/// Where the list's page that `next` starts begins.
fn offset(next: Option<&str>) -> Result<u32> {
    Ok(next.map(str::parse).transpose()?.unwrap_or(0))
}
