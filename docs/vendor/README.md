# Vendored third-party scripts

`gsap.min.js` and `ScrollTrigger.min.js` are **GSAP 3.15.0** from <https://gsap.com>,
used by `docs/index.html` for the landing page animations.

These two files are **not** covered by this repository's MIT license. They are used
under GreenSock's Standard "no charge" license: <https://gsap.com/standard-license>.
Their upstream `/*! ... @license ... */` headers must be preserved verbatim.

They are vendored instead of loaded from a CDN so the landing page makes zero
third-party requests and works on networks where CDNs are slow or blocked.

Upstream sources (fetched byte-for-byte, no reformatting):

- `gsap.min.js` — <https://unpkg.com/gsap@3.15.0/dist/gsap.min.js> (72,927 B)
- `ScrollTrigger.min.js` — <https://unpkg.com/gsap@3.15.0/dist/ScrollTrigger.min.js> (44,575 B)

To update, bump the version in both URLs, re-download with `curl -fsSL -o`, and
re-check the header comments survived.
