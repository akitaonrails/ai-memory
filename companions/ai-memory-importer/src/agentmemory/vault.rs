//! `agentmemory-vault <dir>`: import the Obsidian vault agentmemory's
//! `obsidian-export` writes (`memories/`, `lessons/`, `crystals/`,
//! `sessions/`, each page with YAML frontmatter and `[[id]]` wikilinks).
//!
//! Pages land under `<dest-prefix>/{memories,lessons,crystals,sessions}/`,
//! never under ai-memory's own `sessions/`. A wikilink to another imported
//! page is rewritten to its new path; a link to anything else becomes plain
//! text. `MOC.md`, the vault's generated index, is not imported.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use walkdir::WalkDir;

use super::{
    DEFAULT_DEST_PREFIX, Destination, MAX_EXPLICIT_TITLE_CHARS, Manifest, PlannedPage,
    PlannedVersion, apply_pages, ingest_key, link_target, memory_kind_tier, one_line, preflight,
    print_done, print_plan, provenance_line, rewrite_wikilinks, tags_from, validate_dest_prefix,
    validate_id, validate_planned_path,
};
use crate::{
    DEFAULT_SERVER_URL, ImportClient, YamlMap, first_h1_line, sha256_hex, sha256_parts, slugify,
    split_frontmatter, validate_label, validate_source_rel, yaml_string,
};

const IMPORT_VERSION: &str = "agentmemory-vault-v1";
/// Bound on one vault page.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Bound on the number of pages one vault import plans.
const MAX_FILES: usize = 100_000;

/// The four folders the export writes, with the record name each holds.
const FOLDERS: &[(&str, &str)] = &[
    ("memories", "memory"),
    ("lessons", "lesson"),
    ("crystals", "crystal"),
    ("sessions", "session"),
];

#[derive(Parser, Debug, Clone)]
pub(crate) struct VaultArgs {
    /// The vault directory agentmemory's Obsidian export wrote.
    #[arg(long)]
    pub(crate) dir: PathBuf,
    #[arg(long)]
    pub(crate) workspace: Option<String>,
    #[arg(long)]
    pub(crate) project: Option<String>,
    /// Wiki path prefix every imported page lands under.
    #[arg(long, default_value = DEFAULT_DEST_PREFIX)]
    pub(crate) dest_prefix: String,
    #[arg(long, env = "AI_MEMORY_SERVER_URL", default_value = DEFAULT_SERVER_URL)]
    pub(crate) server_url: String,
    /// Write the pages. Without this flag the command only prints a plan.
    #[arg(long)]
    pub(crate) apply: bool,
    /// Durable import manifest. Required with --apply.
    #[arg(long)]
    pub(crate) manifest_out: Option<PathBuf>,
    /// Let the first write create a missing destination workspace/project.
    #[arg(long)]
    pub(crate) create_destination: bool,
    /// Print every planned page body during the dry run.
    #[arg(long)]
    pub(crate) show_body: bool,
}

/// One vault page, parsed but not yet linked.
struct VaultPage {
    folder: &'static str,
    record: &'static str,
    rel: String,
    id: String,
    path: String,
    frontmatter: YamlMap,
    body: String,
}

