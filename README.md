# Koharu extensions

The sites [Koharu](https://github.com/koharu-rs/koharu) reads, each a module
implementing `Extension`: whether an address is on its site, what a series'
chapters and a chapter's pages are there, and a page's image. Koharu builds
every one in, pinned to a commit of this repository, and sends their requests
with the cookies of the user's sign-in to the site.

To add a site, add its module and list it in `EXTENSIONS`. To try one in
Koharu before it's merged, patch Koharu's dependency on this repository to a
local checkout, and leave the patch out of Koharu's commits:

```toml
[patch."https://github.com/koharu-rs/extensions"]
koharu-extensions = { path = "../extensions" }
```
