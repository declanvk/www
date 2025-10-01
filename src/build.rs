use std::{
    collections::BTreeMap,
    fs::{self, DirEntry, Metadata},
    ops::Bound,
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde::Serialize;
use tracing::debug;

use crate::BuildCmd;

#[derive(Debug)]
struct BuildFile {
    path: PathBuf,
    metadata: Metadata,
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
            let metadata = entry
                .metadata()
                .context(format!("failed to read metadata of [{}]", path.display()))?;
            let page = BuildFile { path, metadata };

            let key = entry
                .path()
                .strip_prefix(content_root)
                .context(format!(
                    "Unable to strip prefix from page [{}]",
                    page.path.display()
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

#[derive(Debug)]
struct Content {
    files: BTreeMap<PathBuf, BuildFile>,
}

#[derive(Debug)]
struct Templates {
    files: BTreeMap<PathBuf, BuildFile>,
}

#[derive(Debug)]
struct StaticFiles {
    files: BTreeMap<PathBuf, BuildFile>,
}

#[derive(Debug)]
struct SiteInput {
    content: Content,
    templates: Templates,
    static_files: StaticFiles,
}

impl SiteInput {
    fn parse(build_files: BuildDirFiles) -> anyhow::Result<Self> {
        let mut content_files = BTreeMap::new();
        let mut templates_files = BTreeMap::new();
        let mut static_files = BTreeMap::new();

        for (path, file) in build_files.files {
            if let Some(first_component) = path.components().next() {
                if first_component.as_os_str() == "content" {
                    let sub_path = path.strip_prefix("content")?.to_path_buf();
                    content_files.insert(sub_path, file);
                } else if first_component.as_os_str() == "templates" {
                    let sub_path = path.strip_prefix("templates")?.to_path_buf();
                    templates_files.insert(sub_path, file);
                } else if first_component.as_os_str() == "static" {
                    let sub_path = path.strip_prefix("static")?.to_path_buf();
                    static_files.insert(sub_path, file);
                } else {
                    debug!(path = %path.display(), "Ignoring file not in a known directory");
                }
            }
        }

        Ok(SiteInput {
            content: Content {
                files: content_files,
            },
            templates: Templates {
                files: templates_files,
            },
            static_files: StaticFiles {
                files: static_files,
            },
        })
    }
}

#[derive(Debug, Serialize)]
struct PageContext {
    content: String,
}

#[tracing::instrument(skip_all, fields(args.path))]
pub fn build(args: BuildCmd) -> anyhow::Result<()> {
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
        SiteInput::parse(build_files).context("failed to parse site structure from input files")?;

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

    Ok(())
}
