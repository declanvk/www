use std::{
    any::{Any, TypeId},
    collections::{BTreeMap, HashMap},
    ffi::{OsStr, OsString},
    fs::{self, DirEntry},
    io,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, bail};
use argh::FromArgs;
use jotdown::{self, Container, Event};
use serde::{Deserialize, Serialize};
use tera::Tera;
use tracing::{debug, instrument};

/// Build the static site.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "build")]
pub struct BuildCmd {
    /// path to the input directory
    #[argh(positional)]
    input_path: PathBuf,

    /// path to the output directory
    #[argh(positional)]
    output_path: PathBuf,

    /// render the site without debug information
    #[argh(switch)]
    release: bool,
}

impl BuildCmd {
    fn template_dir(&self) -> PathBuf {
        self.input_path.join("templates")
    }

    fn output_folder(&self, content_slug: &ContentSlug) -> PathBuf {
        let Some(slug_parent) = content_slug.0.parent() else {
            return self.output_path.clone();
        };

        self.output_path.join(slug_parent)
    }
}

#[derive(Debug)]
struct BuildFile {
    full_path: PathBuf,
}

#[derive(Debug)]
struct BuildDirFiles {
    files: BTreeMap<PathBuf, BuildFile>,
}

impl BuildDirFiles {
    fn gather(content_root: &Path) -> anyhow::Result<Self> {
        let mut pages = BTreeMap::new();

        Self::visit_dirs(content_root, &mut |entry| {
            let path = entry.path();
            let page = BuildFile { full_path: path };

            let key = entry
                .path()
                .strip_prefix(content_root)
                .context(format!(
                    "Unable to strip prefix from page [{}]",
                    page.full_path.display()
                ))?
                .to_path_buf();

            pages.insert(key, page);

            Ok(())
        })?;

        Ok(Self { files: pages })
    }

