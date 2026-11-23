// Adapted from Koharu's scripts/kinde.ts, itself adapted from Azxiana's
// https://github.com/Alexia/kandle-downloader/blob/71f7e5346eddcc031a6899c091f13a2d80ba73fd/kandle-downloader.js
//! Kindle JP's manga reader. Each ASIN is one volume and one chapter.
//! The signed-in reader page supplies the rendering token; render archives
//! supply locations, page elements, and signed CDN image addresses.

use std::{collections::HashSet, sync::LazyLock, time::Duration};

use aes_gcm::{
    AesGcm, KeyInit,
    aead::{Aead as _, Payload, consts::U16},
    aes::Aes128,
};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::future::BoxFuture;
use reqwest::{Client, header::HeaderValue, redirect::Policy};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use url::Url;

use crate::{Catalog, Chapter, ChapterLink, Extension, Listing, Series, SeriesLink};

const SITE: &str = "https://read.amazon.co.jp/";
const TIMEOUT: Duration = Duration::from_secs(30);
// Kindle serves its reader bootstrap only to a supported desktop browser.
const READER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36";

pub struct Kindle;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Library {
    items_list: Vec<LibraryBook>,
    pagination_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LibraryBook {
    asin: String,
    title: String,
    product_url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Book {
    asin: String,
    title: String,
    content_guid: String,
    karamel_token: Token,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Token {
    token: String,
    expires_at: u64,
}

struct Render {
    manifest: Manifest,
    locations: Option<Vec<u64>>,
    pages: Vec<RenderedPage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    cdn: Cdn,
    cdn_resources: Vec<Resource>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Cdn {
    base_url: String,
    auth_parameter: Option<String>,
    is_encrypted: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Resource {
    url: String,
    auth_parameter: Option<String>,
}

#[derive(Deserialize)]
struct RenderedPage {
    children: Vec<Element>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Element {
    image_reference: Option<String>,
    element_id: u64,
}

/// Only the in-memory chapter holds these credentials; shelf links are reader URLs.
#[derive(Deserialize, Serialize)]
struct Page {
    url: String,
    token: Option<Token>,
}

impl Extension for Kindle {
    fn id(&self) -> &'static str {
        "kindle"
    }

    fn name(&self) -> &'static str {
        "Kindle JP"
    }

    fn reads(&self, url: &Url) -> bool {
        url.scheme() == "https" && url.host_str() == Some("read.amazon.co.jp")
    }

    fn sign_in(&self) -> Option<&'static str> {
        Some(SITE)
    }

    fn account<'a>(&'a self, http: &'a Client) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async move {
            let response = http
                .get(format!(
                    "{SITE}kindle-library/search?libraryType=MANGA&querySize=1&sortType=recency"
                ))
                .send()
                .await?
                .error_for_status()?;
            if !self.reads(response.url()) || response.url().path() != "/kindle-library/search" {
                return Ok(None);
            }
            let _: Library = response.json().await?;
            // Kindle's library doesn't carry the account name. Amazon's navbar
            // configuration has the full name; its visible label is shortened.
            let page = http
                .get("https://www.amazon.co.jp/")
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?;
            let name = page
                .lines()
                .find_map(|line| {
                    line.trim()
                        .strip_prefix("window.$Nav && $Nav.declare('config.customerName',")?
                        .strip_suffix(");")
                })
                .context("Amazon's page has no account name")?;
            Ok(Some(json5::from_str(name)?))
        })
    }

