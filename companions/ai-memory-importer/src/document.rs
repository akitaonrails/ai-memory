//! Bounded, dry-run-first extraction of one local document into one wiki page.
//!
//! The extractor accepts only an explicit file. It has no watch mode, never
//! follows a final-component symlink, never stores an absolute source path in
//! the page, and writes through the conditional public MCP tool.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use ai_memory_client::sanitize_external_text;
use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use quick_xml::Reader as XmlReader;
use quick_xml::events::Event;
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zip::ZipArchive;

use crate::ImportClient;
use crate::agentmemory::{validate_dest_prefix, validate_planned_path};
use crate::mcp::{McpWrite, Precondition, PreconditionFailed};

const IMPORT_VERSION: &str = "document-v1";
const DEFAULT_DEST_PREFIX: &str = "imported/documents";
const MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 4_096;
const MAX_ARCHIVE_ENTRY_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ARCHIVE_EXPANDED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: usize = 1024 * 1024;
const MAX_WORKING_TEXT_BYTES: usize = 2 * MAX_EXTRACTED_BYTES;
const MAX_PDF_PAGES: usize = 2_000;
const MAX_PDF_STREAM_BYTES: usize = 16 * 1024 * 1024;
const MAX_SHEETS: usize = 64;
const MAX_SPREADSHEET_CELLS: usize = 100_000;
const MAX_SHARED_STRINGS: usize = MAX_SPREADSHEET_CELLS;

/// Extract one supported local document into one attributed wiki page.
#[derive(Parser, Debug, Clone)]
pub(crate) struct DocumentArgs {
    /// Local PDF, DOCX, PPTX, XLSX, text, Markdown, VTT, or SRT file.
    #[arg(long)]
    file: PathBuf,
    /// Explicit destination workspace. Required with --apply.
    #[arg(long)]
    workspace: Option<String>,
    /// Explicit destination project. Required with --apply.
    #[arg(long)]
    project: Option<String>,
    /// Destination family for imported pages.
    #[arg(long, default_value = DEFAULT_DEST_PREFIX)]
    dest_prefix: String,
    /// Override the page title. The destination path still derives from the file.
    #[arg(long)]
    title: Option<String>,
    #[arg(
        long,
        env = "AI_MEMORY_SERVER_URL",
        default_value = crate::DEFAULT_SERVER_URL
    )]
    server_url: String,
    /// Perform the conditional MCP write. Without this flag, only print a plan.
    #[arg(long)]
    apply: bool,
    /// Durable JSON manifest. Required with --apply.
    #[arg(long)]
    manifest_out: Option<PathBuf>,
    /// Permit the write to create a missing explicit destination.
    #[arg(long)]
    create_destination: bool,
    /// Print the sanitized, bounded page body during a dry run.
    #[arg(long)]
    show_body: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum DocumentFormat {
    Text,
    Markdown,
    Pdf,
    Docx,
    Pptx,
    Xlsx,
    Vtt,
    Srt,
}

impl DocumentFormat {
    fn detect(path: &Path) -> Result<Self> {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match extension.as_str() {
            "txt" | "log" | "rst" | "csv" | "tsv" => Ok(Self::Text),
            "md" | "markdown" => Ok(Self::Markdown),
            "pdf" => Ok(Self::Pdf),
            "docx" => Ok(Self::Docx),
            "pptx" => Ok(Self::Pptx),
            "xlsx" => Ok(Self::Xlsx),
            "vtt" => Ok(Self::Vtt),
            "srt" => Ok(Self::Srt),
            _ => bail!(
                "unsupported document extension {extension:?}; supported: txt, log, rst, csv, tsv, md, markdown, pdf, docx, pptx, xlsx, vtt, srt"
            ),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Markdown => "markdown",
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Pptx => "pptx",
            Self::Xlsx => "xlsx",
            Self::Vtt => "vtt",
            Self::Srt => "srt",
        }
    }

    const fn needs_zip_preflight(self) -> bool {
        matches!(self, Self::Docx | Self::Pptx | Self::Xlsx)
    }
}