pub(crate) async fn run(args: VaultArgs) -> Result<()> {
    if args.apply {
        if args
            .workspace
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
            || args.project.as_deref().is_none_or(|s| s.trim().is_empty())
        {
            bail!("--apply requires explicit --workspace and --project");
        }
        if args.manifest_out.is_none() {
            bail!("--apply requires --manifest-out <path>");
        }
    }
    let destination = Destination {
        workspace: validate_label(
            args.workspace.as_deref().unwrap_or("DRY-RUN-WORKSPACE"),
            "workspace",
            128,
        )?
        .to_owned(),
        project_id: None,
        project: validate_label(
            args.project.as_deref().unwrap_or("DRY-RUN-PROJECT"),
            "project",
            128,
        )?
        .to_owned(),
    };
    let prefix = validate_dest_prefix(&args.dest_prefix)?;
    let plan = plan(&args.dir, &destination, &prefix)?;
    let mut manifest = Manifest::new(
        IMPORT_VERSION,
        args.dir.to_string_lossy().into_owned(),
        plan.source_sha256,
        &plan.pages,
    );
    manifest.dropped = plan.dropped;

    if !args.apply {
        print_plan(&plan.pages, &manifest, args.show_body);
        if let Some(path) = &args.manifest_out {
            manifest.write(path)?;
        }
        return Ok(());
    }
    let manifest_path = args
        .manifest_out
        .as_deref()
        .ok_or_else(|| anyhow!("--apply requires --manifest-out <path>"))?;
    manifest.write(manifest_path)?;
    let client = ImportClient::new(&args.server_url)?;
    let destinations = plan.pages.iter().map(|p| p.destination.clone()).collect();
    if let Err(error) = preflight(&client, &destinations, args.create_destination).await {
        return Err(manifest.fail(manifest_path, error));
    }
    apply_pages(&client, &plan.pages, &mut manifest, manifest_path).await?;
    print_done(&manifest);
    Ok(())
}

#[derive(Debug)]
pub(crate) struct VaultPlan {
    pub(crate) pages: Vec<PlannedPage>,
    pub(crate) dropped: BTreeMap<String, usize>,
    pub(crate) source_sha256: String,
}