    fn search<'a>(
        &'a self,
        http: &'a Client,
        query: &'a str,
        next: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        Box::pin(async move {
            let query = query.trim();
            if !query.starts_with("https://") && !is_asin(query) {
                return library(http, query, next).await;
            }
            let url = if query.starts_with("https://") {
                Url::parse(query)?
            } else {
                let mut url = Url::parse(SITE)?.join("manga/")?;
                url.path_segments_mut()
                    .expect("an https address has a path")
                    .pop_if_empty()
                    .push(query);
                url
            };
            let book = book(http, &url).await?;
            Ok(Listing {
                series: vec![SeriesLink {
                    title: book.title,
                    url: format!("{SITE}manga/{}", book.asin),
                    cover: Some(cover(&book.asin)),
                }],
                next: None,
            })
        })
    }

    fn catalogs(&self) -> &'static [Catalog] {
        &[Catalog::Library]
    }

    fn catalog<'a>(
        &'a self,
        http: &'a Client,
        catalog: Catalog,
        next: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        Box::pin(async move {
            match catalog {
                Catalog::Library => library(http, "", next).await,
                Catalog::Popular | Catalog::Latest => bail!("Kindle JP has no public catalogs"),
            }
        })
    }

    fn series<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Series>> {
        Box::pin(async move {
            let book = book(http, url).await?;
            Ok(Series {
                chapters: vec![ChapterLink {
                    title: book.title.clone(),
                    url: format!("{SITE}manga/{}", book.asin),
                    locked: false,
                }],
                title: book.title,
                cover: Some(cover(&book.asin)),
            })
        })
    }

    fn chapter<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Chapter>> {
        Box::pin(async move {
            let book = book(http, url).await?;
            let initial = render(&book, 0, true).await?;
            let locations = initial
                .locations
                .as_ref()
                .context("Kindle render has no location map")?;
            let mut positions = HashSet::new();
            let mut seen = HashSet::new();
            let mut pages = Vec::new();
            // Locations overlap and a spread can contain multiple images.
            // Keep the first occurrence of each element in source order.
            for &position in locations {
                if !positions.insert(position) {
                    continue;
                }
                let subsequent;
                let rendered = if position == 0 {
                    &initial
                } else {
                    subsequent = render(&book, position, false).await?;
                    &subsequent
                };
                let Manifest { cdn, cdn_resources } = &rendered.manifest;
                for element in rendered.pages.iter().flat_map(|page| &page.children) {
                    let Some(reference) = &element.image_reference else {
                        continue;
                    };
                    if !seen.insert(element.element_id) {
                        continue;
                    }
                    let resource = cdn_resources
                        .iter()
                        .find(|resource| &resource.url == reference)
                        .context("Kindle image is missing from its manifest")?;
                    let mut url = image_url(&format!(
                        "{}/{}",
                        cdn.base_url.trim_end_matches('/'),
                        resource.url
                    ))?;
                    url.set_query(
                        cdn.auth_parameter
                            .as_deref()
                            .or(resource.auth_parameter.as_deref()),
                    );
                    // Append only for encrypted images: existing signed query bytes
                    // must stay exactly as the manifest gave them.
                    let token = &book.karamel_token;
                    if cdn.is_encrypted {
                        url.query_pairs_mut()
                            .append_pair("token", &token.token)
                            .append_pair("expiration", &token.expires_at.to_string());
                    }
                    pages.push(serde_json::to_string(&Page {
                        url: url.into(),
                        token: cdn.is_encrypted.then(|| token.clone()),
                    })?);
                }
            }
            ensure!(!pages.is_empty(), "Kindle book has no manga images");
            Ok(Chapter {
                title: book.title,
                series: None,
                direction: None,
                first_page: None,
                pages,
            })
        })
    }

    fn page<'a>(&'a self, _http: &'a Client, page: &'a str) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            let page: Page = serde_json::from_str(page)?;
            let url = image_url(&page.url)?;
            let response = signed_http()?
                .get(url)
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .map_err(reqwest::Error::without_url)?;
            let bytes = response
                .bytes()
                .await
                .map_err(reqwest::Error::without_url)?;
            let Some(token) = page.token else {
                return Ok(bytes.to_vec());
            };
            let offset = (token.expires_at % 60) as usize;
            let secret = token
                .token
                .as_bytes()
                .get(offset..offset + 40)
                .context("Kindle rendering token is too short")?;
            let (prefix, ciphertext) = bytes
                .split_at_checked(48)
                .context("Truncated Kindle encrypted image")?;
            let salt = STANDARD.decode(&prefix[..24])?;
            let iv: [u8; 16] = STANDARD
                .decode(&prefix[24..])?
                .try_into()
                .map_err(|_| anyhow!("Invalid Kindle image IV"))?;
            let ciphertext = STANDARD.decode(ciphertext)?;
            let key = pbkdf2::pbkdf2_hmac_array::<Sha256, 16>(secret, &salt, 1000);
            // Kindle uses a 16-byte IV, not AES-GCM's usual 12-byte nonce,
            // and authenticates the first nine bytes of the token secret.
            AesGcm::<Aes128, U16>::new(&key.into())
                .decrypt(
                    &iv.into(),
                    Payload {
                        msg: &ciphertext,
                        aad: &secret[..9],
                    },
                )
                .map_err(|_| anyhow!("Kindle image authentication failed"))
        })
    }
}

async fn book(http: &Client, url: &Url) -> Result<Book> {
    ensure!(Kindle.reads(url), "Not a Kindle JP reader URL");
    let asin = url
        .path()
        .strip_prefix("/manga/")
        .filter(|asin| is_asin(asin))
        .context("Use a Kindle JP manga reader URL")?;
    // Only the reader page needs the browser's session cookies.
    let response = http
        .get(format!("{SITE}manga/{asin}"))
        .header(reqwest::header::USER_AGENT, READER_AGENT)
        .timeout(TIMEOUT)
        .send()
        .await?;
    let html = response.error_for_status()?.text().await?;
    let page = Html::parse_document(&html);
    let selector = Selector::parse("#bookInfo").expect("a valid selector");
    let data = page
        .select(&selector)
        .next()
        .context("Kindle reader has no bookInfo; sign in and check access to this manga")?
        .text()
        .collect::<String>();
    serde_json::from_str(&data).context("Invalid Kindle bookInfo")
}

