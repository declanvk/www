use std::{fs, path::Path, sync::LazyLock};

use anyhow::Context;
use hayagriva::{
    BibliographyDriver, BibliographyRequest, BufWriteFormat, CitationItem, CitationRequest,
    ElemChild, ElemMeta, Formatting, Library, RenderedCitation,
    archive::ArchivedStyle,
    citationberg::{
        Display, FontStyle, FontVariant, FontWeight, IndependentStyle, Locale, Style,
        TextDecoration, VerticalAlign,
    },
};
use jotdown::{Attributes, Container, Event};
use tracing::debug;

use crate::build::{BuildFile, ContentSlug, MetadataContainer, djot::collect_strings};

fn read_library_from_file(path: &Path) -> anyhow::Result<Library> {
    let library_content = fs::read_to_string(path).context(format!(
        "reading biblatex library from file [{}]",
        path.display()
    ))?;

    let library = hayagriva::io::from_biblatex_str(&library_content)
        .map_err(|errs| {
            let errors = errs.iter().map(ToString::to_string).collect::<Vec<_>>();
            anyhow::anyhow!(errors[..].join(", "))
        })
        .context("reading library from biblatex source")?;

    Ok(library)
}

static STYLE: LazyLock<IndependentStyle> =
    LazyLock::new(
        || match ArchivedStyle::InstituteOfElectricalAndElectronicsEngineers.get() {
            Style::Independent(style) => style,
            Style::Dependent(style) => panic!("Unexpected dependent style for IEEE! {style:?}"),
        },
    );
static LOCALES: LazyLock<Vec<Locale>> = LazyLock::new(hayagriva::archive::locales);

fn render_citation_to_html(
    citation: &RenderedCitation,
    citations_keys: &[String],
) -> anyhow::Result<String> {
    fn write_css(formatting: &Formatting, buf: &mut String) {
        if formatting.font_style == FontStyle::Italic {
            buf.push_str("font-style: italic;");
        }

        match formatting.font_weight {
            FontWeight::Bold => buf.push_str("font-weight: bold;"),
            FontWeight::Light => buf.push_str("font-weight: lighter;"),
            _ => {},
        }

        if formatting.text_decoration == TextDecoration::Underline {
            buf.push_str("text-decoration: underline;");
        }

        if formatting.font_variant == FontVariant::SmallCaps {
            buf.push_str("font-variant: small-caps;");
        }

        match formatting.vertical_align {
            VerticalAlign::Sub => buf.push_str("vertical-align: sub;"),
            VerticalAlign::Sup => buf.push_str("vertical-align: super;"),
            _ => {},
        }
    }

    let mut buf = String::new();

    let mut stack = vec![];
    stack.extend(citation.citation.0.iter().rev().cloned());
    while let Some(elem) = stack.pop() {
        match elem {
            ElemChild::Text(formatted) => {
                let is_default = formatted.formatting == Formatting::default();
                if !is_default {
                    buf.push_str("<span style=\"");
                    write_css(&formatted.formatting, &mut buf);
                    buf.push_str("\">");
                }
                buf.push_str(&formatted.text);
                if !is_default {
                    buf.push_str("</span>");
                }
            },
            ElemChild::Elem(elem) => {
                let has_link = if let Some(ElemMeta::Entry(entry_idx)) = elem.meta {
                    let key = &citations_keys[entry_idx];

                    buf.push_str("<a href=\"#ref-");
                    buf.push_str(key);
                    buf.push_str("\">");
                    true
                } else {
                    false
                };

                match elem.display {
                    Some(Display::Block) => buf.push_str("<div>\n"),
                    Some(Display::Indent) => buf.push_str("<div style=\"padding-left: 4em;\">"),
                    Some(Display::LeftMargin) => buf.push_str("<div style=\"float: left;\">"),
                    Some(Display::RightInline) => {
                        buf.push_str("<div style=\"float: right; clear: both;\">")
                    },
                    _ => {},
                }

                // Bit of a hack, but we need to push onto the stack `Elem::Markup`s which will
                // close the HTML tags we opened prior to adding the children to the stack. The
                // order also has to be reversed so that the tag close happens in the right
                // order

                if has_link {
                    stack.push(ElemChild::Markup("</a>".into()));
                }

                if elem.display.is_some() {
                    stack.push(ElemChild::Markup("</div>\n".into()));
                }

                stack.extend(elem.children.0.iter().rev().cloned());
            },
            ElemChild::Markup(m) => buf.push_str(&m),
            ElemChild::Link { text, url } => {
                buf.push_str("<a href=\"");
                buf.push_str(&url);
                buf.push_str("\">");
                let is_default = text.formatting == Formatting::default();
                if !is_default {
                    buf.push_str("<span style=\"");
                    write_css(&text.formatting, &mut buf);
                    buf.push_str("\">");
                }
                buf.push_str(&text.text);
                if !is_default {
                    buf.push_str("</span>");
                }
                buf.push_str("</a>")
            },
            ElemChild::Transparent { .. } => {},
        }
    }

    Ok(buf)
}