/// Read, validate and plan a whole vault. No network, no writes.
pub(crate) fn plan(dir: &Path, destination: &Destination, prefix: &str) -> Result<VaultPlan> {
    let root =
        fs::canonicalize(dir).with_context(|| format!("read vault dir {}", dir.display()))?;
    if !root.is_dir() {
        bail!("--dir must be a directory");
    }
    let mut dropped: BTreeMap<String, usize> = BTreeMap::new();
    count_unexported(&root, &mut dropped)?;

    let mut pages = Vec::new();
    let mut fingerprints = Vec::new();
    let mut destinations: HashMap<String, String> = HashMap::new();
    for &(folder, record) in FOLDERS {
        let folder_root = root.join(folder);
        let Ok(meta) = fs::symlink_metadata(&folder_root) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            bail!("vault folder {folder}/ is a symlink; refusing to follow it");
        }
        if !meta.is_dir() {
            bail!("vault entry {folder} is not a directory");
        }
        let walker = WalkDir::new(&folder_root)
            .follow_links(false)
            .sort_by_file_name()
            .min_depth(1);
        for entry in walker {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().is_symlink() {
                bail!(
                    "vault entry {} is a symlink; refusing to follow it",
                    entry.path().display()
                );
            }
            if entry.file_type().is_dir() {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&root)?
                .to_string_lossy()
                .replace('\\', "/");
            validate_source_rel(&rel)?;
            if rel.split('/').any(|segment| segment.starts_with('.')) {
                *dropped.entry("hidden entries".into()).or_default() += 1;
                continue;
            }
            if !name.ends_with(".md") {
                *dropped.entry("non-markdown files".into()).or_default() += 1;
                continue;
            }
            if entry.metadata()?.len() > MAX_FILE_BYTES {
                bail!("vault page {rel} is larger than {MAX_FILE_BYTES} bytes");
            }
            if pages.len() == MAX_FILES {
                bail!("vault has more than {MAX_FILES} pages");
            }
            let content =
                fs::read_to_string(entry.path()).with_context(|| format!("read {rel}"))?;
            fingerprints.push((rel.clone(), sha256_hex(content.as_bytes())));
            let (frontmatter, body) =
                split_frontmatter(&content).with_context(|| format!("parse {rel}"))?;
            let frontmatter = frontmatter.unwrap_or_default();
            let stem = name.trim_end_matches(".md");
            let id = yaml_string(&frontmatter, "id").unwrap_or_else(|| stem.to_owned());
            validate_id(&id, record).with_context(|| format!("in {rel}"))?;

            let inner = rel
                .strip_prefix(folder)
                .and_then(|r| r.strip_prefix('/'))
                .unwrap_or(&rel);
            let mut segments: Vec<String> = inner.split('/').map(str::to_owned).collect();
            let file = segments.pop().unwrap_or_default();
            let mut path = format!("{prefix}/{folder}");
            for segment in &segments {
                path.push('/');
                path.push_str(&slugify(segment));
            }
            path.push('/');
            path.push_str(&slugify(file.trim_end_matches(".md")));
            path.push_str(".md");
            validate_planned_path(&path, prefix)?;
            if let Some(other) = destinations.insert(path.clone(), rel.clone()) {
                bail!("duplicate destination path collision: {other} and {rel} both map to {path}");
            }
            pages.push(VaultPage {
                folder,
                record,
                rel,
                id,
                path,
                frontmatter,
                body,
            });
        }
    }

    // Every way the export spells a link to a page: its id, its file stem,
    // and the MOC's `<folder>/<stem>` form.
    let mut links: HashMap<String, String> = HashMap::new();
    for page in &pages {
        let target = link_target(&page.path);
        let stem = page
            .rel
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .trim_end_matches(".md")
            .to_owned();
        let inner = page
            .rel
            .strip_prefix(page.folder)
            .and_then(|r| r.strip_prefix('/'))
            .unwrap_or(&page.rel)
            .trim_end_matches(".md")
            .to_owned();
        for alias in [
            page.id.clone(),
            stem.clone(),
            inner.clone(),
            format!("{}/{stem}", page.folder),
            format!("{}/{inner}", page.folder),
            format!("{}/{}", page.folder, page.id),
        ] {
            links.entry(alias).or_insert_with(|| target.clone());
        }
    }

    let mut planned = Vec::with_capacity(pages.len());
    for page in pages {
        let linked = rewrite_wikilinks(&page.body, |target| {
            links.get(target.trim_end_matches(".md")).cloned()
        });
        let body = ai_memory_client::sanitize_external_text(&linked);
        let title = first_h1_line(&body)
            .map(|title| one_line(title, MAX_EXPLICIT_TITLE_CHARS))
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| one_line(&page.id, MAX_EXPLICIT_TITLE_CHARS));
        let (kind, tier, tags) = match page.record {
            "memory" => {
                let memory_type = yaml_string(&page.frontmatter, "type").unwrap_or_default();
                let (kind, tier) = memory_kind_tier(&memory_type);
                (kind, tier, string_list(&page.frontmatter, "concepts"))
            }
            "lesson" => ("lesson", "semantic", string_list(&page.frontmatter, "tags")),
            "crystal" => ("crystal", "episodic", Vec::new()),
            _ => ("session", "episodic", Vec::new()),
        };
        let version = page
            .frontmatter
            .get("version")
            .and_then(serde_json::Value::as_u64);
        let key = ingest_key(&format!("vault-{}", page.record), &page.id);
        let body = format!(
            "{}\n\n---\n{}\n",
            body.trim_end(),
            provenance_line(page.record, &page.id, version, &key)
        );
        planned.push(PlannedPage {
            record: page.record,
            destination: destination.clone(),
            path: page.path,
            versions: vec![PlannedVersion {
                key,
                source_id: page.id,
                version,
                title: ai_memory_client::sanitize_external_text(&title),
                body,
                kind: kind.to_owned(),
                tier,
                tags: tags_from(tags.iter().map(String::as_str)),
                expires_at: None,
            }],
        });
    }

    fingerprints.sort();
    let mut parts: Vec<&[u8]> = vec![IMPORT_VERSION.as_bytes()];
    for (rel, sha) in &fingerprints {
        parts.push(rel.as_bytes());
        parts.push(sha.as_bytes());
    }
    Ok(VaultPlan {
        pages: planned,
        dropped,
        source_sha256: sha256_parts(&parts),
    })
}

