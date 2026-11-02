//! HTML to markdown for pages fetched by `web_fetch`.
//!
//! The conversion is `htmd` on top of html5ever. This module only adds what
//! reading an untrusted page needs:
//!
//! - **Limits on the page's shape.** html5ever has no nesting limit, the
//!   parser's cost grows with the square of the depth (100,000 nested `<div>`
//!   take minutes), and `htmd` walks the tree by recursion, so a few thousand
//!   levels overflow the stack and abort the process. `htmd` also takes time
//!   quadratic in the number of adjacent inline siblings. A page past
//!   [`MAX_DEPTH`] levels or [`MAX_CHILDREN`] children of one element is
//!   refused, and the parser is fed in small pieces so that the limits are
//!   checked before the cost builds up.
//! - **What a reader of text does not need.** Scripts, styles, form widgets,
//!   embedded media and the `nav`, `aside` and `footer` landmarks are skipped;
//!   an image becomes its alt text. If a page is empty once the landmarks are
//!   skipped, it is converted again with them kept, so a site that wraps its
//!   whole body in one of them still comes back.
//! - **The page's own address.** Relative links are resolved against the page
//!   URL, and the document title is reported separately from the body.

use std::rc::Rc;

use htmd::element_handler::{HandlerResult, Handlers};
use htmd::{Element, HtmlToMarkdown};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{ParseOpts, parse_document};
use markup5ever_rcdom::{Node, NodeData, RcDom};
use reqwest::Url;

/// Deepest nesting of elements that is converted. Real pages stay under a few
/// dozen levels; the deepest shape `htmd` handles with the least stack (nested
/// tables) is known to survive twice this on a 2 MiB thread stack in a debug
/// build.
const MAX_DEPTH: usize = 128;

/// Most children one element may have.
const MAX_CHILDREN: usize = 10_000;

/// Bytes handed to the parser at a time. A piece of this size can add at most
/// a thousand levels of nesting, so the limits are noticed long before the
/// parser's cost for a deep page becomes noticeable.
const PARSE_PIECE: usize = 4096;

/// Longest title reported, in characters.
const MAX_TITLE: usize = 200;

/// Elements that carry nothing a reader needs, with everything inside them.
const NOISE: &[&str] = &[
    "head", "title", "script", "style", "template", "noscript", "svg", "iframe", "canvas",
    "object", "embed", "video", "audio", "select", "button", "textarea",
];

/// Page furniture, skipped unless that leaves nothing.
const LANDMARKS: &[&str] = &["nav", "aside", "footer"];

/// A page converted for reading.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Rendered {
    /// The document title, when it has a non-empty one.
    pub(super) title: Option<String>,
    /// The page body as markdown.
    pub(super) text: String,
}

/// Convert `html` to markdown, resolving relative links against `base`.
///
/// # Errors
/// A message ending in what to do instead, when the page is shaped so that it
/// cannot be converted safely.
pub(super) fn to_markdown(html: &str, base: &Url) -> Result<Rendered, String> {
    let tree = parse(html)?;
    let title = prepare(&tree, base)?;
    let mut text = converter(true).tree_to_markdown(&tree);
    if text.trim().is_empty() {
        text = converter(false).tree_to_markdown(&tree);
    }
    Ok(Rendered { title, text })
}

/// The converter, with or without the landmarks skipped.
fn converter(skip_landmarks: bool) -> HtmlToMarkdown {
    let mut skipped = NOISE.to_vec();
    if skip_landmarks {
        skipped.extend_from_slice(LANDMARKS);
    }
    HtmlToMarkdown::builder()
        .skip_tags(skipped)
        .add_handler(vec!["img"], alt_text)
        .build()
}

/// An image is its alt text. The address of a picture is no use to a reader of
/// text, and a `data:` address can be larger than the rest of the page.
fn alt_text(_: &dyn Handlers, element: Element) -> Option<HandlerResult> {
    let alt = element.attrs.iter().find(|a| &*a.name.local == "alt")?;
    let alt = alt.value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!alt.is_empty()).then(|| HandlerResult::from(alt))
}

/// Parse `html`, refusing it as soon as its newest branch gets too deep or too
/// wide.
fn parse(html: &str) -> Result<Rc<Node>, String> {
    let mut parser = parse_document(RcDom::default(), ParseOpts::default());
    let mut rest = html;
    while !rest.is_empty() {
        let mut end = rest.len().min(PARSE_PIECE);
        while !rest.is_char_boundary(end) {
            end += 1;
        }
        let (piece, tail) = rest.split_at(end);
        parser.process(StrTendril::from_slice(piece));
        rest = tail;
        check_newest_branch(&parser.tokenizer.sink.sink.document)?;
    }
    Ok(parser.finish().document)
}

