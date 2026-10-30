# Web access

`web_fetch` lets the model read a web page: documentation, an RFC, an issue, a
changelog. It takes an absolute `http://` or `https://` URL and an optional
`offset`, and returns the page as text. It is the only tool that sends a request
over the network on the model's behalf, so the rest of this page is mostly about
what it refuses to reach and who is asked first.

The tool lives in [`src/tools/web.rs`](../crates/polaris_code/src/tools/web.rs)
and is registered by `WebToolsPlugin`. There is no search tool; see
[What is not here](#what-is-not-here).

## Permission

`web_fetch` registers as `ToolPermission::Confirm`, and `WebToolsPlugin`
declares in the `ToolPolicyRegistry` that every attended call needs a person.
No permission mode adds an entry for it.

| Situation | Result |
|---|---|
| `confirm`, `auto-edit` | Asks. The prompt shows the arguments, which include the URL. |
| `auto` | Asks, the same way. The automatic reviewer never sees the call. |
| `plan` | Asks. |
| Unattended (`--print`) | Refused, because the run cannot ask. |
| Unattended with `--allow-tool web_fetch` | Runs. |

A permission rule that allows `web_fetch`, a session grant and the automatic
reviewer never remove the prompt in an attended mode. A deny rule, a ceiling or
a registry that refuses it still refuses the call. See
[`permission-modes.md`](permission-modes.md#tools-that-need-a-person).

Reaching the network is a side effect the user is asked about, so a mode does
not grant it silently. That includes `plan`: the permission ceiling orders the
modes `plan < confirm < auto-edit` by what each one asks and refuses, and a
`plan` that ran `web_fetch` unasked would be more permissive than `confirm` on
this tool, which the ceiling cannot express. For scheduling, `web_fetch` is
classified read-only (`tools::tool_is_read_only`): it changes nothing on this
machine. A batch runs side by side only when every call is read-only and
permitted outright, so several fetches run together only in a run that has
granted the tool; with a prompt in the batch they run one at a time.

## Where requests may go

Page text is untrusted. A page can tell the model to fetch some other address,
and the model's choice of address is the one thing the user did not make. So the
tool reaches public hosts only:

- Only `http` and `https`. A URL with a user name or password is refused; the
  tool sends no credentials and no cookies.
- These address ranges are refused, for IPv4, for IPv6, and for IPv4 addresses
  written inside IPv6 (`::ffff:127.0.0.1`, NAT64, 6to4):
  loopback, private networks, link-local (including the cloud metadata address
  `169.254.169.254`), carrier-grade NAT, multicast, unspecified, documentation,
  benchmarking and other reserved ranges. Alternative spellings such as
  `http://2130706433/` or `http://0x7f.1/` are normalised by the URL parser
  before the check.
- A host name is resolved by the tool, **every** address it returns is checked,
  and the connection is then pinned to exactly those addresses. A second lookup
  cannot return something different between the check and the connection.
  A name with both public and blocked addresses is refused before connecting,
  even when a public address appears first.
- Redirects are followed by the tool, not the HTTP client. Each target is
  checked in full (scheme, credentials, address) before anything is sent to it,
  so a public page cannot redirect the tool to a local service.
- Proxy environment variables (`HTTP_PROXY` and similar) are ignored: a proxy
  resolves names itself, so the checks above could no longer see where a
  request goes. On a network that only allows access through a proxy, the tool
  cannot reach anything.

A refusal says which address and what kind it is, and points to `run_command`
(for example `curl`) for a service on this machine or network, which asks first.

A program that embeds the agent and has its own documentation server can exempt
specific addresses with `WebToolsConfig::with_allowed_address`. The `polaris`
binary exempts none.

## Limits

| Limit | Default | Set with |
|---|---|---|
| Time for the whole fetch, redirects included | 30 s | `with_timeout` |
| Redirects followed | 5 | `with_max_redirects` |
| Bytes downloaded from one response | 2 MiB | `with_max_download_bytes` |
| Bytes of text returned by one call | 32 KiB (the `read_file` cap) | `with_page_bytes` |
| Levels of nested elements in an HTML page | 128 | fixed |
| Children of one element in an HTML page | 10,000 | fixed |

A page past either HTML limit is refused, not cut: the error names the URL and
points to `run_command` (for example `curl`) for reading the part you need. See
[HTML conversion](#html-conversion) for why.

A download that reaches the cap is cut there and the result says so:

```
Note: the download stopped at 2097152 of 6291456 bytes, so the end of the page is missing.
```

## What comes back

```
URL: https://example.com/guide
Redirected from: https://example.com/old-guide
Type: text/html (converted to markdown)
Title: The guide

# The guide
…
… showed bytes 0 to 31870 of 84012 bytes of page text. Next: web_fetch(url="https://example.com/guide", offset=31870)
```

- `URL:` is where the text came from after redirects. `Redirected from:` appears
  only when a redirect happened. Credentials and fragments are never shown.
- `Type:` is the declared media type. HTML is converted to markdown; JSON, XML,
  YAML and `text/*` come back as they are. A response with no content type is
  sniffed (an HTML document is converted; text is returned; bytes with NUL
  characters are refused). Images, PDFs, archives and other binary types are
  refused by name, with `run_command` and `curl -o` as the way to keep the file.
- The charset comes from the `Content-Type` header, then a `<meta charset>` near
  the top of an HTML page, then UTF-8. A byte order mark wins over all three.
- A long page comes back in pieces. A piece ends on a line break when one falls
  in its second half, so the next piece starts at the start of a line. The last
  line names the call that fetches the next piece, and there is no such line on
  the final piece. Every call fetches the page again, so a page that changes
  between calls can shift the pieces.
- An error names the URL and ends with what to do: check the URL, try later, or
  use another tool.

## HTML conversion

The conversion is done by [`htmd`](https://crates.io/crates/htmd), which parses
with html5ever (a full HTML5 parser, so malformed markup is read the way a
browser reads it) and writes markdown: headings, paragraphs, emphasis, links,
lists, block quotes, inline code, code blocks with their language, tables as
pipe tables, and decoded entities. The tool's own part is a short wrapper
([`src/tools/web/html.rs`](../crates/polaris_code/src/tools/web/html.rs)) that
makes three choices.

**What is left out.** Everything a reader of text does not need, which costs
prompt tokens on every later turn: `head`, `script`, `style`, `template`,
`noscript`, `svg`, `iframe`, `canvas`, `object`, `embed`, `video`, `audio`,
`select`, `button`, `textarea`, and the `nav`, `aside` and `footer` landmarks,
with everything inside them. An image is replaced by its alt text, because the
address of a picture is no use as text and a `data:` address can be larger than
the page. If a page is empty once the landmarks are skipped, it is converted
again with them kept, so a site that wraps its whole body in one of them still
comes back.

**Where the page's own details go.** The `<title>` is reported on the `Title:`
line and not repeated in the body. Relative links are resolved against the
page's final URL, so a link is usable as written.

**What shapes of page are refused.** html5ever has no nesting limit, and its
cost and `htmd`'s grow faster than the size of the page when elements nest
deeply or when one element has very many children. Measured in a debug build,
100,000 nested `<div>` (500 KB) took five minutes to parse; between 512 and 768
levels of nested `<div>` (fewer for tables) overflowed a 2 MiB thread stack in
the converter, which walks the page by recursion, and an overflow ends the whole
program; and 260,000 adjacent `<b>` elements (2 MB) took 88 seconds to convert.
So the page is fed to the parser in pieces of 4 KiB and stopped once it nests
deeper than 128 levels or an element has more than 10,000 children, and the
finished page is checked against the same limits before it is converted.
Browsers apply a similar limit (Chrome stops at 512 levels); real pages stay far
below 128. The conversion also runs on a blocking thread inside the fetch
deadline, so a fault in it ends that one call.

Nothing is executed, so a page that builds its content with JavaScript returns
only what its HTML already holds.

## Secrets and size

A `web_fetch` result is an ordinary tool result. It passes through the same
secret redaction as every other tool ([`secret-redaction.md`](secret-redaction.md)),
so a credential-shaped value on a page is replaced with a `[REDACTED:…]` marker
before the model, the transcript, the session file or the log see it. The
conversion leaves those shapes intact for the redactor to find. Redaction is an
accident control, not a boundary; the address checks above are the boundary.

The result follows the conventions in [`tool-results.md`](tool-results.md#web-pages):
it says what it is, how much of the whole it is, and which call gets the rest.

## What is not here

- **No search.** A search tool needs a search backend, and the framework this
  workspace builds on provides neither a provider-hosted search nor a search
  client. Adding one without a backend would be a tool that cannot work. A model
  that needs a page finds its URL from the user, the repository, or an earlier
  page.
- **No JavaScript, cookies, logins or form submission.** Pages behind a login
  answer `401` or `403` and the error says so.
- **No file downloads.** Binary responses are refused; use `run_command`.
- **No `file://`, `ftp://` or other schemes.**