#[derive(Debug)]
struct DocumentPlan {
    source_path: String,
    source_filename: String,
    source_sha256: String,
    format: DocumentFormat,
    workspace: String,
    project: String,
    destination_path: String,
    title: String,
    body: String,
    import_key: String,
    extracted_bytes: usize,
    truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum ManifestStatus {
    Planned,
    Written,
    Present,
    Failed,
}

#[derive(Debug, Serialize)]
struct DocumentManifest {
    import_version: &'static str,
    source_path: String,
    source_filename: String,
    source_sha256: String,
    format: DocumentFormat,
    workspace: String,
    project: String,
    destination_path: String,
    import_key: String,
    extracted_bytes: usize,
    truncated: bool,
    status: ManifestStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl DocumentManifest {
    fn from_plan(plan: &DocumentPlan) -> Self {
        Self {
            import_version: IMPORT_VERSION,
            source_path: plan.source_path.clone(),
            source_filename: plan.source_filename.clone(),
            source_sha256: plan.source_sha256.clone(),
            format: plan.format,
            workspace: plan.workspace.clone(),
            project: plan.project.clone(),
            destination_path: plan.destination_path.clone(),
            import_key: plan.import_key.clone(),
            extracted_bytes: plan.extracted_bytes,
            truncated: plan.truncated,
            status: ManifestStatus::Planned,
            page_id: None,
            error: None,
        }
    }

    fn write(&self, path: &Path) -> Result<()> {
        crate::atomic_write(path, serde_json::to_string_pretty(self)?.as_bytes())
            .with_context(|| format!("write document manifest {}", path.display()))
    }

    fn fail(&mut self, path: &Path, error: &anyhow::Error) -> Result<()> {
        self.status = ManifestStatus::Failed;
        self.error = Some(crate::mcp::preview(&format!("{error:#}")));
        self.write(path)
    }
}

pub(crate) async fn run(args: DocumentArgs) -> Result<()> {
    if args.apply {
        if args
            .workspace
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
            || args
                .project
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
        {
            bail!("--apply requires explicit --workspace and --project");
        }
        if args.manifest_out.is_none() {
            bail!("--apply requires --manifest-out <path>");
        }
    }

    let workspace = args
        .workspace
        .clone()
        .unwrap_or_else(|| "DRY-RUN-WORKSPACE".into());
    let project = args
        .project
        .clone()
        .unwrap_or_else(|| "DRY-RUN-PROJECT".into());
    let plan = plan_document(
        &args.file,
        &workspace,
        &project,
        &args.dest_prefix,
        args.title.as_deref(),
    )?;
    let mut manifest = DocumentManifest::from_plan(&plan);

    if !args.apply {
        if args.show_body {
            println!("--- {} ---\n{}", plan.destination_path, plan.body);
        }
        if let Some(path) = &args.manifest_out {
            manifest.write(path)?;
        } else {
            println!(
                "dry-run: {} ({}) -> {}/{}:{}; {} extracted bytes{}; no HTTP writes performed",
                plan.source_filename,
                plan.format.as_str(),
                plan.workspace,
                plan.project,
                plan.destination_path,
                plan.extracted_bytes,
                if plan.truncated { ", truncated" } else { "" }
            );
        }
        return Ok(());
    }

    let manifest_path = args
        .manifest_out
        .as_ref()
        .ok_or_else(|| anyhow!("--apply requires --manifest-out <path>"))?;
    manifest.write(manifest_path)?;
    let client = ImportClient::new(&args.server_url)?;
    if let Err(error) = preflight(&client, &plan, args.create_destination).await {
        manifest.fail(manifest_path, &error)?;
        return Err(error);
    }

    let tags = vec![
        "document-import".to_owned(),
        format!("format-{}", plan.format.as_str()),
    ];
    let write = McpWrite {
        workspace: &plan.workspace,
        project: &plan.project,
        project_id: None,
        path: &plan.destination_path,
        body: &plan.body,
        title: Some(&plan.title),
        kind: "document",
        tier: "semantic",
        tags: &tags,
        expires_at: None,
    };
    match client.mcp_write_page(&write, Precondition::Absent).await {
        Ok(page_id) => {
            manifest.status = ManifestStatus::Written;
            manifest.page_id = Some(page_id);
            manifest.write(manifest_path)?;
        }
        Err(error) if error.downcast_ref::<PreconditionFailed>().is_some() => {
            let current = client
                .mcp_read_page(&plan.workspace, &plan.project, None, &plan.destination_path)
                .await
                .with_context(|| {
                    format!("reading {} after a refused write", plan.destination_path)
                })?;
            if let Some(error) = current.unverifiable(&plan.destination_path) {
                manifest.fail(manifest_path, &error)?;
                return Err(error);
            }
            if current.body != plan.body {
                let error = anyhow!(
                    "{} already holds a page this document import did not write; refusing to replace it",
                    plan.destination_path
                );
                manifest.fail(manifest_path, &error)?;
                return Err(error);
            }
            manifest.status = ManifestStatus::Present;
            manifest.page_id = current.page_id;
            manifest.write(manifest_path)?;
        }
        Err(error) => {
            manifest.fail(manifest_path, &error)?;
            return Err(error.context(format!("writing {}", plan.destination_path)));
        }
    }
    let outcome = match manifest.status {
        ManifestStatus::Written => "wrote",
        ManifestStatus::Present => "already had",
        status => bail!("document import ended in unexpected status {status:?}"),
    };
    println!(
        "import complete: {} {}/{}:{}",
        outcome, plan.workspace, plan.project, plan.destination_path
    );
    Ok(())
}

async fn preflight(
    client: &ImportClient,
    plan: &DocumentPlan,
    create_destination: bool,
) -> Result<()> {
    client.ensure_conditional_writes().await?;
    if !create_destination
        && !client
            .mcp_scope_exists(&plan.workspace, &plan.project)
            .await
    {
        bail!(
            "destination {}/{} must already exist (or pass --create-destination)",
            plan.workspace,
            plan.project
        );
    }
    Ok(())
}

fn plan_document(
    path: &Path,
    workspace: &str,
    project: &str,
    dest_prefix: &str,
    title: Option<&str>,
) -> Result<DocumentPlan> {
    let format = DocumentFormat::detect(path)?;
    let bytes = read_source(path)?;
    if format.needs_zip_preflight() {
        preflight_zip(&bytes)?;
    }
    let source_sha256 = hex_sha256(&bytes);
    let source_filename = source_filename(path)?;
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .map_or("document", |value| value);
    let title = one_line(&sanitize_external_text(title.unwrap_or(stem)), 200);
    let title = if title.is_empty() {
        "Imported document".to_owned()
    } else {
        title
    };
    let prefix = validate_dest_prefix(dest_prefix)?;
    let destination_path = format!(
        "{prefix}/{}-{source_sha256}.md",
        slugify(&one_line(stem, 64))
    );
    validate_planned_path(&destination_path, &prefix)?;

    let extracted = extract(format, path, &bytes)?;
    let sanitized = sanitize_external_text(&normalize_text(&extracted));
    if sanitized.trim().is_empty() {
        let hint = if format == DocumentFormat::Pdf {
            "; image-only PDFs need OCR before import"
        } else {
            ""
        };
        bail!("document contained no extractable text{hint}");
    }
    let extracted_bytes = sanitized.len();
    let truncated = extracted_bytes > MAX_EXTRACTED_BYTES;
    let extracted = if truncated {
        let suffix = format!(
            "\n\n[Document truncated by ai-memory-importer after {MAX_EXTRACTED_BYTES} bytes.]"
        );
        let budget = MAX_EXTRACTED_BYTES.saturating_sub(suffix.len());
        format!("{}{}", crate::truncate_utf8(&sanitized, budget), suffix)
    } else {
        sanitized
    };
    let import_key = document_key(format, &source_sha256);
    let body = render_page(
        &title,
        &source_filename,
        format,
        &source_sha256,
        &extracted,
        &import_key,
    );
    Ok(DocumentPlan {
        source_path: path.display().to_string(),
        source_filename,
        source_sha256,
        format,
        workspace: workspace.to_owned(),
        project: project.to_owned(),
        destination_path,
        title,
        body,
        import_key,
        extracted_bytes,
        truncated,
    })
}

#[cfg(unix)]
fn read_source(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect source document {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("source document must be a regular file, not a symlink");
    }
    if metadata.len() > MAX_SOURCE_BYTES {
        bail!(
            "source document is {} bytes; maximum is {MAX_SOURCE_BYTES}",
            metadata.len()
        );
    }
    let mut options = OpenOptions::new();
    options.read(true);
    use std::os::unix::fs::OpenOptionsExt as _;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut file = options
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file() {
        bail!("source document changed before it could be read");
    }
    use std::os::unix::fs::MetadataExt as _;
    if metadata.dev() != opened_metadata.dev() || metadata.ino() != opened_metadata.ino() {
        bail!("source document changed before it could be read");
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.by_ref()
        .take(MAX_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SOURCE_BYTES {
        bail!("source document grew past the {MAX_SOURCE_BYTES}-byte maximum while reading");
    }
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_source(_path: &Path) -> Result<Vec<u8>> {
    bail!(
        "document import is unavailable on this platform because stable no-follow source identity cannot be guaranteed"
    )
}

fn source_filename(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("source document path needs a file name"))?
        .to_string_lossy();
    let name = one_line(&sanitize_external_text(&name), 200);
    if name.is_empty() {
        bail!("source document file name is empty after sanitization");
    }
    Ok(name)
}

fn extract(format: DocumentFormat, _path: &Path, bytes: &[u8]) -> Result<String> {
    match format {
        DocumentFormat::Text | DocumentFormat::Markdown => {
            if bytes.len() > MAX_WORKING_TEXT_BYTES {
                bail!("text document exceeds the {MAX_WORKING_TEXT_BYTES}-byte extraction limit");
            }
            let text = String::from_utf8(bytes.to_vec())
                .context("text and Markdown documents must be UTF-8")?;
            // Editors that save UTF-8 with a BOM (Notepad, VS Code's `utf8bom`
            // encoding, Excel's "CSV UTF-8" export) put one here.
            // `extract_transcript` already strips it; without this it reaches
            // the page body, where `# Overview` stops being a heading. Neither
            // `normalize_text` nor `sanitize_external_text` removes it: U+FEFF
            // is neither whitespace nor a control character, so a trim misses it.
            Ok(text.trim_start_matches('\u{feff}').to_owned())
        }
        DocumentFormat::Pdf => extract_pdf(bytes),
        DocumentFormat::Docx => extract_docx(bytes),
        DocumentFormat::Pptx => extract_pptx(bytes),
        DocumentFormat::Xlsx => extract_xlsx(bytes),
        DocumentFormat::Vtt => extract_transcript(bytes, true),
        DocumentFormat::Srt => extract_transcript(bytes, false),
    }
}

fn extract_pdf(bytes: &[u8]) -> Result<String> {
    std::panic::catch_unwind(|| extract_pdf_inner(bytes))
        .map_err(|_| anyhow!("PDF text extractor panicked on the input; refusing the document"))?
}

fn extract_pdf_inner(bytes: &[u8]) -> Result<String> {
    extract_pdf_inner_with_limit(bytes, MAX_PDF_STREAM_BYTES)
}

fn extract_pdf_inner_with_limit(bytes: &[u8], max_stream_bytes: usize) -> Result<String> {
    let options = lopdf::LoadOptions::with_max_decompressed_size(max_stream_bytes);
    let document = lopdf::Document::load_mem_with_options(bytes, options).context("open PDF")?;
    let pages = document.get_pages();
    let page_count = pages.len();
    if page_count > MAX_PDF_PAGES {
        bail!("PDF has {page_count} pages; maximum is {MAX_PDF_PAGES}");
    }
    let mut output = String::new();
    for (index, page_number) in pages.keys().enumerate() {
        let page = document
            .extract_text_with_limit(&[*page_number], max_stream_bytes)
            .with_context(|| format!("extract PDF page {}", index + 1))?;
        push_bounded(&mut output, &format!("## Page {}\n\n", index + 1))?;
        push_bounded(&mut output, &page)?;
        push_bounded(&mut output, "\n\n")?;
    }
    Ok(output)
}

fn extract_docx(bytes: &[u8]) -> Result<String> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).context("open DOCX archive")?;
    let mut output = String::new();
    for (name, heading) in [
        ("word/document.xml", None),
        ("word/footnotes.xml", Some("## Footnotes\n\n")),
        ("word/endnotes.xml", Some("## Endnotes\n\n")),
    ] {
        let Some(xml) = read_zip_entry(&mut archive, name)? else {
            continue;
        };
        if let Some(heading) = heading {
            push_bounded(&mut output, heading)?;
        }
        push_bounded(&mut output, &xml_text(&xml)?)?;
        push_bounded(&mut output, "\n\n")?;
    }
    if output.is_empty() {
        bail!("DOCX archive has no word/document.xml text");
    }
    Ok(output)
}

fn extract_pptx(bytes: &[u8]) -> Result<String> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).context("open PPTX archive")?;
    let presentation = read_zip_entry(&mut archive, "ppt/presentation.xml")?
        .ok_or_else(|| anyhow!("PPTX archive has no presentation metadata"))?;
    let relationships = read_zip_entry(&mut archive, "ppt/_rels/presentation.xml.rels")?
        .ok_or_else(|| anyhow!("PPTX archive has no presentation relationships"))?;
    let slides = pptx_slide_order(&presentation, &relationships)?;
    let mut output = String::new();
    for (position, name) in slides.into_iter().enumerate() {
        let Some(xml) = read_zip_entry(&mut archive, &name)? else {
            bail!("PPTX slide relationship points to missing entry {name:?}");
        };
        push_bounded(&mut output, &format!("## Slide {}\n\n", position + 1))?;
        push_bounded(&mut output, &xml_text(&xml)?)?;
        push_bounded(&mut output, "\n\n")?;
        let number = numbered_part(&name, "ppt/slides/slide", ".xml")?;
        let relationships = format!("ppt/slides/_rels/slide{number}.xml.rels");
        if let Some(xml) = read_zip_entry(&mut archive, &relationships)?
            && let Some(notes) = pptx_notes_target(&xml)?
            && let Some(xml) = read_zip_entry(&mut archive, &notes)?
        {
            let text = xml_text(&xml)?;
            if !text.trim().is_empty() {
                push_bounded(&mut output, "### Speaker notes\n\n")?;
                push_bounded(&mut output, &text)?;
                push_bounded(&mut output, "\n\n")?;
            }
        }
    }
    if output.is_empty() {
        bail!("PPTX archive has no slide text");
    }
    Ok(output)
}

