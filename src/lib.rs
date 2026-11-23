//! The sites Koharu reads: each an [`Extension`] that searches its site,
//! lists its catalogs, and says what a series' chapters and a chapter's
//! pages are there. Koharu builds every one in.

mod comic_walker;
mod gigaviewer;
mod kindle;
mod naver_webtoon;
mod pixiv_comic;
mod webtoons;

use anyhow::Result;
use futures::future::BoxFuture;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use url::Url;

/// Every extension.
pub static EXTENSIONS: &[&dyn Extension] = &[
    &kindle::Kindle,
    &comic_walker::ComicWalker,
    &naver_webtoon::NaverWebtoon,
    &gigaviewer::SHONEN_JUMP_PLUS,
    &pixiv_comic::PixivComic,
    &webtoons::Webtoons,
];

/// The extension that reads `url`.
#[must_use]
pub fn find(url: &Url) -> Option<&'static dyn Extension> {
    EXTENSIONS
        .iter()
        .copied()
        .find(|extension| extension.reads(url))
}

/// A site Koharu reads. `http` sends the cookies of the user's sign-in.
/// An extension can use a separate client for signed requests that must
/// reject redirects and carry no browser cookies.
pub trait Extension: Send + Sync {
    /// Its module's name, which keys it.
    fn id(&self) -> &'static str;

    fn name(&self) -> &'static str;

    /// Whether `url` is an address on its site.
    fn reads(&self, url: &Url) -> bool;

    /// The page the user signs in to its site on, when it has one.
    fn sign_in(&self) -> Option<&'static str> {
        None
    }

    /// The name of the account signed in to its site, `None` when no one
    /// is, as a site without a sign-in always answers.
    fn account<'a>(&'a self, _http: &'a Client) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async { Ok(None) })
    }

    /// The page of its search for `query` that `next` starts, the first
    /// when it's `None`.
    fn search<'a>(
        &'a self,
        http: &'a Client,
        query: &'a str,
        next: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>>;

    /// The lists it has besides its search.
    fn catalogs(&self) -> &'static [Catalog];

    /// The page of one of its `catalogs` that `next` starts, the first when
    /// it's `None`.
    fn catalog<'a>(
        &'a self,
        http: &'a Client,
        catalog: Catalog,
        next: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>>;

    fn series<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Series>>;

    fn chapter<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Chapter>>;

    /// A page's image, from one of the values its chapter gave.
    fn page<'a>(&'a self, http: &'a Client, page: &'a str) -> BoxFuture<'a, Result<Vec<u8>>>;

    /// A series' cover, from the address its series gave.
    fn cover<'a>(&'a self, http: &'a Client, url: &'a Url) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            let response = http.get(url.clone()).send().await?.error_for_status()?;
            Ok(response.bytes().await?.to_vec())
        })
    }
}

/// A list a site has besides its search, as Mihon's catalogue sources have
/// popular and latest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum Catalog {
    Popular,
    /// Recently updated.
    Latest,
    /// Books available to the signed-in account.
    Library,
}

/// One page of a search or a catalog.
#[derive(Clone, Debug, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    pub series: Vec<SeriesLink>,
    /// Where the next page starts, as only its extension reads it; `None`
    /// on the last.
    pub next: Option<String>,
}

/// A series as a list shows it.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, specta::Type)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SeriesLink {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub cover: Option<String>,
}

/// A series: its chapters in reading order.
#[derive(Clone, Debug, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct Series {
    pub title: String,
    pub cover: Option<String>,
    pub chapters: Vec<ChapterLink>,
}

#[derive(Clone, Debug, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ChapterLink {
    pub title: String,
    pub url: String,
    /// Paid and not bought, or otherwise not readable by the signed-in user.
    pub locked: bool,
}

/// A chapter: its pages in reading order, as values only its extension
/// reads, which Koharu hands back to its `page`.
#[derive(Clone, Debug)]
pub struct Chapter {
    pub title: String,
    /// The series' title, which names the chapter's project.
    pub series: Option<String>,
    pub direction: Option<ReadingDirection>,
    /// Which side of a two-page spread its first page takes, as EPUB's
    /// `page-spread-left` and `page-spread-right` place a page; `None` when
    /// the site doesn't say.
    pub first_page: Option<PageSide>,
    pub pages: Vec<String>,
}

/// A side of a two-page spread.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum PageSide {
    Left,
    Right,
}

/// How a chapter's pages turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum ReadingDirection {
    /// Right to left, as Japanese manga turns.
    Rtl,
    Ltr,
    /// One strip, top to bottom, as a webtoon scrolls.
    Vertical,
}

/// The page at `url`, as text.
async fn text(http: &Client, url: &Url) -> Result<String> {
    Ok(http
        .get(url.clone())
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

/// The page number `next` holds, 1 when it's `None`.
fn page(next: Option<&str>) -> Result<u32> {
    Ok(next.map(str::parse).transpose()?.unwrap_or(1))
}