fn is_asin(asin: &str) -> bool {
    asin.len() == 10
        && asin
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

fn cover(asin: &str) -> String {
    format!("https://m.media-amazon.com/images/P/{asin}.09.L.jpg")
}

async fn library(http: &Client, query: &str, next: Option<&str>) -> Result<Listing> {
    let mut next = next.map(str::to_owned);
    loop {
        let mut url = Url::parse(SITE)?.join("kindle-library/search")?;
        url.query_pairs_mut().extend_pairs([
            ("libraryType", "MANGA"),
            ("sortType", "recency"),
            ("querySize", "50"),
            ("query", query),
        ]);
        if let Some(next) = &next {
            url.query_pairs_mut().append_pair("paginationToken", next);
        }
        let library: Library = http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        next = library.pagination_token.filter(|token| !token.is_empty());
        let series = library
            .items_list
            .into_iter()
            .map(|book| SeriesLink {
                title: book.title,
                url: format!("{SITE}manga/{}", book.asin),
                cover: Some(book.product_url),
            })
            .collect::<Vec<_>>();
        // Amazon filters manga after paging the library, so a page can have
        // no manga while there are still more books to read.
        if !series.is_empty() || next.is_none() {
            return Ok(Listing { series, next });
        }
    }
}

/// Token-authenticated requests must not forward their token through redirects
/// or send browser cookies to the CDN. Reqwest's redirect policy is client-wide.
fn signed_http() -> Result<&'static Client> {
    static HTTP: LazyLock<Result<Client, reqwest::Error>> = LazyLock::new(|| {
        Client::builder()
            .user_agent(READER_AGENT)
            .redirect(Policy::custom(|attempt| {
                attempt.error("Kindle request redirected")
            }))
            .timeout(TIMEOUT)
            .build()
    });
    HTTP.as_ref()
        .map_err(|error| anyhow!("Kindle HTTP client: {error}"))
}

async fn render(book: &Book, position: u64, location_map: bool) -> Result<Render> {
    let mut url = Url::parse(SITE)?.join("renderer/render")?;
    url.query_pairs_mut().extend_pairs([
        ("version", "3.0"),
        ("asin", &book.asin),
        ("contentType", "FullBook"),
        ("revision", &book.content_guid),
        ("fontFamily", "Bookerly"),
        ("fontSize", "4.95"),
        ("lineHeight", "1.4"),
        ("dpi", "160"),
        ("height", "808"),
        ("width", "2560"),
        ("marginBottom", "0"),
        ("marginLeft", "9"),
        ("marginRight", "9"),
        ("marginTop", "0"),
        ("maxNumberColumns", "1"),
        ("theme", "dark"),
        ("packageType", "TAR"),
        ("encryptionVersion", "NONE"),
        ("numPage", "1"),
        ("skipPageCount", "0"),
        ("startingPosition", &position.to_string()),
        ("bundleImages", "false"),
    ]);
    if location_map {
        url.query_pairs_mut().append_pair("locationMap", "true");
    }
    let mut token = HeaderValue::from_str(&book.karamel_token.token)?;
    token.set_sensitive(true);
    let response = signed_http()?
        .get(url)
        .header("Accept", "application/x-tar")
        .header("x-amz-rendering-token", token)
        .send()
        .await?
        .error_for_status()?;
    let bytes = response.bytes().await?;
    let mut archive = tar::Archive::new(bytes.as_ref());
    let mut manifest = None;
    let mut locations = None;
    let mut pages = Vec::new();
    for entry in archive.entries()? {
        let entry = entry?;
        match entry.path()?.to_str() {
            Some("manifest.json") => manifest = Some(serde_json::from_reader(entry)?),
            Some("location_map.json") => {
                #[derive(Deserialize)]
                struct Locations {
                    locations: Vec<u64>,
                }
                locations = Some(serde_json::from_reader::<_, Locations>(entry)?.locations);
            }
            Some(name) if name.starts_with("page_data_") => {
                pages.extend(serde_json::from_reader::<_, Vec<RenderedPage>>(entry)?);
            }
            _ => {}
        }
    }
    Ok(Render {
        manifest: manifest.context("Kindle render has no manifest")?,
        locations,
        pages,
    })
}

fn image_url(address: &str) -> Result<Url> {
    let url = Url::parse(address).context("Invalid Kindle image URL")?;
    ensure!(
        url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|host| host.ends_with(".cloudfront.net"))
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "Unexpected Kindle image host"
    );
    Ok(url)
}