fn extract_xlsx(bytes: &[u8]) -> Result<String> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).context("open XLSX archive")?;
    let workbook = read_zip_entry(&mut archive, "xl/workbook.xml")?
        .ok_or_else(|| anyhow!("XLSX archive has no workbook metadata"))?;
    let relationships = read_zip_entry(&mut archive, "xl/_rels/workbook.xml.rels")?
        .ok_or_else(|| anyhow!("XLSX archive has no workbook relationships"))?;
    let sheets = xlsx_sheet_order(&workbook, &relationships)?;
    if sheets.len() > MAX_SHEETS {
        bail!(
            "XLSX workbook has {} sheets; maximum is {MAX_SHEETS}",
            sheets.len()
        );
    }
    let shared_strings = match read_zip_entry(&mut archive, "xl/sharedStrings.xml")? {
        Some(xml) => xlsx_shared_strings(&xml)?,
        None => Vec::new(),
    };
    let mut output = String::new();
    let mut cells = 0_usize;
    for (name, path) in sheets {
        let xml = read_zip_entry(&mut archive, &path)?.ok_or_else(|| {
            anyhow!("XLSX worksheet relationship points to missing entry {path:?}")
        })?;
        push_bounded(
            &mut output,
            &format!("## Sheet: {}\n\n", one_line(&name, 120)),
        )?;
        extract_xlsx_sheet(&xml, &shared_strings, &mut cells, &mut output)?;
        push_bounded(&mut output, "\n")?;
    }
    Ok(output)
}