    fn visit_dirs(
        dir: &Path,
        cb: &mut impl FnMut(&DirEntry) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        if dir.is_dir() {
            for entry in fs::read_dir(dir)
                .context(format!("failed to read [{}] directory", dir.display()))?
            {
                let entry = entry.context(format!(
                    "failed to read directory entry in [{}]",
                    dir.display()
                ))?;
                let path = entry.path();
                if path.is_dir() {
                    Self::visit_dirs(&path, cb)?;
                } else {
                    cb(&entry).context(format!("callback for [{}] failed", path.display()))?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
struct ContentSlug(PathBuf);

#[derive(Debug)]
struct Content {
    files: BTreeMap<ContentSlug, ContentFile>,
}

#[derive(Debug)]
enum MediaType {
    Other(Option<String>),
    Djot,
    Html,
}

impl MediaType {
    fn extension(&self) -> String {
        match self {
            MediaType::Other(ext) => ext.as_ref().cloned().unwrap_or_default(),
            MediaType::Djot => "dj".into(),
            MediaType::Html => "html".into(),
        }
    }
}

#[derive(Debug)]
enum Transform {
    ReadDjotFrontMatter,
    ReadDjotTitle,
    DjotToHtml,
    ApplyTemplate,
}

#[derive(Debug, Default)]
struct Metadata {
    map: HashMap<TypeId, Box<dyn Any>>,
}

impl Metadata {
    fn insert(&mut self, value: impl Any) -> bool {
        let type_id = value.type_id();
        self.map.insert(type_id, Box::new(value)).is_some()
    }

    fn remove<T: Any>(&mut self) -> Option<Box<T>> {
        let Some(value) = self.map.remove(&TypeId::of::<T>()) else {
            return None;
        };

        Some(
            value
                .downcast::<T>()
                .expect("should have value of type `T`"),
        )
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(transparent)]
struct Frontmatter(tera::Value);

#[derive(Debug, Serialize)]
#[serde(transparent)]
struct PageTitle(String);

#[derive(Debug)]
struct ContentFile {
    input: BuildFile,
    metadata: Metadata,
    current_media_type: MediaType,
    plan: Vec<Transform>,
}

impl ContentFile {
    fn from_input(input: BuildFile) -> Self {
        let current_media_type = match input.full_path.extension().and_then(OsStr::to_str) {
            Some("dj") => MediaType::Djot,
            Some("html") => MediaType::Html,
            Some(other) => MediaType::Other(Some(other.into())),
            None => MediaType::Other(None),
        };

        let mut file = Self {
            input,
            current_media_type,
            plan: vec![],
            metadata: Metadata::default(),
        };

        // Add steps to the plan based on various characteristics
        // The order here is also very important

        if matches!(file.current_media_type, MediaType::Djot) {
            file.plan.push(Transform::ReadDjotFrontMatter);
            file.plan.push(Transform::ReadDjotTitle);
            file.plan.push(Transform::DjotToHtml);
            file.current_media_type = MediaType::Html;
        }

        if matches!(file.current_media_type, MediaType::Html) {
            file.plan.push(Transform::ApplyTemplate);
        }

        file
    }

    fn output_filename(&mut self) -> OsString {
        let mut full_path = self.input.full_path.clone();
        full_path.set_extension(self.current_media_type.extension());

        full_path.file_name().unwrap_or_default().to_owned()
    }

    #[instrument(skip_all, fields(slug = %content_slug.0.display()))]
    fn process(
        mut self,
        args: &BuildCmd,
        tera: &Tera,
        templates: &Templates,
        content_slug: ContentSlug,
    ) -> anyhow::Result<()> {
        let output_folder = self.create_output_parent(args, &content_slug)?;
        if self.plan.is_empty() {
            debug!("Plan is empty, copying file directly to output location");
            let output_path = output_folder.join(self.output_filename());

            fs::copy(self.input.full_path, output_path).context("failed to copy file to output")?;
            return Ok(());
        }

        let mut content =
            fs::read_to_string(&self.input.full_path).context("failed to read content file")?;

        for step in self.plan.drain(..) {
            debug!(?step, "Applying step");
            match step {
                Transform::ReadDjotFrontMatter => {
                    let mut events = jotdown::Parser::new(&content).into_offset_iter();

                    let Some((Event::Start(Container::RawBlock { format: "json" }, _), _)) =
                        events.next()
                    else {
                        debug!("Missing json raw block start, skipping frontmatter");
                        continue;
                    };

                    // We know at this point that we're in a raw json block, so we'll expect the
                    // next event(s) to be `Str`
                    let mut frontmatter = String::new();
                    let next_event = loop {
                        let next = events.next();
                        match next {
                            Some((Event::Str(fragment), _)) => {
                                frontmatter.push_str(&fragment);
                            },
                            Some(_) | None => break next,
                        }
                    };

                    // Also need the block to terminate
                    let Some((Event::End(Container::RawBlock { format: "json" }), end_span)) =
                        next_event
                    else {
                        debug!("Missing raw block ending, skipping frontmatter");
                        continue;
                    };

                    let frontmatter: Frontmatter = serde_json::from_str(&frontmatter)
                        .context("failed to parse frontmatter")?;

                    debug!(?frontmatter, "Parsed frontmatter from djot file");

                    self.metadata.insert(frontmatter);

                    // We can remove the frontmatter from the content at this
                    // point
                    let remainder = content.split_off(end_span.end);
                    content = remainder;
                },
                Transform::ReadDjotTitle => {
                    let mut events = jotdown::Parser::new(&content).skip_while(|e| {
                        !matches!(e, Event::Start(Container::Heading { level: 1, .. }, _))
                    });

                    let Some(Event::Start(Container::Heading { level: 1, .. }, _)) = events.next()
                    else {
                        debug!("Missing page title start, skipping");
                        continue;
                    };

                    // We know at this point that we're in a raw json block, so we'll expect the
                    // next event(s) to be `Str`
                    let mut title = String::new();
                    let next_event = loop {
                        let next = events.next();
                        match next {
                            Some(Event::Str(fragment)) => {
                                title.push_str(&fragment);
                            },
                            Some(Event::Softbreak) => continue,
                            Some(_) | None => break next,
                        }
                    };

                    // Also need the block to terminate
                    let Some(Event::End(Container::Heading { level: 1, .. })) = next_event else {
                        debug!("Missing page title end, skipping");
                        continue;
                    };

                    self.metadata.insert(PageTitle(title));
                },
                Transform::DjotToHtml => {
                    let parser = jotdown::Parser::new(&content);
                    content = jotdown::html::render_to_string(parser);
                },
                Transform::ApplyTemplate => {
                    let Some(template) =
                        templates.find_template(&content_slug, &self.current_media_type)
                    else {
                        debug!(content_slug = %content_slug.0.display(), "Did not find template for content");
                        continue;
                    };

                    let template_path = &template
                        .full_path
                        .strip_prefix(args.template_dir())
                        .unwrap();
                    debug!(template = %template_path.display(), "Rendering with template");
                    let metadata = self
                        .metadata
                        .remove::<Frontmatter>()
                        .map(|f| *f)
                        .unwrap_or_default();
                    let title = self.metadata.remove::<PageTitle>().map(|f| *f);
                    let context = PageContext {
                        content,
                        metadata,
                        title,
                        debug: !args.release,
                    };
                    let tera_context = tera::Context::from_serialize(&context)
                        .context("failed to create tera context")?;
                    content = tera
                        .render(template_path.to_str().unwrap(), &tera_context)
                        .context("failed to render template")?;
                },
            }
        }

        let output_path = output_folder.join(self.output_filename());
        debug!(input = %self.input.full_path.display(), output = %output_path.display(), "Ensured output folder for content exists");

        fs::write(&output_path, content).context("failed to write content file")?;
        debug!(output_path = %output_path.display(), "Written content file");

        Ok(())
    }

    fn create_output_parent(
        &self,
        args: &BuildCmd,
        content_slug: &ContentSlug,
    ) -> anyhow::Result<PathBuf> {
        let output_folder = args.output_folder(&content_slug);

        fs::create_dir_all(&output_folder)
            .context("failed to create parent directory for output")?;

        Ok(output_folder)
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
struct TemplateSlug(PathBuf);

#[derive(Debug)]
struct Templates {
    files: BTreeMap<TemplateSlug, BuildFile>,
}

impl Templates {
    fn initialize_template_engine(args: &BuildCmd) -> anyhow::Result<Tera> {
        let template_dir = args.template_dir();
        let template_glob = format!("{}/**/*.html", template_dir.display());
        let tera = Tera::new(&template_glob).context("failed to initialize template engine")?;

        debug!(engine = ?tera, "Created templating engine");

        Ok(tera)
    }

    fn find_template(&self, path: &ContentSlug, media_type: &MediaType) -> Option<&BuildFile> {
        if let Some(file) = self.files.get(&TemplateSlug(path.0.clone())) {
            return Some(file);
        }

        let extension = media_type.extension();
        let mut current_dir = path.0.parent();
        loop {
            let dir = current_dir.unwrap_or_else(|| Path::new(""));

            // Look for the `page.<ext>` in the current directory
            let mut page_path = dir.join("page");
            page_path.set_extension(extension.clone());
            if let Some(file) = self.files.get(&TemplateSlug(page_path)) {
                return Some(file);
            }

            // If `dir` is empty, then we're in the `or_else` case from the top of the loop
            // and there are no more parent dirs to check
            if dir.as_os_str().is_empty() {
                return None;
            }
            current_dir = dir.parent();
        }
    }
}

#[derive(Debug)]
struct Site {
    content: Content,
    templates: Templates,
}

impl Site {
    fn parse(build_files: BuildDirFiles) -> anyhow::Result<Self> {
        let mut content_files = BTreeMap::new();
        let mut templates_files = BTreeMap::new();

        for (path, file) in build_files.files {
            if let Some(first_component) = path.components().next() {
                if first_component.as_os_str() == "content" {
                    // Make sure that there are no content pages named `page.<ext>`, otherwise there
                    // would be some confusion around what the related template is.
                    if path.file_stem().map(|s| s == "page").unwrap_or(false) {
                        bail!(
                            "Cannot have a content page named 'page', found at {}",
                            path.display()
                        )
                    }

                    let sub_path = path.strip_prefix("content")?.to_path_buf();
                    content_files.insert(ContentSlug(sub_path), ContentFile::from_input(file));
                } else if first_component.as_os_str() == "templates" {
                    if path.extension().map(|ext| ext != "html").unwrap_or(true) {
                        bail!(
                            "Template files must be HTML, found [{}] with missing or non-HTML \
                             extension",
                            path.display()
                        );
                    }

                    let sub_path = path.strip_prefix("templates")?.to_path_buf();
                    templates_files.insert(TemplateSlug(sub_path), file);
                } else {
                    debug!(path = %path.display(), "Ignoring file not in a known directory");
                }
            }
        }

        Ok(Site {
            content: Content {
                files: content_files,
            },
            templates: Templates {
                files: templates_files,
            },
        })
    }

    fn format_output(args: &BuildCmd) -> anyhow::Result<()> {
        // Format all code in output using prettier
        // prettier --write --no-config --ignore-path '' site.out/
        let prettier_output = Command::new("prettier")
            .arg("--write")
            .arg("--no-config")
            .arg("--ignore-path")
            .arg("''")
            .arg(args.output_path.display().to_string())
            .output()
            .context("failed to execute  output code using prettier")?;

        if !prettier_output.status.success() {
            let stdout = String::from_utf8_lossy(&prettier_output.stdout);
            let stderr = String::from_utf8_lossy(&prettier_output.stderr);
            debug!(%stdout, %stderr, "Failed 'prettier' output");
            bail!("Execution of 'prettier' returned an unsuccessful status code")
        } else {
            debug!("Successfully executed 'prettier' to format site output")
        }

        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct PageContext {
    content: String,
    metadata: Frontmatter,
    title: Option<PageTitle>,
    debug: bool,
}

#[tracing::instrument(skip_all)]
pub fn build(args: BuildCmd) -> anyhow::Result<()> {
    // Clean site output
    if let Err(err) = fs::remove_dir_all(&args.output_path) {
        match err.kind() {
            io::ErrorKind::NotFound => {
                debug!("Output directory is already missing before build step");
            },
            _ => {
                bail!(
                    "Failed to clean output directory [{}] before build step: {err}",
                    args.output_path.display()
                );
            },
        }
    }

    let build_files = BuildDirFiles::gather(&args.input_path)
        .context("failed to collect input files from directory")?;

    debug!(?build_files, "Collect input build files!");

    // Next steps:
    //  1. Parse the files into a new structure with specific sub-fields for
    //     `content/`, `templates/`, and `static/`
    //  2. `content/` contains all page contents and any assets that are related to
    //     a specific page. `content/` pages are rendered according to their
    //     extension. `*.dj` files are converted to HTML and then treated as HTML
    //     for the rest of the process.
    //  3. `templates/` contains `tera` templates that are used to render pages from
    //     content or are used in `extends`/`includes` in the templates. The
    //     decision of which template renders which page is decided by look at the
    //     current path within `contents/` and using that relative path to find a
    //     start point inside `templates/`. From the starting point, look for any
    //     file that has an exact match for the filename. Otherwise look for a file
    //     with the name `page` and an extension matching the filename from
    //     `contents/`. If not found, then go up one directory level and try the
    //     `page` search again. If no match is found, then a template is not applied
    //     to the given file.
    //  4. Files in `static/` are copied directly to the output directory
    //  5. Files all folder are copied (after processing) to the output directory
    //     while maintaining their relative directory structure

    let site =
        Site::parse(build_files).context("failed to parse site structure from input files")?;

    debug!(?site, "Separated input files into distinct categories");

    // For each `content/` file, run the following process:
    //  1. Use the extension to apply a transformation:
    //       1. For `.dj` files, convert them into HTML
    //       2. For any other file type, leave them as is
    //  2. Find the corresponding template file using the lookup logic above, then
    //     wrap the content from step #1 in `PageContext` and use that to render the
    //     given template. If no template applies, skip this step.
    //  3. Take the output and write it into the `output_path` directory. The
    //     directory structure should be copied across from the input.

    // For each `static/` file, copy it directly to the `output_path` directory,
    // also maintaining directory structure.

    let tera = Templates::initialize_template_engine(&args)?;

    if !args.output_path.exists() {
        fs::create_dir_all(&args.output_path).context("failed to create output directory")?;
        debug!(
            output_path = %args.output_path.display(),
            "Created folder for site output"
        )
    }

    // Process content files
    for (slug, file) in site.content.files {
        let ctx = format!(
            "Failed to process file [{}] into output",
            file.input.full_path.display()
        );
        file.process(&args, &tera, &site.templates, slug)
            .context(ctx)?;
    }

    Site::format_output(&args)?;

    Ok(())
}
