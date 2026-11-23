//! pixiv Comic's episodes, through the API its own pages read, which
//! answers only requests that say they come from the site. Its viewer's
//! reply, an episode's pages, also wants each request signed as the viewer
//! signs it, and the pages come shuffled on a grid, which `unshuffle` puts
//! back as the viewer does. Both follow the viewer's code as read on
//! 2026-11-23; that Koharu ships them is the risk its owner accepted.

use std::io::Cursor;

use anyhow::{Context as _, Result};
use futures::future::BoxFuture;
use image::{ImageFormat, RgbaImage};
use rand_xoshiro::{
    Xoshiro128StarStar,
    rand_core::{RngCore as _, SeedableRng as _},
};
use reqwest::{Client, RequestBuilder, header::REFERER};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::{Chapter, ChapterLink, Extension, ReadingDirection, Series, text};

const SITE: &str = "https://comic.pixiv.net/";
/// What the viewer hashes before a page's key to seed its shuffle.
const SEED: &str = "4wXCKprMMoxnyJ3PocJFs4CYbfnbazNe";

pub struct PixivComic;

/// The API's every answer.
#[derive(Deserialize)]
struct Data<T> {
    data: T,
}

#[derive(Deserialize)]
struct Work {
    official_work: OfficialWork,
}

#[derive(Deserialize)]
struct OfficialWork {
    name: String,
    image: Images,
}

#[derive(Deserialize)]
struct Images {
    main: String,
}

#[derive(Deserialize)]
struct Episodes {
    /// Every episode, an unpublished one without its `episode`.
    episodes: Vec<Listed>,
}

#[derive(Deserialize)]
struct Listed {
    episode: Option<Episode>,
}

#[derive(Deserialize)]
struct Episode {
    numbering_title: String,
    sub_title: String,
    viewer_path: String,
    state: String,
}

#[derive(Deserialize)]
struct Read {
    reading_episode: Reading,
}

#[derive(Deserialize)]
struct Reading {
    work_title: String,
    numbering_title: String,
    sub_title: String,
    is_tateyomi: bool,
    pages: Vec<Page>,
}

/// A page as the viewer's reply gives it, which its chapter's `pages` hold
/// as JSON.
#[derive(Deserialize, Serialize)]
struct Page {
    url: String,
    /// Its shuffle's key and grid, when it's shuffled.
    key: Option<String>,
    gridsize: Option<u32>,
}

impl Extension for PixivComic {
    fn id(&self) -> &'static str {
        "pixiv-comic"
    }

    fn name(&self) -> &'static str {
        "pixiv Comic"
    }

    fn reads(&self, url: &Url) -> bool {
        url.scheme() == "https" && url.host_str() == Some("comic.pixiv.net")
    }

    fn sign_in(&self) -> Option<&'static str> {
        Some(
            "https://accounts.pixiv.net/login?source=comic&return_to=https%3A%2F%2Fcomic.pixiv.net%2F",
        )
    }

    fn series<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Series>> {
        Box::pin(async move {
            let id = url
                .path()
                .strip_prefix("/works/")
                .with_context(|| format!("{url} isn't a work"))?;
            let work: Work = answer(api(http, &format!("works/v5/{id}"))).await?;
            let episodes: Episodes =
                answer(api(http, &format!("works/{id}/episodes/v2?order=asc"))).await?;
            let chapters = episodes
                .episodes
                .into_iter()
                .filter_map(|listed| listed.episode)
                .map(|episode| {
                    Ok(ChapterLink {
                        title: title(&episode.numbering_title, &episode.sub_title),
                        url: url.join(&episode.viewer_path)?.to_string(),
                        locked: episode.state != "readable",
                    })
                })
                .collect::<Result<_>>()?;
            Ok(Series {
                title: work.official_work.name,
                cover: Some(work.official_work.image.main),
                chapters,
            })
        })
    }

    fn chapter<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Chapter>> {
        Box::pin(async move {
            let id = url
                .path()
                .strip_prefix("/viewer/stories/")
                .with_context(|| format!("{url} isn't an episode"))?;
            // The viewer signs with the salt its page carries: SHA-256 of the
            // time and the salt.
            let page = text(http, url).await?;
            let salt = {
                let page = Html::parse_document(&page);
                let script = Selector::parse("script#__NEXT_DATA__").expect("a valid selector");
                let data: Value = serde_json::from_str(
                    &page
                        .select(&script)
                        .next()
                        .context("the episode's page has no data")?
                        .text()
                        .collect::<String>(),
                )?;
                data.pointer("/props/pageProps/salt")
                    .and_then(Value::as_str)
                    .context("the episode's page has no salt")?
                    .to_owned()
            };
            let time = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
            let hash: String = Sha256::digest(format!("{time}{salt}"))
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let read: Read = answer(
                api(http, &format!("episodes/{id}/read_v4"))
                    .header("X-Client-Time", time)
                    .header("X-Client-Hash", hash),
            )
            .await?;
            let episode = read.reading_episode;
            Ok(Chapter {
                title: title(&episode.numbering_title, &episode.sub_title),
                series: Some(episode.work_title),
                direction: Some(if episode.is_tateyomi {
                    ReadingDirection::Vertical
                } else {
                    ReadingDirection::Rtl
                }),
                pages: episode
                    .pages
                    .iter()
                    .map(serde_json::to_string)
                    .collect::<Result<_, _>>()?,
            })
        })
    }

    fn page<'a>(&'a self, http: &'a Client, page: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            let page: Page = serde_json::from_str(page)?;
            let mut request = http.get(&page.url).header(REFERER, SITE);
            if let Some(key) = &page.key {
                request = request.header("X-Cobalt-Thumber-Parameter-GridShuffle-Key", key);
            }
            let bytes = request.send().await?.error_for_status()?.bytes().await?;
            let (Some(key), Some(grid)) = (page.key, page.gridsize) else {
                return Ok(bytes.to_vec());
            };
            let image = image::load_from_memory(&bytes)?.into_rgba8();
            let (width, height) = image.dimensions();
            let pixels = unshuffle(image.as_raw(), width, height, grid, &key);
            let image = RgbaImage::from_raw(width, height, pixels).expect("as many pixels");
            let mut png = Vec::new();
            image.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
            Ok(png)
        })
    }
}