fn pptx_slide_order(presentation: &str, relationships: &str) -> Result<Vec<String>> {
    let targets = office_relationships(relationships, "/slide", pptx_slide_target)?;
    let mut reader = XmlReader::from_str(presentation);
    let mut slides = Vec::new();
    loop {
        match reader
            .read_event()
            .context("parse PPTX presentation metadata")?
        {
            Event::Start(tag) | Event::Empty(tag) if tag.local_name().as_ref() == "sldId" => {
                let id = required_qualified_attribute(&tag, "r:id", "PPTX slide id")?;
                let target = targets
                    .get(&id)
                    .ok_or_else(|| anyhow!("PPTX slide id {id:?} has no slide relationship"))?;
                slides.push(target.clone());
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if slides.is_empty() {
        bail!("PPTX presentation contains no ordered slides");
    }
    Ok(slides)
}

fn xlsx_sheet_order(workbook: &str, relationships: &str) -> Result<Vec<(String, String)>> {
    let targets = office_relationships(relationships, "/worksheet", xlsx_sheet_target)?;
    let mut reader = XmlReader::from_str(workbook);
    let mut sheets = Vec::new();
    loop {
        match reader
            .read_event()
            .context("parse XLSX workbook metadata")?
        {
            Event::Start(tag) | Event::Empty(tag) if tag.local_name().as_ref() == "sheet" => {
                let name = required_attribute(&tag, "name", "XLSX sheet name")?;
                let id = required_qualified_attribute(&tag, "r:id", "XLSX sheet id")?;
                // `<sheets>` lists chartsheets and dialogsheets alongside worksheets;
                // only the relationship type tells them apart, so skip the ones that
                // carry no worksheet relationship instead of failing the whole import.
                let Some(target) = targets.get(&id) else {
                    continue;
                };
                sheets.push((name, target.clone()));
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if sheets.is_empty() {
        bail!("XLSX workbook contains no worksheets");
    }
    Ok(sheets)
}

fn office_relationships(
    xml: &str,
    type_suffix: &str,
    normalize_target: fn(&str) -> Result<String>,
) -> Result<HashMap<String, String>> {
    let mut reader = XmlReader::from_str(xml);
    let mut relationships = HashMap::new();
    loop {
        match reader.read_event().context("parse Office relationships")? {
            Event::Start(tag) | Event::Empty(tag)
                if tag.local_name().as_ref() == "Relationship" =>
            {
                let relation_type = required_attribute(&tag, "Type", "relationship type")?;
                if !relation_type.ends_with(type_suffix) {
                    continue;
                }
                let id = required_attribute(&tag, "Id", "relationship id")?;
                let target = required_attribute(&tag, "Target", "relationship target")?;
                if relationships
                    .insert(id.clone(), normalize_target(&target)?)
                    .is_some()
                {
                    bail!("Office relationships contain duplicate id {id:?}");
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(relationships)
}

fn pptx_slide_target(target: &str) -> Result<String> {
    let filename = target
        .strip_prefix("slides/")
        .or_else(|| target.strip_prefix("/ppt/slides/"))
        .ok_or_else(|| anyhow!("unsafe PPTX slide relationship target"))?;
    let number = numbered_part(filename, "slide", ".xml")?;
    Ok(format!("ppt/slides/slide{number}.xml"))
}

fn xlsx_sheet_target(target: &str) -> Result<String> {
    let filename = target
        .strip_prefix("worksheets/")
        .or_else(|| target.strip_prefix("/xl/worksheets/"))
        .ok_or_else(|| anyhow!("unsafe XLSX worksheet relationship target"))?;
    let number = numbered_part(filename, "sheet", ".xml")?;
    Ok(format!("xl/worksheets/sheet{number}.xml"))
}

fn numbered_part<'a>(value: &'a str, prefix: &str, suffix: &str) -> Result<&'a str> {
    value
        .strip_prefix(prefix)
        .and_then(|value| value.strip_suffix(suffix))
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| anyhow!("unsafe numbered Office part {value:?}"))
}

fn required_attribute(
    tag: &quick_xml::events::BytesStart<'_>,
    local_name: &str,
    description: &str,
) -> Result<String> {
    for attribute in tag.attributes() {
        let attribute = attribute.context("parse Office XML attribute")?;
        if attribute.key.local_name().as_ref() == local_name {
            return Ok(attribute
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                .into_owned());
        }
    }
    bail!("missing {description}")
}

fn required_qualified_attribute(
    tag: &quick_xml::events::BytesStart<'_>,
    name: &str,
    description: &str,
) -> Result<String> {
    for attribute in tag.attributes() {
        let attribute = attribute.context("parse Office XML attribute")?;
        if attribute.key.as_ref() == name {
            return Ok(attribute
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                .into_owned());
        }
    }
    bail!("missing {description}")
}

fn xlsx_shared_strings(xml: &str) -> Result<Vec<String>> {
    let mut reader = XmlReader::from_str(xml);
    let mut strings = Vec::new();
    let mut current = None::<String>;
    let mut in_text = false;
    loop {
        match reader.read_event().context("parse XLSX shared strings")? {
            Event::Start(tag) if tag.local_name().as_ref() == "si" => {
                if current.is_some() {
                    bail!("nested XLSX shared string entry");
                }
                current = Some(String::new());
            }
            Event::Start(tag) if tag.local_name().as_ref() == "t" => in_text = true,
            Event::Text(text) if in_text => {
                if let Some(current) = current.as_mut() {
                    push_bounded(current, &text.xml10_content())?;
                }
            }
            Event::CData(text) if in_text => {
                if let Some(current) = current.as_mut() {
                    push_bounded(current, &text.xml10_content())?;
                }
            }
            Event::GeneralRef(reference) if in_text => {
                if let Some(current) = current.as_mut() {
                    push_bounded(current, &xml_reference(&reference)?)?;
                }
            }
            Event::End(tag) if tag.local_name().as_ref() == "t" => in_text = false,
            Event::End(tag) if tag.local_name().as_ref() == "si" => {
                strings.push(one_line(&current.take().unwrap_or_default(), 4_096));
                if strings.len() > MAX_SHARED_STRINGS {
                    bail!("XLSX exceeds the {MAX_SHARED_STRINGS}-shared-string limit");
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if current.is_some() {
        bail!("unterminated XLSX shared string entry");
    }
    Ok(strings)
}

fn extract_xlsx_sheet(
    xml: &str,
    shared_strings: &[String],
    cells: &mut usize,
    output: &mut String,
) -> Result<()> {
    extract_xlsx_sheet_with_limit(xml, shared_strings, cells, output, MAX_WORKING_TEXT_BYTES)
}

fn extract_xlsx_sheet_with_limit(
    xml: &str,
    shared_strings: &[String],
    cells: &mut usize,
    output: &mut String,
    max_output_bytes: usize,
) -> Result<()> {
    let mut reader = XmlReader::from_str(xml);
    let mut row_cells = None::<usize>;
    let mut cell = None::<(Option<String>, String)>;
    let mut capture_value = false;
    loop {
        match reader.read_event().context("parse XLSX worksheet")? {
            Event::Start(tag) if tag.local_name().as_ref() == "row" => {
                if row_cells.is_some() {
                    bail!("nested XLSX row");
                }
                row_cells = Some(0);
            }
            Event::Start(tag) if tag.local_name().as_ref() == "c" => {
                if cell.is_some() {
                    bail!("nested XLSX cell");
                }
                *cells = cells
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("XLSX cell count overflow"))?;
                if *cells > MAX_SPREADSHEET_CELLS {
                    bail!("XLSX exceeds the {MAX_SPREADSHEET_CELLS}-cell extraction limit");
                }
                let mut kind = None;
                for attribute in tag.attributes() {
                    let attribute = attribute.context("parse XLSX cell attribute")?;
                    if attribute.key.local_name().as_ref() == "t" {
                        kind = Some(
                            attribute
                                .normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                                .into_owned(),
                        );
                    }
                }
                cell = Some((kind, String::new()));
            }
            Event::Empty(tag) if tag.local_name().as_ref() == "c" => {
                *cells = cells
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("XLSX cell count overflow"))?;
                if *cells > MAX_SPREADSHEET_CELLS {
                    bail!("XLSX exceeds the {MAX_SPREADSHEET_CELLS}-cell extraction limit");
                }
                let row_cells = row_cells
                    .as_mut()
                    .ok_or_else(|| anyhow!("XLSX cell appears outside a row"))?;
                push_bounded_with_limit(
                    output,
                    if *row_cells == 0 { "    " } else { "\t" },
                    max_output_bytes,
                )?;
                *row_cells += 1;
            }
            Event::Start(tag)
                if cell.is_some() && matches!(tag.local_name().as_ref(), "v" | "t") =>
            {
                capture_value = true;
            }
            Event::Text(text) if capture_value => {
                if let Some((_, value)) = cell.as_mut() {
                    push_bounded(value, &text.xml10_content())?;
                }
            }
            Event::CData(text) if capture_value => {
                if let Some((_, value)) = cell.as_mut() {
                    push_bounded(value, &text.xml10_content())?;
                }
            }
            Event::GeneralRef(reference) if capture_value => {
                if let Some((_, value)) = cell.as_mut() {
                    push_bounded(value, &xml_reference(&reference)?)?;
                }
            }
            Event::End(tag) if matches!(tag.local_name().as_ref(), "v" | "t") => {
                capture_value = false;
            }
            Event::End(tag) if tag.local_name().as_ref() == "c" => {
                let (kind, raw) = cell
                    .take()
                    .ok_or_else(|| anyhow!("XLSX cell ended without a start"))?;
                let value: Cow<'_, str> = if kind.as_deref() == Some("s") {
                    let index = raw
                        .parse::<usize>()
                        .context("XLSX shared-string cell has a non-numeric index")?;
                    Cow::Borrowed(
                        shared_strings
                            .get(index)
                            .ok_or_else(|| {
                                anyhow!("XLSX shared-string index {index} is out of range")
                            })?
                            .as_str(),
                    )
                } else {
                    Cow::Owned(one_line(&raw, 4_096))
                };
                let row_cells = row_cells
                    .as_mut()
                    .ok_or_else(|| anyhow!("XLSX cell appears outside a row"))?;
                push_bounded_with_limit(
                    output,
                    if *row_cells == 0 { "    " } else { "\t" },
                    max_output_bytes,
                )?;
                push_bounded_with_limit(output, &value, max_output_bytes)?;
                *row_cells += 1;
            }
            Event::End(tag) if tag.local_name().as_ref() == "row" => {
                let cells_in_row = row_cells
                    .take()
                    .ok_or_else(|| anyhow!("XLSX row ended without a start"))?;
                if cells_in_row == 0 {
                    push_bounded_with_limit(output, "    ", max_output_bytes)?;
                }
                push_bounded_with_limit(output, "\n", max_output_bytes)?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if row_cells.is_some() || cell.is_some() {
        bail!("unterminated XLSX row or cell");
    }
    Ok(())
}

fn pptx_notes_target(relationships: &str) -> Result<Option<String>> {
    let mut reader = XmlReader::from_str(relationships);
    loop {
        match reader
            .read_event()
            .context("parse PPTX slide relationships")?
        {
            Event::Start(tag) | Event::Empty(tag)
                if tag.local_name().as_ref() == "Relationship" =>
            {
                let mut relation_type = None;
                let mut target = None;
                for attribute in tag.attributes() {
                    let attribute = attribute.context("parse PPTX relationship attribute")?;
                    match attribute.key.local_name().as_ref() {
                        "Type" => {
                            relation_type = Some(
                                attribute
                                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                                    .into_owned(),
                            )
                        }
                        "Target" => {
                            target = Some(
                                attribute
                                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                                    .into_owned(),
                            )
                        }
                        _ => {}
                    }
                }
                if relation_type
                    .as_deref()
                    .is_some_and(|value| value.ends_with("/notesSlide"))
                {
                    let target =
                        target.ok_or_else(|| anyhow!("PPTX notes relationship has no target"))?;
                    let filename = target
                        .strip_prefix("../notesSlides/")
                        .or_else(|| target.strip_prefix("/ppt/notesSlides/"))
                        .ok_or_else(|| anyhow!("unsafe PPTX notes relationship target"))?;
                    let number = filename
                        .strip_prefix("notesSlide")
                        .and_then(|value| value.strip_suffix(".xml"))
                        .filter(|value| {
                            !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
                        })
                        .ok_or_else(|| anyhow!("unsafe PPTX notes relationship target"))?;
                    return Ok(Some(format!("ppt/notesSlides/notesSlide{number}.xml")));
                }
            }
            Event::Eof => return Ok(None),
            _ => {}
        }
    }
}

fn extract_transcript(bytes: &[u8], webvtt: bool) -> Result<String> {
    let text = std::str::from_utf8(bytes).context("transcript must be UTF-8")?;
    let mut output = String::new();
    let normalized = text
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    for (block_index, block) in normalized.split("\n\n").enumerate() {
        // Blocks can be separated by more than one blank line (WebVTT allows
        // any number: https://www.w3.org/TR/webvtt1/#file-structure), so
        // splitting on a single one can leave a blank line atop the next block.
        let lines = block
            .lines()
            .map(str::trim)
            .skip_while(|line| line.is_empty())
            .collect::<Vec<_>>();
        let Some(first) = lines.first().copied() else {
            continue;
        };
        if webvtt && block_index == 0 && first.starts_with("WEBVTT") {
            continue;
        }
        // A cue identifier may start with NOTE; only a complete NOTE token
        // introduces a comment: https://www.w3.org/TR/webvtt1/#webvtt-comment-block
        let comment = first == "NOTE" || first.starts_with("NOTE ") || first.starts_with("NOTE\t");
        if webvtt && (matches!(first, "STYLE" | "REGION") || comment) {
            continue;
        }
        let timing_index = if is_timing_line(first) {
            Some(0)
        } else if lines.get(1).is_some_and(|line| is_timing_line(line)) {
            Some(1)
        } else {
            None
        };
        let payload_start = timing_index.map_or(0, |index| index + 1);
        for line in &lines[payload_start..] {
            if line.is_empty() {
                continue;
            }
            push_bounded(&mut output, clean_transcript_markup(line, webvtt).trim())?;
            push_bounded(&mut output, "\n")?;
        }
        if !output.is_empty() && !output.ends_with("\n\n") {
            push_bounded(&mut output, "\n")?;
        }
    }
    Ok(output)
}

fn is_timing_line(line: &str) -> bool {
    let Some((start, end)) = line.split_once("-->") else {
        return false;
    };
    let looks_like_timestamp = |value: &str| {
        let value = value.split_whitespace().next().unwrap_or_default();
        value.contains(':')
            && value.bytes().any(|byte| byte.is_ascii_digit())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b':' | b'.' | b','))
    };
    looks_like_timestamp(start) && looks_like_timestamp(end)
}

fn clean_transcript_markup(line: &str, webvtt: bool) -> String {
    // WebVTT uses HTML character references in text (not attribute) context:
    // https://www.w3.org/TR/webvtt1/#webvtt-cue-text-parsing-rules
    // Decode original text segments once: escaped tags remain literal, and
    // removing markup cannot join fragments into a new character reference.
    let push_text = |output: &mut String, text: &str| {
        if webvtt {
            output.push_str(&htmlize::unescape(text));
        } else {
            output.push_str(text);
        }
    };
    let mut output = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find('<') {
        push_text(&mut output, &rest[..start]);
        let after_start = &rest[start + 1..];
        let Some(end) = after_start.find('>') else {
            push_text(&mut output, &rest[start..]);
            return output;
        };
        let candidate = &after_start[..end];
        if !is_transcript_tag(candidate) {
            output.push('<');
            rest = after_start;
            continue;
        }
        rest = &after_start[end + 1..];
    }
    push_text(&mut output, rest);
    output
}

fn is_transcript_tag(value: &str) -> bool {
    let value = value.trim_start_matches('/');
    let name = value
        .split(|ch: char| ch.is_whitespace() || ch == '.' || ch == ':')
        .next()
        .unwrap_or_default();
    matches!(name, "b" | "c" | "i" | "lang" | "ruby" | "rt" | "u" | "v")
        || (value.contains(':')
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b':' | b'.')))
}

fn preflight_zip(bytes: &[u8]) -> Result<()> {
    preflight_zip_with_limits(
        bytes,
        MAX_ARCHIVE_ENTRIES,
        MAX_ARCHIVE_ENTRY_BYTES,
        MAX_ARCHIVE_EXPANDED_BYTES,
    )
}

fn preflight_zip_with_limits(
    bytes: &[u8],
    max_entries: usize,
    max_entry_bytes: u64,
    max_expanded_bytes: u64,
) -> Result<()> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).context("open Office ZIP container")?;
    if archive.len() > max_entries {
        bail!(
            "Office archive has {} entries; maximum is {max_entries}",
            archive.len()
        );
    }
    let mut expanded = 0_u64;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        if entry.size() > max_entry_bytes {
            let name = entry.name().map_or_else(
                |_| "<invalid entry name>".to_owned(),
                |name| name.into_owned(),
            );
            bail!(
                "Office archive entry {name:?} expands to {} bytes; per-entry maximum is {max_entry_bytes}",
                entry.size(),
            );
        }
        expanded = expanded
            .checked_add(entry.size())
            .ok_or_else(|| anyhow!("Office archive expanded-size overflow"))?;
        if expanded > max_expanded_bytes {
            bail!("Office archive expands past the {max_expanded_bytes}-byte maximum");
        }
    }
    Ok(())
}

fn read_zip_entry<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<Option<String>> {
    let Ok(mut entry) = archive.by_name(name) else {
        return Ok(None);
    };
    if !entry.is_file() || entry.size() > MAX_ARCHIVE_ENTRY_BYTES {
        bail!("unsafe or oversized Office archive entry {name:?}");
    }
    let mut bytes = Vec::with_capacity(usize::try_from(entry.size()).unwrap_or(0));
    entry
        .by_ref()
        .take(MAX_ARCHIVE_ENTRY_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_ARCHIVE_ENTRY_BYTES {
        bail!("Office archive entry {name:?} exceeded its extraction limit");
    }
    Ok(Some(String::from_utf8(bytes).with_context(|| {
        format!("Office XML {name:?} is not UTF-8")
    })?))
}

fn xml_text(xml: &str) -> Result<String> {
    let mut reader = XmlReader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut output = String::new();
    let mut text_depth = 0_usize;
    let mut skipped_depth = 0_usize;
    loop {
        match reader.read_event().context("parse Office XML")? {
            Event::Start(_) if skipped_depth > 0 => skipped_depth += 1,
            // These tab lists define paragraph layout, not tab characters:
            // https://learn.microsoft.com/en-us/dotnet/api/documentformat.openxml.wordprocessing.tabs
            // https://learn.microsoft.com/en-us/dotnet/api/documentformat.openxml.drawing.tabstoplist
            Event::Start(tag)
                if matches!(
                    tag.local_name().as_ref(),
                    "del" | "delText" | "instrText" | "tabs" | "tabLst"
                ) =>
            {
                skipped_depth = 1;
            }
            Event::End(_) if skipped_depth > 0 => skipped_depth -= 1,
            Event::Start(tag) if tag.local_name().as_ref() == "t" => text_depth += 1,
            Event::End(tag) if tag.local_name().as_ref() == "t" => {
                text_depth = text_depth.saturating_sub(1);
            }
            Event::Text(text) if text_depth > 0 => {
                push_bounded(&mut output, &text.xml10_content())?
            }
            Event::CData(text) if text_depth > 0 => {
                push_bounded(&mut output, &text.xml10_content())?
            }
            Event::GeneralRef(reference) if text_depth > 0 => {
                push_bounded(&mut output, &xml_reference(&reference)?)?;
            }
            // DrawingML breaks can contain run properties; empty WordprocessingML
            // breaks and tabs may also use paired start/end tags. A `w:cr` is a
            // line break, like `w:br`, rather than a new paragraph.
            // https://learn.microsoft.com/en-us/dotnet/api/documentformat.openxml.drawing.break
            // https://learn.microsoft.com/en-us/dotnet/api/documentformat.openxml.wordprocessing.carriagereturn
            Event::Start(tag) | Event::Empty(tag) if skipped_depth == 0 => {
                match tag.local_name().as_ref() {
                    "br" | "cr" => push_bounded(&mut output, "\n")?,
                    "tab" => push_bounded(&mut output, "\t")?,
                    _ => {}
                }
            }
            Event::End(tag) if skipped_depth == 0 => match tag.local_name().as_ref() {
                "p" | "tr" => push_bounded(&mut output, "\n")?,
                "tc" => push_bounded(&mut output, "\t")?,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(normalize_text(&output))
}

fn xml_reference(reference: &quick_xml::events::BytesRef<'_>) -> Result<String> {
    Ok(match reference.as_ref() {
        "amp" => "&".to_owned(),
        "lt" => "<".to_owned(),
        "gt" => ">".to_owned(),
        "quot" => "\"".to_owned(),
        "apos" => "'".to_owned(),
        _ => reference
            .resolve_char_ref()?
            .map(|ch| ch.to_string())
            .ok_or_else(|| anyhow!("Office XML contains an unsupported entity"))?,
    })
}

fn push_bounded(output: &mut String, value: &str) -> Result<()> {
    push_bounded_with_limit(output, value, MAX_WORKING_TEXT_BYTES)
}

fn push_bounded_with_limit(output: &mut String, value: &str, limit: usize) -> Result<()> {
    if output.len().saturating_add(value.len()) > limit {
        bail!("extracted document text exceeds the {limit}-byte working limit");
    }
    output.push_str(value);
    Ok(())
}

fn normalize_text(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut output = String::with_capacity(normalized.len());
    let mut blank = 0;
    for line in normalized.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        output.push_str(line);
        output.push('\n');
    }
    output.trim().to_owned()
}

fn one_line(text: &str, max_chars: usize) -> String {
    one_line_chars(text.chars(), max_chars)
}

fn one_line_chars(chars: impl Iterator<Item = char>, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut output = String::new();
    let mut pending_space = false;
    let mut emitted = 0_usize;
    for ch in chars {
        if emitted == max_chars {
            break;
        }
        if ch.is_whitespace() {
            pending_space = !output.is_empty();
            continue;
        }
        if pending_space {
            output.push(' ');
            emitted += 1;
            pending_space = false;
            if emitted == max_chars {
                break;
            }
        }
        output.push(ch);
        emitted += 1;
        if emitted == max_chars {
            break;
        }
    }
    output
}

fn slugify(value: &str) -> String {
    let mut output = String::new();
    let mut dash = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            output.push(ch);
            dash = false;
        } else if !dash {
            output.push('-');
            dash = true;
        }
    }
    let output = output.trim_matches('-');
    if output.is_empty() {
        "document".to_owned()
    } else {
        output.to_owned()
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn key_namespace() -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        b"https://github.com/akitaonrails/ai-memory/companions/ai-memory-importer/document",
    )
}

fn document_key(format: DocumentFormat, sha256: &str) -> String {
    Uuid::new_v5(
        &key_namespace(),
        format!("{}:{sha256}", format.as_str()).as_bytes(),
    )
    .to_string()
}

fn provenance_marker(key: &str) -> String {
    format!("<!-- ai-memory-document-import:{key} -->")
}

fn render_page(
    title: &str,
    source_filename: &str,
    format: DocumentFormat,
    sha256: &str,
    extracted: &str,
    key: &str,
) -> String {
    format!(
        "# {title}\n\n> Imported from an untrusted local document by ai-memory-importer.\n\n- Source file: `{}`\n- Format: `{}`\n- SHA-256: `{sha256}`\n- Extractor: `{IMPORT_VERSION}`\n\n## Extracted content\n\n{extracted}\n\n{}",
        source_filename.replace('`', "'"),
        format.as_str(),
        provenance_marker(key)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentmemory::mock::{MockServer, StoredVersion};
    use lopdf::content::{Content, Operation};
    use lopdf::{Document, Object, Stream, dictionary};
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;
    use zip::write::SimpleFileOptions;

    fn write_zip(path: &Path, entries: &[(&str, &str)]) {
        let file = File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (name, body) in entries {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    fn write_pdf(path: &Path, text: &str) {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let font_id = document.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let resources_id = document.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![72.into(), 720.into()]),
                Operation::new("Tj", vec![Object::string_literal(text)]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = document.add_object(Stream::new(
            dictionary! {},
            content.encode().expect("encode PDF content"),
        ));
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        });
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);
        document.save(path).expect("write PDF fixture");
    }

    fn write_xlsx(path: &Path) {
        write_zip(
            path,
            &[
                (
                    "[Content_Types].xml",
                    r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
                ),
                (
                    "_rels/.rels",
                    r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
                ),
                (
                    "xl/workbook.xml",
                    r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Totals" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="XFD1048576"><v>42</v></c></row></sheetData></worksheet>"#,
                ),
                (
                    "xl/sharedStrings.xml",
                    r#"<?xml version="1.0" encoding="UTF-8"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>Item</t></si></sst>"#,
                ),
            ],
        );
    }

    fn apply_args(file: &Path, server_url: &str, manifest: &Path) -> DocumentArgs {
        DocumentArgs {
            file: file.to_path_buf(),
            workspace: Some("w".into()),
            project: Some("p".into()),
            dest_prefix: DEFAULT_DEST_PREFIX.into(),
            title: None,
            server_url: server_url.into(),
            apply: true,
            manifest_out: Some(manifest.to_path_buf()),
            create_destination: false,
            show_body: false,
        }
    }

    #[test]
    fn text_plan_is_redacted_bounded_attributed_and_deterministic() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("Release Notes.md");
        fs::write(
            &file,
            format!(
                "# Release\n\ntoken=abcdefghijklmnop\n{}",
                "x".repeat(MAX_EXTRACTED_BYTES)
            ),
        )
        .unwrap();
        let first = plan_document(&file, "w", "p", DEFAULT_DEST_PREFIX, None).unwrap();
        let second = plan_document(&file, "w", "p", DEFAULT_DEST_PREFIX, None).unwrap();
        assert_eq!(first.destination_path, second.destination_path);
        assert_eq!(first.import_key, second.import_key);
        assert!(
            first
                .destination_path
                .starts_with("imported/documents/release-notes-")
        );
        assert!(first.body.contains("token=[REDACTED]"));
        assert!(!first.body.contains("abcdefghijklmnop"));
        assert!(first.body.contains("Source file: `Release Notes.md`"));
        assert!(first.body.contains("SHA-256:"));
        assert!(
            first
                .body
                .trim_end()
                .ends_with(&provenance_marker(&first.import_key))
        );
        assert!(first.truncated);
        assert!(first.body.contains("Document truncated"));
        let titled = plan_document(
            &file,
            "w",
            "p",
            DEFAULT_DEST_PREFIX,
            Some("token=abcdefghijklmnop"),
        )
        .unwrap();
        assert_eq!(titled.title, "token=[REDACTED]");
    }

    #[test]
    fn text_plan_drops_a_leading_utf8_bom() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("notes.md");
        fs::write(&file, "\u{feff}# Overview\n\nBody line.\n").unwrap();
        let plan = plan_document(&file, "w", "p", DEFAULT_DEST_PREFIX, None).unwrap();
        assert!(
            !plan.body.contains('\u{feff}'),
            "BOM reached the page body: {:?}",
            plan.body
        );
        assert!(
            plan.body.contains("## Extracted content\n\n# Overview\n"),
            "{:?}",
            plan.body
        );
    }

    #[test]
    fn docx_extracts_document_footnote_text_and_decodes_entities() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("brief.docx");
        write_zip(
            &file,
            &[
                (
                    "word/document.xml",
                    r#"<w:document xmlns:w="w"><w:body><w:p><w:r><w:t>Hello &amp; world</w:t></w:r><w:del><w:r><w:delText>deleted secret</w:delText></w:r></w:del><w:instrText>field instruction</w:instrText></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#,
                ),
                (
                    "word/footnotes.xml",
                    r#"<w:footnotes xmlns:w="w"><w:footnote><w:p><w:r><w:t>Foot</w:t></w:r></w:p></w:footnote></w:footnotes>"#,
                ),
            ],
        );
        let plan = plan_document(&file, "w", "p", DEFAULT_DEST_PREFIX, None).unwrap();
        assert!(plan.body.contains("Hello & world"), "{}", plan.body);
        assert!(plan.body.contains("A\n\tB"), "{}", plan.body);
        assert!(plan.body.contains("## Footnotes"));
        assert!(plan.body.contains("Foot"));
        assert!(!plan.body.contains("deleted secret"));
        assert!(!plan.body.contains("field instruction"));
    }

    #[test]
    fn docx_run_separators_are_preserved_and_sanitized() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("separators.docx");
        for line_break in ["<w:br/>", "<w:br></w:br>", "<w:cr/>", "<w:cr></w:cr>"] {
            for tab in ["<w:tab/>", "<w:tab></w:tab>"] {
                let document = format!(
                    r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>First line</w:t>{line_break}<w:t>Host</w:t>{tab}<w:t>8443 token=abcdefghijklmnop</w:t></w:r></w:p><w:p><w:pPr><w:tabs><w:tab w:val="left" w:pos="720"/><w:tab w:val="left" w:pos="1440"></w:tab></w:tabs></w:pPr><w:r><w:t>Kept</w:t></w:r><w:del><w:r><w:delText>DELETED_SENTINEL</w:delText>{line_break}{tab}</w:r></w:del><w:r><w:instrText>FIELD_SENTINEL</w:instrText><w:t> together</w:t></w:r></w:p></w:body></w:document>"#
                );
                write_zip(&file, &[("word/document.xml", &document)]);
                let plan = plan_document(&file, "w", "p", DEFAULT_DEST_PREFIX, None).unwrap();
                assert!(
                    plan.body
                        .contains("First line\nHost\t8443 token=[REDACTED]\nKept together"),
                    "break {line_break}, tab {tab}: {}",
                    plan.body
                );
                assert!(!plan.body.contains("abcdefghijklmnop"));
                assert!(!plan.body.contains("DELETED_SENTINEL"));
                assert!(!plan.body.contains("FIELD_SENTINEL"));
            }
        }
    }

    #[test]
    fn pdf_extracts_text_by_page() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("brief.pdf");
        write_pdf(&file, "Quarterly result");
        let plan = plan_document(&file, "w", "p", DEFAULT_DEST_PREFIX, None).unwrap();
        assert!(plan.body.contains("## Page 1"), "{}", plan.body);
        assert!(plan.body.contains("Quarterly result"), "{}", plan.body);
    }

    #[test]
    fn pdf_refuses_content_stream_expansion_past_limit() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("large-stream.pdf");
        write_pdf(&file, &"x".repeat(4_096));
        let bytes = fs::read(file).unwrap();
        assert!(extract_pdf_inner_with_limit(&bytes, 64).is_err());
    }

    #[test]
    fn xlsx_extracts_sheet_names_and_cells() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("totals.xlsx");
        write_xlsx(&file);
        let plan = plan_document(&file, "w", "p", DEFAULT_DEST_PREFIX, None).unwrap();
        assert!(plan.body.contains("## Sheet: Totals"), "{}", plan.body);
        assert!(plan.body.contains("Item\t42"), "{}", plan.body);
    }

    #[test]
    fn xlsx_sheet_order_skips_a_chartsheet_and_keeps_the_worksheets() {
        let workbook = r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/><sheet name="Chart1" sheetId="2" r:id="rId2"/></sheets></workbook>"#;
        let relationships = r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet" Target="chartsheets/sheet1.xml"/></Relationships>"#;
        let sheets = xlsx_sheet_order(workbook, relationships).unwrap();
        assert_eq!(
            sheets,
            vec![("Data".to_owned(), "xl/worksheets/sheet1.xml".to_owned())]
        );
    }

    #[test]
    fn xlsx_sheet_order_still_reports_a_workbook_of_only_chartsheets() {
        let workbook = r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Chart1" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
        let relationships = r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet" Target="chartsheets/sheet1.xml"/></Relationships>"#;
        let error = xlsx_sheet_order(workbook, relationships).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("XLSX workbook contains no worksheets"),
            "{error}"
        );
    }

    #[test]
    fn xlsx_row_hits_output_limit_before_parsing_later_cells() {
        let xml = r#"<worksheet><sheetData><row><c t="s"><v>0</v></c><c/><c><v>&invalid;</v></c></row></sheetData></worksheet>"#;
        let mut output = String::new();
        let mut cells = 0;
        let error =
            extract_xlsx_sheet_with_limit(xml, &["A".to_owned()], &mut cells, &mut output, 5)
                .unwrap_err();
        assert!(error.to_string().contains("5-byte working limit"));
        assert_eq!(output, "    A");
        assert_eq!(cells, 2);
    }

    #[test]
    fn xlsx_normalizes_a_long_shared_string_once_before_repeated_references() {
        let shared_xml = format!("<sst><si><t>{}</t></si></sst>", "x".repeat(1_500_000));
        let shared = xlsx_shared_strings(&shared_xml).unwrap();
        assert_eq!(shared[0].len(), 4_096);

        let cells_xml = (0..100)
            .map(|_| r#"<c t="s"><v>0</v></c>"#)
            .collect::<String>();
        let sheet = format!("<worksheet><sheetData><row>{cells_xml}</row></sheetData></worksheet>");
        let mut output = String::new();
        let mut cells = 0;
        let error = extract_xlsx_sheet_with_limit(&sheet, &shared, &mut cells, &mut output, 9_000)
            .unwrap_err();
        assert!(error.to_string().contains("9000-byte working limit"));
        assert_eq!(cells, 3);
        assert_eq!(output.len(), 8_198);
    }

    #[test]
    fn one_line_stops_consuming_when_its_character_budget_is_full() {
        let consumed = std::cell::Cell::new(0_usize);
        let chars = std::iter::from_fn(|| {
            let next = consumed.get() + 1;
            consumed.set(next);
            assert!(next <= 4_096, "one_line consumed past its output budget");
            Some('x')
        });
        let output = one_line_chars(chars, 4_096);
        assert_eq!(output.len(), 4_096);
        assert_eq!(consumed.get(), 4_096);
    }

    #[test]
    fn pptx_uses_presentation_order_and_includes_speaker_notes() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("deck.pptx");
        write_zip(
            &file,
            &[
                (
                    "ppt/presentation.xml",
                    r#"<p:presentation xmlns:p="p" xmlns:r="r"><p:sldIdLst><p:sldId id="10" r:id="rId10"/><p:sldId id="2" r:id="rId2"/></p:sldIdLst></p:presentation>"#,
                ),
                (
                    "ppt/_rels/presentation.xml.rels",
                    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide2.xml"/><Relationship Id="rId10" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide10.xml"/></Relationships>"#,
                ),
                (
                    "ppt/slides/slide10.xml",
                    "<p:sld xmlns:p=\"p\"><p><t>Ten</t></p></p:sld>",
                ),
                (
                    "ppt/slides/slide2.xml",
                    "<p:sld xmlns:p=\"p\"><p><t>Two</t></p></p:sld>",
                ),
                (
                    "ppt/slides/_rels/slide2.xml.rels",
                    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide7.xml"/></Relationships>"#,
                ),
                (
                    "ppt/notesSlides/notesSlide7.xml",
                    "<p:notes xmlns:p=\"p\"><p><t>Say this</t></p></p:notes>",
                ),
            ],
        );
        let plan = plan_document(&file, "w", "p", DEFAULT_DEST_PREFIX, None).unwrap();
        let ten = plan.body.find("Ten").unwrap();
        let two = plan.body.find("Two").unwrap();
        assert!(ten < two);
        assert!(plan.body.contains("## Slide 1"));
        assert!(plan.body.contains("## Slide 2"));
        assert!(plan.body.contains("### Speaker notes"));
        assert!(plan.body.contains("Say this"));
    }

    #[test]
    fn pptx_run_separators_are_preserved_and_sanitized() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("separators.pptx");
        let tab_stops = r#"<a:tab pos="914400" algn="l"/><a:tab pos="1828800" algn="l"></a:tab>"#;
        let list_style = format!(
            "<a:lstStyle><a:defPPr><a:tabLst>{tab_stops}</a:tabLst></a:defPPr><a:lvl1pPr><a:tabLst>{tab_stops}</a:tabLst></a:lvl1pPr></a:lstStyle>"
        );
        // The first shape makes any injected tab in the next shape's paragraph
        // properties visible instead of letting normalize_text trim it away.
        let first_shape = "<p:sp><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>Heading</a:t></a:r></a:p></p:txBody></p:sp>";
        for line_break in [
            "<a:br/>",
            "<a:br></a:br>",
            r#"<a:br><a:rPr lang="en-US"/></a:br>"#,
        ] {
            let slide = format!(
                r#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree>{first_shape}<p:sp><p:txBody><a:bodyPr/>{list_style}<a:p><a:pPr><a:tabLst>{tab_stops}</a:tabLst></a:pPr><a:r><a:t>Use</a:t></a:r>{line_break}<a:r><a:t>TLS token=abcdefghijklmnop</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
            );
            let notes = format!(
                r#"<p:notes xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree>{first_shape}<p:sp><p:txBody><a:bodyPr/>{list_style}<a:p><a:pPr><a:tabLst>{tab_stops}</a:tabLst></a:pPr><a:r><a:t>Say</a:t></a:r>{line_break}<a:r><a:t>this token=abcdefghijklmnop</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:notes>"#
            );
            write_zip(
                &file,
                &[
                    (
                        "ppt/presentation.xml",
                        r#"<p:presentation xmlns:p="p" xmlns:r="r"><p:sldIdLst><p:sldId id="1" r:id="rId1"/></p:sldIdLst></p:presentation>"#,
                    ),
                    (
                        "ppt/_rels/presentation.xml.rels",
                        r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></Relationships>"#,
                    ),
                    ("ppt/slides/slide1.xml", &slide),
                    (
                        "ppt/slides/_rels/slide1.xml.rels",
                        r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide1.xml"/></Relationships>"#,
                    ),
                    ("ppt/notesSlides/notesSlide1.xml", &notes),
                ],
            );
            let plan = plan_document(&file, "w", "p", DEFAULT_DEST_PREFIX, None).unwrap();
            assert!(
                plan.body
                    .contains("## Slide 1\n\nHeading\nUse\nTLS token=[REDACTED]"),
                "break {line_break}: {}",
                plan.body
            );
            assert!(
                plan.body
                    .contains("### Speaker notes\n\nHeading\nSay\nthis token=[REDACTED]"),
                "break {line_break}: {}",
                plan.body
            );
            assert!(!plan.body.contains("abcdefghijklmnop"));
        }
    }

    #[test]
    fn transcripts_drop_timing_metadata_and_markup() {
        let vtt = b"WEBVTT\n\nNOTE private cue note\nskip me\n\nintro-id\n00:00:01.000 --> 00:00:02.000\n<v Alice>Hello</v> world\n\n00:00:03.000 --> 00:00:04.000\n1 < 2 and literal --> arrow\n";
        let text = extract_transcript(vtt, true).unwrap();
        assert_eq!(text.trim(), "Hello world\n\n1 < 2 and literal --> arrow");
        assert!(!text.contains("intro-id"));
        let srt = b"1\n00:00:01,000 --> 00:00:02,000\n12345\n\n2\n00:00:03,000 --> 00:00:04,000\nSecond line\n";
        let text = extract_transcript(srt, false).unwrap();
        assert_eq!(text.trim(), "12345\n\nSecond line");
    }

    #[test]
    fn webvtt_decodes_character_references_in_cue_text() {
        for (payload, expected) in [
            (
                "AT&amp;T: 1 &lt; 2 &gt; 0, a&nbsp;b &lrm;left &rlm;right",
                "AT&T: 1 < 2 > 0, a\u{a0}b \u{200e}left \u{200f}right",
            ),
            (
                "&#39;caf&#233;&#39; &#x1F642; &trade; &NotEqualTilde;",
                "'café' 🙂 ™ ≂\u{338}",
            ),
            ("&notit; &copy 2026 &#65 &#x41 &#X41", "¬it; © 2026 A A A"),
            (
                "&#0; &#xD800; &#1114112; &#128; &#999999999999999999999;",
                "� � � € �",
            ),
            (
                "&unknown; &#; &#x; bare & ampersand &amp;",
                "&unknown; &#; &#x; bare & ampersand &",
            ),
        ] {
            let input = format!("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n{payload}\n");
            assert_eq!(
                extract_transcript(input.as_bytes(), true).unwrap().trim(),
                expected,
                "cue payload {payload:?}"
            );
        }
    }

    #[test]
    fn webvtt_decodes_once_without_reparsing_or_joining_markup() {
        for (payload, expected) in [
            (
                "<v Alice><b>AT&amp;T</b> &lt;b&gt;literal&lt;/b&gt;</v>",
                "AT&T <b>literal</b>",
            ),
            ("&amp;lt; &amp;amp;", "&lt; &amp;"),
            ("&am<b></b>p; &#x<i></i>41;", "&amp; &#x41;"),
            ("<unknown>&amp;</unknown>", "<unknown>&</unknown>"),
            ("1 < 2 &amp;", "1 < 2 &"),
            (
                "<v Alice><b>First &amp;\nSecond &lt;third&gt;</b></v>",
                "First &\nSecond <third>",
            ),
            ("&am\np;", "&am\np;"),
        ] {
            let input = format!("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n{payload}\n");
            assert_eq!(
                extract_transcript(input.as_bytes(), true).unwrap().trim(),
                expected,
                "cue payload {payload:?}"
            );
        }
    }

    #[test]
    fn srt_keeps_character_references_literal() {
        let input =
            b"1\n00:00:01,000 --> 00:00:02,000\n<b>AT&amp;T</b> &lt;b&gt;literal&lt;/b&gt; &#65;\n";
        assert_eq!(
            extract_transcript(input, false).unwrap().trim(),
            "AT&amp;T &lt;b&gt;literal&lt;/b&gt; &#65;"
        );
    }

    #[test]
    fn webvtt_decoded_text_is_sanitized_before_the_document_plan() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("captions.vtt");
        fs::write(
            &file,
            "WEBVTT\n\nNOTE annotation\nCOMMENT_SENTINEL\n\n00:00:01.000 --> 00:00:02.000\nKnown fact AT&amp;T token&#61;abcdefghijklmnop\nLiteral &lt;b&gt;sample&lt;/b&gt; &amp;lt;\n",
        )
        .unwrap();
        let plan = plan_document(&file, "w", "p", DEFAULT_DEST_PREFIX, None).unwrap();
        assert!(plan.body.contains("Known fact AT&T"));
        assert!(plan.body.contains("token=[REDACTED]"));
        assert!(plan.body.contains("Literal <b>sample</b> &lt;"));
        assert!(!plan.body.contains("abcdefghijklmnop"));
        assert!(!plan.body.contains("COMMENT_SENTINEL"));
    }

    #[test]
    fn webvtt_entity_expansion_respects_the_working_text_limit() {
        // &nGt; has five input bytes and expands to six UTF-8 bytes.
        let payload = "&nGt;".repeat(MAX_WORKING_TEXT_BYTES / 6 + 1);
        assert!(payload.len() < MAX_WORKING_TEXT_BYTES);
        let input = format!("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n{payload}\n");
        assert!(extract_transcript(input.as_bytes(), true).is_err());
    }

    #[test]
    fn webvtt_keeps_cues_whose_identifiers_only_start_with_note() {
        let vtt = b"WEBVTT\n\nNOTE an actual comment\nNot spoken\n\nNOTEworthy\n00:00:01.000 --> 00:00:02.000\nKeep this fact\n\nNOTE123\n00:00:03.000 --> 00:00:04.000\nKeep this too\n\nNOTE\tanother comment\nNot spoken either\n\nNOTE\nNot spoken at all\n";
        assert_eq!(
            extract_transcript(vtt, true).unwrap().trim(),
            "Keep this fact\n\nKeep this too"
        );
    }

    #[test]
    fn transcripts_accept_each_supported_line_terminator() {
        let vtt = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nFirst fact\n\n00:00:03.000 --> 00:00:04.000\nSecond fact\n";
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nFirst fact\n\n2\n00:00:03,000 --> 00:00:04,000\nSecond fact\n";
        for newline in ["\n", "\r\n", "\r"] {
            for (input, webvtt) in [(vtt, true), (srt, false)] {
                let input = input.replace('\n', newline);
                assert_eq!(
                    extract_transcript(input.as_bytes(), webvtt).unwrap().trim(),
                    "First fact\n\nSecond fact",
                    "line terminator {newline:?}, WebVTT={webvtt}"
                );
            }
        }
    }

    #[test]
    fn transcripts_keep_cues_after_more_than_one_blank_line() {
        let vtt = "WEBVTT\n\n\n00:00:01.000 --> 00:00:02.000\nFirst fact\n\n\n00:00:03.000 --> 00:00:04.000\nSecond fact\n";
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nFirst fact\n\n\n2\n00:00:03,000 --> 00:00:04,000\nSecond fact\n";
        for newline in ["\n", "\r\n", "\r"] {
            for (input, webvtt) in [(vtt, true), (srt, false)] {
                let input = input.replace('\n', newline);
                assert_eq!(
                    extract_transcript(input.as_bytes(), webvtt).unwrap().trim(),
                    "First fact\n\nSecond fact",
                    "line terminator {newline:?}, WebVTT={webvtt}"
                );
            }
        }
    }

    #[test]
    fn subtitle_cues_are_sanitized_before_the_document_plan() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("captions.vtt");
        for identifier in ["NOTE123", "ordinary-cue"] {
            for newline in ["\n", "\r\n", "\r"] {
                let input = format!(
                    "WEBVTT\n\nNOTE annotation\nCOMMENT_SENTINEL\n\n{identifier}\n00:00:01.000 --> 00:00:02.000\nKeep this fact token=abcdefghijklmnop\n"
                )
                .replace('\n', newline);
                fs::write(&file, input).unwrap();
                let plan = plan_document(&file, "w", "p", DEFAULT_DEST_PREFIX, None).unwrap();
                assert!(plan.body.contains("Keep this fact token=[REDACTED]"));
                assert!(!plan.body.contains("abcdefghijklmnop"));
                assert!(!plan.body.contains("COMMENT_SENTINEL"));
            }
        }
    }

    #[test]
    fn unsafe_sources_prefixes_and_malformed_archives_fail_closed() {
        let temp = TempDir::new().unwrap();
        let bad = temp.path().join("bad.docx");
        fs::write(&bad, b"not a zip").unwrap();
        assert!(plan_document(&bad, "w", "p", DEFAULT_DEST_PREFIX, None).is_err());
        let text = temp.path().join("a.txt");
        fs::write(&text, "hello").unwrap();
        assert!(plan_document(&text, "w", "p", "sessions", None).is_err());
        assert!(DocumentFormat::detect(Path::new("malware.exe")).is_err());

        let oversized = temp.path().join("oversized.txt");
        File::create(&oversized)
            .unwrap()
            .set_len(MAX_SOURCE_BYTES + 1)
            .unwrap();
        assert!(plan_document(&oversized, "w", "p", DEFAULT_DEST_PREFIX, None).is_err());

        assert!(
            extract(
                DocumentFormat::Text,
                Path::new("large.txt"),
                &vec![b'x'; MAX_WORKING_TEXT_BYTES + 1]
            )
            .is_err()
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&text, temp.path().join("link.txt")).unwrap();
            assert!(
                plan_document(
                    &temp.path().join("link.txt"),
                    "w",
                    "p",
                    DEFAULT_DEST_PREFIX,
                    None
                )
                .is_err()
            );
        }
    }

    #[test]
    fn zip_preflight_refuses_excessive_claimed_expansion() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("large.docx");
        write_zip(&file, &[("word/document.xml", &"x".repeat(65))]);
        let bytes = fs::read(file).unwrap();
        assert!(preflight_zip_with_limits(&bytes, 10, 64, 128).is_err());
    }

    #[tokio::test]
    async fn apply_is_conditional_idempotent_and_refuses_foreign_pages() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("notes.md");
        fs::write(&file, "# Notes\n\nOne durable fact.").unwrap();
        let server = MockServer::start(&[("w", "p")]);
        let manifest = temp.path().join("manifest.json");

        run(apply_args(&file, &server.url, &manifest))
            .await
            .unwrap();
        assert_eq!(server.state().stored_versions(), 1);
        assert!(
            server
                .state()
                .pages
                .values()
                .flatten()
                .all(|version| version.arguments["create_only"] == true)
        );
        run(apply_args(&file, &server.url, &manifest))
            .await
            .unwrap();
        assert_eq!(server.state().stored_versions(), 1);
        let recorded: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        assert_eq!(recorded["status"], "present");

        let other = temp.path().join("foreign.md");
        fs::write(&other, "A different document.").unwrap();
        let planned = plan_document(&other, "w", "p", DEFAULT_DEST_PREFIX, None).unwrap();
        server.state().pages.insert(
            ("w".into(), "p".into(), planned.destination_path.clone()),
            vec![StoredVersion {
                page_id: "someone-else".into(),
                body: format!(
                    "# Hand written\n\n{}",
                    provenance_marker(&planned.import_key)
                ),
                arguments: serde_json::Value::Null,
            }],
        );
        let error = run(apply_args(&other, &server.url, &manifest))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("did not write"), "{error:#}");
        assert_eq!(
            server
                .state()
                .chain("w", "p", &planned.destination_path)
                .len(),
            1
        );
        let recorded: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        assert_eq!(recorded["status"], "failed");
    }

    /// The server returns 12,000 characters of a body unless asked for more,
    /// and never more than 64,000 (#1827). A rerun asks for the ceiling, so a
    /// long document it wrote is still found present; a page past the ceiling
    /// cannot be compared and fails with that reason, not as a foreign page.
    #[tokio::test]
    async fn a_rerun_compares_long_documents_whole_or_says_why_it_cannot() {
        let temp = TempDir::new().unwrap();
        let server = MockServer::start(&[("w", "p")]);
        let manifest = temp.path().join("manifest.json");
        let recorded = || -> serde_json::Value {
            serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap()
        };
        let line = "The importer keeps every rerun idempotent.\n";

        let long = temp.path().join("long.md");
        fs::write(&long, line.repeat(500)).unwrap();
        run(apply_args(&long, &server.url, &manifest))
            .await
            .unwrap();
        run(apply_args(&long, &server.url, &manifest))
            .await
            .unwrap();
        assert_eq!(recorded()["status"], "present");
        assert_eq!(server.state().stored_versions(), 1);

        let longer = temp.path().join("longer.md");
        fs::write(&longer, line.repeat(1600)).unwrap();
        run(apply_args(&longer, &server.url, &manifest))
            .await
            .unwrap();
        let error = run(apply_args(&longer, &server.url, &manifest))
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("longer than memory_read_page returns"),
            "{message}"
        );
        assert!(!message.contains("did not write"), "{message}");
        assert_eq!(recorded()["status"], "failed");
        assert_eq!(server.state().stored_versions(), 2);
    }
}