/// Count what the vault holds outside the four exported folders.
fn count_unexported(root: &Path, dropped: &mut BTreeMap<String, usize>) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if FOLDERS.iter().any(|(folder, _)| *folder == name) {
            continue;
        }
        let reason = if name == "MOC.md" {
            "index page (MOC.md)"
        } else if name.starts_with('.') {
            "hidden entries"
        } else {
            "entries outside memories/lessons/crystals/sessions"
        };
        *dropped.entry(reason.into()).or_default() += 1;
    }
    Ok(())
}

fn string_list(map: &YamlMap, key: &str) -> Vec<String> {
    match map.get(key) {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        Some(serde_json::Value::String(item)) => vec![item.clone()],
        _ => Vec::new(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::agentmemory::mock::{Failure, MockServer};
    use tempfile::tempdir;

    fn put(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    /// A synthetic vault in the shape `obsidian-export` writes: JSON-valued
    /// frontmatter, an H1, `[[id]]` links, and a generated `MOC.md`.
    pub(crate) fn fixture_vault(root: &Path) {
        put(
            root,
            "memories/mem_alpha.md",
            "---\nid: \"mem_alpha\"\ntype: \"architecture\"\ncreated: \"2026-01-02T03:04:05.000Z\"\nupdated: \"2026-01-03T03:04:05.000Z\"\nstrength: 0.9\nversion: 2\nconcepts: [\"writer actor\", \"sqlite\"]\nfiles: [\"src/store.rs\"]\n---\n\n# Single writer owns the store\n\nEvery mutation goes through one writer thread.\n\n## Concepts\n#writer-actor #sqlite\n\n## Related\n- [[mem_beta]]\n- [[mem_gone]]",
        );
        put(
            root,
            "memories/mem_beta.md",
            "---\nid: \"mem_beta\"\ntype: \"bug\"\ncreated: \"2026-01-04T00:00:00.000Z\"\nupdated: \"2026-01-04T00:00:00.000Z\"\nstrength: 0.5\nversion: 1\nconcepts: []\nfiles: []\n---\n\n# Retry storms on reconnect\n\nBack off exponentially; see [[lesson_one]].",
        );
        put(
            root,
            "lessons/lesson_one.md",
            "---\nid: \"lesson_one\"\ntype: \"lesson\"\nsource: \"manual\"\nconfidence: 0.8\nreinforcements: 3\ncreated: \"2026-01-05T00:00:00.000Z\"\nupdated: \"2026-01-05T00:00:00.000Z\"\nproject: \"demo\"\ntags: [\"networking\"]\ndecayRate: 0.01\n---\n\n# Lesson: Always cap retries\n\nAlways cap retries.\n\n## Sources\n- [[crys_one]]",
        );
        put(
            root,
            "crystals/crys_one.md",
            "---\nid: \"crys_one\"\ntype: \"crystal\"\ncreated: \"2026-01-06T00:00:00.000Z\"\nproject: \"demo\"\nsessionId: \"ses_one\"\nfilesAffected: [\"src/net.rs\"]\n---\n\n# Crystal: Reconnect work\n\nReconnect work.\n\n## Key Outcomes\n- capped retries\n\n## Source Actions\n- [[act_dropped]]",
        );
        put(
            root,
            "sessions/ses_one.md",
            "---\nid: \"ses_one\"\ntype: \"session\"\nproject: \"demo\"\nstatus: \"completed\"\nstarted: \"2026-01-06T00:00:00.000Z\"\nended: \"2026-01-06T01:00:00.000Z\"\nobservations: 4\n---\n\n# Session: demo\n\n**Status:** completed\n**Observations:** 4\n**CWD:** `/work/demo`",
        );
        put(
            root,
            "MOC.md",
            "---\ntype: moc\n---\n\n# agentmemory vault\n\n- [[memories/mem_alpha|Single writer owns the store]]",
        );
        put(root, "memories/attachment.png", "not markdown");
    }

    fn dest() -> Destination {
        Destination {
            workspace: "w".into(),
            project: "p".into(),
            project_id: None,
        }
    }

    #[test]
    fn plan_counts_equal_the_fixture_and_paths_stay_under_the_prefix() {
        let td = tempdir().unwrap();
        fixture_vault(td.path());
        let plan = plan(td.path(), &dest(), DEFAULT_DEST_PREFIX).unwrap();
        let manifest = Manifest::new(IMPORT_VERSION, String::new(), String::new(), &plan.pages);
        assert_eq!(manifest.planned["memory_pages"], 2);
        assert_eq!(manifest.planned["lesson_pages"], 1);
        assert_eq!(manifest.planned["crystal_pages"], 1);
        assert_eq!(manifest.planned["session_pages"], 1);
        assert_eq!(plan.dropped["index page (MOC.md)"], 1);
        assert_eq!(plan.dropped["non-markdown files"], 1);
        for page in &plan.pages {
            assert!(
                page.path.starts_with("imported/agentmemory/"),
                "{}",
                page.path
            );
            assert!(!page.path.starts_with("sessions/"));
        }
        let session = plan.pages.iter().find(|p| p.record == "session").unwrap();
        assert_eq!(session.path, "imported/agentmemory/sessions/ses-one.md");
    }

    #[test]
    fn links_point_at_imported_paths_or_become_plain_text() {
        let td = tempdir().unwrap();
        fixture_vault(td.path());
        let plan = plan(td.path(), &dest(), DEFAULT_DEST_PREFIX).unwrap();
        let alpha = plan
            .pages
            .iter()
            .find(|p| p.path.ends_with("mem-alpha.md"))
            .unwrap();
        let body = &alpha.versions[0].body;
        assert!(
            body.contains("[[imported/agentmemory/memories/mem-beta]]"),
            "{body}"
        );
        assert!(body.contains("- mem_gone"), "{body}");
        assert!(!body.contains("[[mem_gone]]"), "{body}");
        let crystal = plan.pages.iter().find(|p| p.record == "crystal").unwrap();
        assert!(crystal.versions[0].body.contains("- act_dropped"));
        assert_eq!(alpha.versions[0].kind, "decision");
        assert_eq!(alpha.versions[0].title, "Single writer owns the store");
        assert_eq!(
            alpha.versions[0].tags,
            ["agentmemory-import", "writer actor", "sqlite"]
        );
    }

    #[test]
    fn hostile_vaults_are_refused_before_any_request() {
        // A reserved, absolute or traversing prefix.
        let td = tempdir().unwrap();
        fixture_vault(td.path());
        for prefix in ["sessions", "/abs", "../up", "_rules/x"] {
            assert!(validate_dest_prefix(prefix).is_err(), "{prefix}");
        }
        // Two files whose slugs collide.
        put(td.path(), "memories/Mem Alpha.md", "# Dup");
        let error = plan(td.path(), &dest(), DEFAULT_DEST_PREFIX).unwrap_err();
        assert!(error.to_string().contains("collision"), "{error}");

        // A credential-shaped id in frontmatter.
        let td = tempdir().unwrap();
        put(
            td.path(),
            "memories/m.md",
            // Split so secret scanners do not flag the fixture.
            concat!(
                "---\nid: \"ghp_",
                "1234567890abcdefghijklmnopqrstuvwxyzAB\"\n---\n# M"
            ),
        );
        assert!(plan(td.path(), &dest(), DEFAULT_DEST_PREFIX).is_err());

        // A frontmatter id with traversal never names a path: the file name
        // does, slugged.
        let td = tempdir().unwrap();
        put(
            td.path(),
            "memories/m.md",
            "---\nid: \"../../_rules/evil\"\n---\n# M",
        );
        let planned = plan(td.path(), &dest(), DEFAULT_DEST_PREFIX).unwrap();
        assert_eq!(planned.pages[0].path, "imported/agentmemory/memories/m.md");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_inside_the_vault_are_refused() {
        let td = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("secret.md"), "# outside").unwrap();
        fs::create_dir_all(td.path().join("memories")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.md"),
            td.path().join("memories/link.md"),
        )
        .unwrap();
        let error = plan(td.path(), &dest(), DEFAULT_DEST_PREFIX).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error}");

        let td = tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), td.path().join("lessons")).unwrap();
        let error = plan(td.path(), &dest(), DEFAULT_DEST_PREFIX).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error}");
    }

    #[test]
    fn page_bodies_are_redacted_before_planning() {
        let td = tempdir().unwrap();
        put(
            td.path(),
            "memories/m.md",
            "---\nid: \"m\"\n---\n# M\n\ntoken=github_pat_1234567890abcdefghijklmnop",
        );
        let planned = plan(td.path(), &dest(), DEFAULT_DEST_PREFIX).unwrap();
        let body = &planned.pages[0].versions[0].body;
        assert!(
            !body.contains("github_pat_1234567890abcdefghijklmnop"),
            "{body}"
        );
        assert!(body.contains("REDACTED"), "{body}");
    }

    fn args(dir: &Path, url: &str, manifest: &Path) -> VaultArgs {
        VaultArgs {
            dir: dir.to_path_buf(),
            workspace: Some("w".into()),
            project: Some("p".into()),
            dest_prefix: DEFAULT_DEST_PREFIX.into(),
            server_url: url.into(),
            apply: true,
            manifest_out: Some(manifest.to_path_buf()),
            create_destination: false,
            show_body: false,
        }
    }

    fn read_manifest(path: &Path) -> serde_json::Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn apply_writes_create_only_and_a_rerun_writes_nothing() {
        let td = tempdir().unwrap();
        fixture_vault(td.path());
        let server = MockServer::start(&[("w", "p")]);
        let manifest = td.path().join("manifest.json");
        run(args(td.path(), &server.url, &manifest)).await.unwrap();
        assert_eq!(server.state().stored_versions(), 5);
        assert!(
            server
                .state()
                .pages
                .values()
                .flatten()
                .all(|v| v.arguments["create_only"] == true)
        );
        let first = read_manifest(&manifest);
        assert_eq!(first["status"], "imported");
        assert_eq!(first["written_versions"], 5);

        run(args(td.path(), &server.url, &manifest)).await.unwrap();
        assert_eq!(server.state().stored_versions(), 5, "a rerun writes 0");
        let second = read_manifest(&manifest);
        assert_eq!(second["written_versions"], 0);
        assert_eq!(second["present_versions"], 5);
    }

    /// A rerun reads each page up to the server's 64,000 character ceiling
    /// (12,000 when not asked, #1827), so a long page it wrote is still
    /// present; a page past the ceiling stops the run with that reason
    /// instead of being reported as somebody else's.
    #[tokio::test]
    async fn a_rerun_compares_long_pages_whole_or_says_why_it_cannot() {
        let td = tempdir().unwrap();
        let line = "Every mutation goes through one writer thread.\n";
        put(
            td.path(),
            "memories/long.md",
            &format!("---\nid: \"long\"\n---\n# Long\n\n{}", line.repeat(500)),
        );
        let server = MockServer::start(&[("w", "p")]);
        let manifest = td.path().join("manifest.json");
        run(args(td.path(), &server.url, &manifest)).await.unwrap();
        run(args(td.path(), &server.url, &manifest)).await.unwrap();
        let second = read_manifest(&manifest);
        assert_eq!(second["written_versions"], 0, "{second}");
        assert_eq!(second["present_versions"], 1, "{second}");

        put(
            td.path(),
            "memories/longer.md",
            &format!(
                "---\nid: \"longer\"\n---\n# Longer\n\n{}",
                line.repeat(1500)
            ),
        );
        run(args(td.path(), &server.url, &manifest)).await.unwrap();
        assert_eq!(server.state().stored_versions(), 2);
        let error = run(args(td.path(), &server.url, &manifest))
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("longer than memory_read_page returns"),
            "{message}"
        );
        assert!(!message.contains("did not write"), "{message}");
        assert_eq!(server.state().stored_versions(), 2);
    }

    #[tokio::test]
    async fn a_foreign_page_at_the_destination_is_never_replaced() {
        let td = tempdir().unwrap();
        put(td.path(), "memories/m.md", "---\nid: \"m\"\n---\n# M");
        let server = MockServer::start(&[("w", "p")]);
        server.state().pages.insert(
            (
                "w".into(),
                "p".into(),
                "imported/agentmemory/memories/m.md".into(),
            ),
            vec![crate::agentmemory::mock::StoredVersion {
                page_id: "someone-else".into(),
                body: "# Hand written".into(),
                arguments: serde_json::Value::Null,
            }],
        );
        let manifest = td.path().join("manifest.json");
        let error = run(args(td.path(), &server.url, &manifest))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("did not write"), "{error:#}");
        let chain = server
            .state()
            .chain("w", "p", "imported/agentmemory/memories/m.md");
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].page_id, "someone-else");
        assert_eq!(read_manifest(&manifest)["status"], "failed");
    }

    #[tokio::test]
    async fn a_server_older_than_2_7_is_refused_before_any_write() {
        let td = tempdir().unwrap();
        fixture_vault(td.path());
        let server = MockServer::start(&[("w", "p")]);
        server.state().conditional = false;
        let manifest = td.path().join("manifest.json");
        let error = run(args(td.path(), &server.url, &manifest))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(">= 3.0"), "{error:#}");
        assert_eq!(server.state().write_calls, 0);
    }

    #[tokio::test]
    async fn established_routes_do_not_require_the_new_batch_tool() {
        let td = tempdir().unwrap();
        fixture_vault(td.path());
        let server = MockServer::start(&[("w", "p")]);
        server.state().batch = false;
        let manifest = td.path().join("manifest.json");
        run(args(td.path(), &server.url, &manifest)).await.unwrap();
        assert_eq!(server.state().stored_versions(), 5);
        assert_eq!(server.state().batch_calls, 0);
    }

    #[tokio::test]
    async fn a_missing_destination_is_refused_without_create_destination() {
        let td = tempdir().unwrap();
        fixture_vault(td.path());
        let server = MockServer::start(&[]);
        let manifest = td.path().join("manifest.json");
        let error = run(args(td.path(), &server.url, &manifest))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("must already exist"),
            "{error:#}"
        );
        assert_eq!(server.state().write_calls, 0);

        let mut create = args(td.path(), &server.url, &manifest);
        create.create_destination = true;
        run(create).await.unwrap();
        assert_eq!(server.state().stored_versions(), 5);
    }

    #[tokio::test]
    async fn an_interrupted_import_resumes_without_duplicates() {
        for failure in [Failure::Before(3), Failure::After(3)] {
            let td = tempdir().unwrap();
            fixture_vault(td.path());
            let server = MockServer::start(&[("w", "p")]);
            server.state().failure = Some(failure);
            let manifest = td.path().join("manifest.json");
            assert!(run(args(td.path(), &server.url, &manifest)).await.is_err());
            let failed = read_manifest(&manifest);
            assert_eq!(failed["status"], "failed");
            server.state().failure = None;
            run(args(td.path(), &server.url, &manifest)).await.unwrap();
            let state = server.state();
            assert_eq!(state.stored_versions(), 5, "{failure:?}");
            assert!(
                state.pages.values().all(|chain| chain.len() == 1),
                "{failure:?}"
            );
        }
    }

    #[tokio::test]
    async fn dry_run_makes_no_request() {
        let td = tempdir().unwrap();
        fixture_vault(td.path());
        let server = MockServer::start(&[("w", "p")]);
        let manifest = td.path().join("manifest.json");
        let mut dry = args(td.path(), &server.url, &manifest);
        dry.apply = false;
        run(dry).await.unwrap();
        assert!(server.state().requests.is_empty());
        let planned = read_manifest(&manifest);
        assert_eq!(planned["status"], "planned");
        assert!(
            planned["pages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|page| page["versions"][0]["status"] == "planned")
        );
    }
}