/// A request to the site's API, said to come from the site.
fn api(http: &Client, path: &str) -> RequestBuilder {
    http.get(format!("{SITE}api/app/{path}"))
        .header("X-Requested-With", "pixivcomic")
}

async fn answer<T: DeserializeOwned>(request: RequestBuilder) -> Result<T> {
    let data: Data<T> = request.send().await?.error_for_status()?.json().await?;
    Ok(data.data)
}

fn title(numbering: &str, sub: &str) -> String {
    if sub.is_empty() {
        numbering.to_owned()
    } else {
        format!("{numbering} {sub}")
    }
}

/// Puts a page's tiles back where the viewer does. `pixels` are RGBA, in
/// `grid`-pixel tiles; each row of tiles was shuffled by its own draws from
/// xoshiro128**, seeded with the first 16 bytes of SHA-256 of `SEED` and
/// the page's `key`, after 100 discarded draws. A strip right of the last
/// whole tile isn't shuffled.
fn unshuffle(pixels: &[u8], width: u32, height: u32, grid: u32, key: &str) -> Vec<u8> {
    let (width, height, grid) = (width as usize, height as usize, grid as usize);
    let columns = width / grid;
    let digest = Sha256::digest(format!("{SEED}{key}"));
    let mut random =
        Xoshiro128StarStar::from_seed(digest[..16].try_into().expect("SHA-256 has 32 bytes"));
    for _ in 0..100 {
        random.next_u32();
    }
    // Where each row's tiles came from: the inverse of its Fisher–Yates
    // shuffle.
    let sources: Vec<Vec<usize>> = (0..height.div_ceil(grid))
        .map(|_| {
            let mut shuffled: Vec<usize> = (0..columns).collect();
            for index in (1..columns).rev() {
                let other = random.next_u32() as usize % (index + 1);
                shuffled.swap(index, other);
            }
            let mut sources = vec![0; columns];
            for (position, &tile) in shuffled.iter().enumerate() {
                sources[tile] = position;
            }
            sources
        })
        .collect();
    let mut unshuffled = pixels.to_vec();
    let tile = grid * 4;
    for y in 0..height {
        let row = y * width * 4;
        for (to, &from) in sources[y / grid].iter().enumerate() {
            let (to, from) = (row + to * tile, row + from * tile);
            unshuffled[to..to + tile].copy_from_slice(&pixels[from..from + tile]);
        }
    }
    unshuffled
}
