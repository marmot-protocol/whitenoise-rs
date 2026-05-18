# Bare URLs

All six recognized schemes, bare (no `<>`):

- https: https://example.com/path?q=1
- http: http://example.com
- mailto: mailto:foo@bar.com
- tel: tel:+15551234567
- whitenoise: whitenoise://group/abc
- whitenoise-staging: whitenoise-staging://group/abc

Mid-sentence: see https://example.com for details.

Trailing-punct stripping: visit https://example.com.

Trailing-punct cluster: really?! https://example.com?!

Balanced parens kept: see https://en.wikipedia.org/wiki/Foo_(bar) — note the closing paren stays.

Unbalanced paren stripped: (see https://example.com).

Word-internal stays literal: xhttps://example.com.

Opaque whitenoise stays literal: whitenoise:foo.

Bare `nostr:` URI uses the structured path:

- nostr:nevent1qzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqzqz

Inside emphasis: *https://example.com* and **https://example.com**.

In a list:

- click https://example.com to continue
- or mailto:help@example.com
- deep link: whitenoise://thread/xyz

In a blockquote:

> visit https://example.com when ready

Angle-bracket form still works for any scheme: <ftp://example.com/file>.