/// Check the chain of last children from `root`: the branch the parser is
/// still adding to, and the only one whose cost is still growing.
fn check_newest_branch(root: &Rc<Node>) -> Result<(), String> {
    let mut node = Rc::clone(root);
    for depth in 1.. {
        if depth > MAX_DEPTH {
            return Err(too_deep());
        }
        let next = {
            let children = node.children.borrow();
            if children.len() > MAX_CHILDREN {
                return Err(too_wide());
            }
            children.last().map(Rc::clone)
        };
        match next {
            Some(child) => node = child,
            None => break,
        }
    }
    Ok(())
}

/// Check the whole tree against the limits, resolve every link against `base`
/// and return the document title.
///
/// Walks with a list, not recursion, so that no page can exhaust the stack here.
fn prepare(root: &Rc<Node>, base: &Url) -> Result<Option<String>, String> {
    let mut title = None;
    let mut pending = vec![(Rc::clone(root), 1_usize)];
    while let Some((node, depth)) = pending.pop() {
        if depth > MAX_DEPTH {
            return Err(too_deep());
        }
        let children = node.children.borrow();
        if children.len() > MAX_CHILDREN {
            return Err(too_wide());
        }
        if let NodeData::Element { name, attrs, .. } = &node.data {
            match &*name.local {
                "a" => {
                    for attr in attrs.borrow_mut().iter_mut() {
                        if &*attr.name.local == "href"
                            && let Ok(resolved) = base.join(&attr.value)
                        {
                            attr.value = StrTendril::from_slice(resolved.as_str());
                        }
                    }
                }
                "head" => title = title.or_else(|| head_title(&children)),
                _ => {}
            }
        }
        pending.extend(children.iter().map(|child| (Rc::clone(child), depth + 1)));
    }
    Ok(title)
}

