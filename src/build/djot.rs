use anyhow::{Context, bail};
use jotdown::{Container, Event};
use tera::Value;
use tracing::debug;

use crate::build::{BuildFile, ContentSlug, Frontmatter, MetadataContainer};

mod biblatex;

fn collect_strings(events: &[Event<'_>]) -> (String, usize) {
    let mut content = String::new();
    let mut num_str_events = 0;

    for event in events {
        if let Event::Str(fragment) = event {
            content.push_str(fragment);
            num_str_events += 1;
        } else {
            break;
        }
    }

    (content, num_str_events)
}

#[tracing::instrument(skip_all)]
pub fn render(
    input: &BuildFile,
    metadata: &mut MetadataContainer,
    slug: &ContentSlug,
    content: &str,
) -> anyhow::Result<String> {
    let mut events = jotdown::Parser::new(content).collect::<Vec<_>>();

    'extract_frontmatter: {
        if !matches!(
            &events[..],
            [Event::Start(Container::RawBlock { format: "json" }, _), ..]
        ) {
            debug!("Missing json raw block start, skipping frontmatter");
            break 'extract_frontmatter;
        }

        // We know at this point that we're in a raw json block, so we'll expect the
        // next event(s) to be `Str`
        let (frontmatter, num_str_events) = collect_strings(&events[1..]);

        // Also need the block to terminate
        if !matches!(
            &events[1 + num_str_events],
            Event::End(Container::RawBlock { format: "json" })
        ) {
            debug!("Missing raw block ending, skipping frontmatter");
            break 'extract_frontmatter;
        }

        let frontmatter: Frontmatter =
            serde_json::from_str(&frontmatter).context("failed to parse frontmatter")?;

        debug!(?frontmatter, "Parsed frontmatter from djot file");

        if let Some(map) = frontmatter.0.as_object()
            && let Some(Value::String(bibliography_field)) = map.get("bibliography")
        {
            metadata[slug].bibliography_file = Some(bibliography_field.clone());
        }
        metadata[slug].frontmatter = Some(frontmatter);

        // Remove events from the start
        events.drain(..(1 + num_str_events + 1));
    }

    'find_title: {
        let mut events_it = events
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e, Event::Start(Container::Heading { level: 1, .. }, _)));

        let Some((title_offset, _)) = events_it.next() else {
            debug!("Missing page title start, skipping");
            break 'find_title;
        };

        if events_it.next().is_some() {
            bail!("Found multiple level 1 headers in the same document");
        }

        let (title, num_str_events) = collect_strings(&events[(title_offset + 1)..]);

        if !matches!(
            &events.get(title_offset + num_str_events + 1),
            Some(Event::End(Container::Heading { level: 1, .. }))
        ) {
            debug!("Missing page title end, skipping");
            break 'find_title;
        }

        metadata[slug].title = Some(title);
    }

    biblatex::handle_references(input, metadata, slug, &mut events)
        .context("parsing out citations and inserting reference")?;

    Ok(jotdown::html::render_to_string(events.into_iter()))
}
