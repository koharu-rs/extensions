//! The sites Koharu reads: each an [`Extension`] that says what a series'
//! chapters and a chapter's pages are on its site. Koharu builds every one
//! in.

mod pixiv_comic;
mod webtoons;

use anyhow::Result;
use futures::future::BoxFuture;
use reqwest::Client;
use serde::Serialize;
use url::Url;

/// Every extension.
pub static EXTENSIONS: &[&dyn Extension] = &[&pixiv_comic::PixivComic, &webtoons::Webtoons];

/// The extension that reads `url`.
#[must_use]
pub fn find(url: &Url) -> Option<&'static dyn Extension> {
    EXTENSIONS
        .iter()
        .copied()
        .find(|extension| extension.reads(url))
}

/// A site Koharu reads. Its requests go through `http`, which sends the
/// cookies of the user's sign-in to the site.
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
    pub pages: Vec<String>,
}

/// How a chapter's pages turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, specta::Type)]
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