/// The text of the `<title>` among a `<head>`'s children, with its whitespace
/// collapsed.
fn head_title(children: &[Rc<Node>]) -> Option<String> {
    let title = children.iter().find(
        |child| matches!(&child.data, NodeData::Element { name, .. } if &*name.local == "title"),
    )?;
    let mut text = String::new();
    for part in title.children.borrow().iter() {
        if let NodeData::Text { contents } = &part.data {
            text.push_str(&contents.borrow());
        }
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then(|| text.chars().take(MAX_TITLE).collect())
}

fn too_deep() -> String {
    format!(
        "the page nests elements more than {MAX_DEPTH} levels deep, which is more than can be \
         converted safely; fetch it with run_command (for example curl) and read the part you need"
    )
}

fn too_wide() -> String {
    format!(
        "an element on the page has more than {MAX_CHILDREN} children, which is more than can be \
         converted in reasonable time; fetch it with run_command (for example curl) and read the \
         part you need"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/docs/guide").unwrap()
    }

    fn md(html: &str) -> String {
        to_markdown(html, &base()).unwrap().text
    }

    #[test]
    fn a_page_comes_back_as_readable_markdown() {
        let text = md("<h1>Install</h1><p>Run <code>cargo add x</code> and read \
             <a href=\"next\">the docs</a>.</p><ul><li>one</li><li>two</li></ul>\
             <pre><code class=\"language-rust\">fn main() {}</code></pre>\
             <table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table>");
        assert!(text.contains("# Install"), "{text}");
        assert!(text.contains("`cargo add x`"), "{text}");
        assert!(
            text.contains("[the docs](https://example.com/docs/next)"),
            "{text}"
        );
        assert!(text.contains("one") && text.contains("two"), "{text}");
        assert!(text.contains("```rust\nfn main() {}\n```"), "{text}");
        assert!(text.contains("| a | b |"), "{text}");
    }

    #[test]
    fn the_title_is_reported_and_not_part_of_the_body() {
        let page = to_markdown(
            "<head><title> A &amp; B \n page </title></head><body><p>Hi</p></body>",
            &base(),
        )
        .unwrap();
        assert_eq!(page.title.as_deref(), Some("A & B page"));
        assert_eq!(page.text, "Hi");
    }

    #[test]
    fn a_long_title_is_cut() {
        let html = format!("<title>{}</title><p>x</p>", "t".repeat(1000));
        let title = to_markdown(&html, &base()).unwrap().title.unwrap();
        assert_eq!(title.chars().count(), MAX_TITLE);
    }

    #[test]
    fn a_page_without_a_title_has_none() {
        assert_eq!(to_markdown("<p>x</p>", &base()).unwrap().title, None);
        assert_eq!(
            to_markdown("<title>  </title><p>x</p>", &base())
                .unwrap()
                .title,
            None
        );
    }

    #[test]
    fn scripts_styles_widgets_and_landmarks_are_dropped() {
        let text = md(
            "<body><nav>Home | About</nav><script>var secret = 1;</script>\
             <style>p { color: red }</style><noscript>enable js</noscript>\
             <select><option>Pick</option></select><button>Copy</button>\
             <svg><title>icon</title><text>glyph</text></svg>\
             <main><p>Body</p></main><aside>Related</aside><footer>Copyright</footer></body>",
        );
        assert_eq!(text, "Body");
    }

    #[test]
    fn a_page_wholly_inside_a_landmark_is_still_returned() {
        assert_eq!(
            md("<body><footer><p>All of it</p></footer></body>"),
            "All of it"
        );
    }

    #[test]
    fn links_are_resolved_against_the_page() {
        let text = md("<a href=\"/abs\">a</a> <a href=\"../up\">b</a> \
             <a href=\"https://other.org/x\">c</a> <a href=\"#top\">d</a> <a>e</a>");
        assert!(text.contains("[a](https://example.com/abs)"), "{text}");
        assert!(text.contains("[b](https://example.com/up)"), "{text}");
        assert!(text.contains("[c](https://other.org/x)"), "{text}");
        assert!(
            text.contains("[d](https://example.com/docs/guide#top)"),
            "{text}"
        );
    }

    #[test]
    fn an_image_is_its_alt_text_and_nothing_else() {
        let text = md(
            "<p>See <img src=\"data:image/png;base64,AAAA\" alt=\" the  diagram \"> \
             and <img src=\"/x.png\"> here.</p>",
        );
        assert!(text.contains("the diagram"), "{text}");
        assert!(
            !text.contains("base64") && !text.contains("x.png"),
            "{text}"
        );
    }

    #[test]
    fn multibyte_text_survives_the_piece_boundaries() {
        // A page several pieces long whose characters straddle every boundary.
        let words = "héllo wörld ✓ ".repeat(2000);
        let text = md(&format!("<p>{words}</p>"));
        assert_eq!(
            text.split_whitespace().count(),
            6000,
            "{} bytes",
            text.len()
        );
        assert_eq!(text, words.trim_end());
    }

    #[test]
    fn nesting_past_the_limit_is_refused_before_it_costs_anything() {
        // Parsed whole, this takes minutes and converting it overflows the stack.
        let hostile = "<div>".repeat(200_000);
        let error = to_markdown(&hostile, &base()).unwrap_err();
        assert!(error.contains("levels deep"), "{error}");
        assert!(error.contains("run_command"), "{error}");
    }

    #[test]
    fn nesting_up_to_the_limit_is_converted() {
        // The document, html and body take three levels and the text one more.
        let divs = MAX_DEPTH - 4;
        let page = format!("{}deep{}", "<div>".repeat(divs), "</div>".repeat(divs));
        assert_eq!(md(&page), "deep");
        let over = format!(
            "{}deep{}",
            "<div>".repeat(divs + 1),
            "</div>".repeat(divs + 1)
        );
        assert!(to_markdown(&over, &base()).is_err());
    }

    #[test]
    fn deep_nesting_is_refused_even_when_it_closes_within_one_piece() {
        // The newest branch is shallow again by the end of the piece, so only
        // the check of the whole tree sees it.
        let page = format!(
            "{}x{}<p>after</p>",
            "<span>".repeat(MAX_DEPTH + 5),
            "</span>".repeat(MAX_DEPTH + 5)
        );
        assert!(
            to_markdown(&page, &base())
                .unwrap_err()
                .contains("levels deep")
        );
    }

    #[test]
    fn too_many_siblings_are_refused_before_they_cost_anything() {
        // Converted, this many adjacent inline siblings take over a minute.
        let hostile = "<b>a</b>".repeat(MAX_CHILDREN + 1);
        let error = to_markdown(&hostile, &base()).unwrap_err();
        assert!(error.contains("children"), "{error}");
    }
    #[test]
    fn prepare_refuses_a_wide_tree_independently_of_incremental_parsing() {
        let tree = RcDom::default();
        // Directly construct the boundary shape so parse() cannot intercept it.
        tree.document
            .children
            .borrow_mut()
            .extend((0..=MAX_CHILDREN).map(|_| {
                Node::new(NodeData::Comment {
                    contents: StrTendril::from_slice("sibling"),
                })
            }));
        assert_eq!(prepare(&tree.document, &base()).unwrap_err(), too_wide());
    }
}