#[tracing::instrument(skip_all)]
pub fn handle_references(
    input: &BuildFile,
    metadata: &mut MetadataContainer,
    slug: &ContentSlug,
    events: &mut Vec<Event<'_>>,
) -> anyhow::Result<()> {
    let Some(bibliography_path) = &metadata[slug].bibliography_file else {
        debug!("No bibliography file reference found, skipping");
        return Ok(());
    };
    let bibliography_path = input
        .full_path
        .parent()
        .map(Path::to_owned)
        .unwrap_or_default()
        .join(bibliography_path);
    let library = read_library_from_file(&bibliography_path).context("reading biblatex library")?;

    let mut driver = BibliographyDriver::new();

    let citation_offsets = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            // Citations in text are in the format `key1; key2; key3`{=cite}
            matches!(
                event,
                Event::Start(Container::RawInline { format: "cite" }, _)
            )
        })
        .map(|(offset, _)| offset);

    // This loop through the text fines all the in-text citations and records them
    // in order
    let mut citation_spans = vec![];
    let mut citations_keys = vec![];
    for cite_start_offset in citation_offsets {
        let (raw_citations, num_str_events) = collect_strings(&events[(cite_start_offset + 1)..]);

        if !matches!(
            &events.get(cite_start_offset + num_str_events + 1),
            Some(Event::End(Container::RawInline { format: "cite" }))
        ) {
            debug!(cite_start_offset, "Missing citation end, skipping");
            return Ok(());
        }
        citation_spans.push(cite_start_offset..(cite_start_offset + num_str_events + 1 + 1));

        let mut keys = vec![];
        let mut citation_items = vec![];
        for key in raw_citations.split(";").map(str::trim) {
            let Some(entry) = library.get(key) else {
                debug!(key, "Citation key not found in library");
                continue;
            };
            keys.push(key.to_owned());

            citation_items.push(CitationItem::new(entry, None, None, false, None));
        }

        citations_keys.push(keys);
        driver.citation(CitationRequest::new(
            citation_items,
            &STYLE,
            None,
            &LOCALES,
            None,
        ));
    }

    // This loop through the library add all items as hidden so that the
    // bibliography rendered at the end will contain all citations
    for entry in library.iter() {
        let items = vec![CitationItem::new(entry, None, None, true, None)];
        driver.citation(CitationRequest::from_items(items, &STYLE, &LOCALES));
    }

    let rendered = driver.finish(BibliographyRequest {
        style: &STYLE,
        locale: None,
        locale_files: &LOCALES,
    });

    // Now we have to:
    //  1. Remove all the raw inline blocks and replace them with citations and
    //     links
    //  2. Insert a bibliography at the end of the text

    let mut removed_offset = 0;
    for (citation_idx, span) in citation_spans.into_iter().enumerate() {
        let citation = &rendered.citations[citation_idx];
        let rendered_citation = render_citation_to_html(citation, &citations_keys[citation_idx])
            .context("rendering citation to HTML")?;
        let updated_span = (removed_offset + span.start)..(removed_offset + span.end);
        let num_events_removed = events
            .splice(
                updated_span,
                [
                    Event::Start(Container::RawInline { format: "html" }, Attributes::new()),
                    Event::Str(rendered_citation.into()),
                    Event::End(Container::RawInline { format: "html" }),
                ],
            )
            .count();

        removed_offset += num_events_removed - 3;
    }

    let Some(bib) = rendered.bibliography else {
        debug!("No bibliography, skipping adding events");
        return Ok(());
    };

    let mut bibliography_events = vec![];
    let num_bib_items = bib.items.len();
    for (idx, item) in bib.items.into_iter().enumerate() {
        let mut rendered_bib_item = String::new();
        item.content
            .write_buf(&mut rendered_bib_item, BufWriteFormat::Html)
            .context("formatting reference item to HTML")?;
        bibliography_events.extend([
            Event::Start(
                Container::Div {
                    class: "reference-key",
                },
                Attributes::new(),
            ),
            Event::Start(Container::RawBlock { format: "html" }, Attributes::new()),
            Event::Str(format!("<span id=\"ref-{}\">[{}]</span>", item.key, idx + 1).into()),
            Event::End(Container::RawBlock { format: "html" }),
            Event::End(Container::Div {
                class: "reference-key",
            }),
            Event::Start(Container::RawBlock { format: "html" }, Attributes::new()),
            Event::Str("<cite class=\"reference-body\">".into()),
            Event::End(Container::RawBlock { format: "html" }),
            Event::Start(Container::RawBlock { format: "html" }, Attributes::new()),
            Event::Str(rendered_bib_item.into()),
            Event::End(Container::RawBlock { format: "html" }),
            Event::Start(Container::RawBlock { format: "html" }, Attributes::new()),
            Event::Str("</cite>".into()),
            Event::End(Container::RawBlock { format: "html" }),
        ]);

        if idx != num_bib_items - 1 {
            bibliography_events.push(Event::Blankline);
        }
    }

    events.extend(
        [
            Event::Start(
                Container::Section {
                    id: "reference".into(),
                },
                Attributes::new(),
            ),
            Event::Start(
                Container::Heading {
                    level: 2,
                    has_section: true,
                    id: "reference".into(),
                },
                Attributes::new(),
            ),
            Event::Str("Reference".into()),
            Event::End(Container::Heading {
                level: 2,
                has_section: true,
                id: "reference".into(),
            }),
            Event::Start(
                Container::Div {
                    class: "reference-grid",
                },
                Attributes::new(),
            ),
        ]
        .into_iter()
        .chain(bibliography_events)
        .chain([
            Event::End(Container::Div {
                class: "reference-grid",
            }),
            Event::End(Container::Section {
                id: "reference".into(),
            }),
        ]),
    );

    Ok(())
}
