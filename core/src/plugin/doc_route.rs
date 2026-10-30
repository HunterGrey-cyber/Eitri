//! The document route: `/.eitri/doc/<generation>/<path>` in a following plugin's origin serves the image files
//! of the followed document's project, so a page that renders the document can show its local images. The
//! document is untrusted text, so only regular image files inside the project are served, by canonical path,
//! and a request names the generation of the project it was rendered against: once the followed document moves
//! to another project, addresses made for the old one stop resolving.

use std::path::{Path, PathBuf};

use super::page_policy::{decoded, encode_path, mime_for, origin, request_path, SiteFile, SiteRefusal};
use super::protocol::DocPrefixes;

/// The route's place in a plugin's origin, as a decoded request path starts.
pub const DOC_PREFIX: &str = ".eitri/doc/";

/// The extensions the route serves, compared ASCII-case-insensitively. Every one has a type in the page
/// policy's table.
pub const IMAGE_TYPES: [&str; 9] = ["png", "jpg", "jpeg", "gif", "webp", "svg", "avif", "bmp", "ico"];

/// What the route currently serves for one plugin: the followed document's project directory (canonical, as
/// `project_dir` returns it), and a counter that grows each time that directory changes. Kept on the GTK
/// thread; the scheme handler reads it per request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocSlot {
    pub generation: u64,
    pub dir: Option<PathBuf>,
}

impl DocSlot {
    /// Records the project directory; a different one (including none) starts a new generation and says true.
    pub fn set_dir(&mut self, dir: Option<PathBuf>) -> bool {
        if self.dir == dir {
            return false;
        }
        self.dir = dir;
        self.generation += 1;
        true
    }

    /// A copy of what `slot` holds now, for the scheme handler, which reads it per request: the guard ends inside
    /// this call, so the handler never holds the slot while it answers.
    pub fn snapshot(slot: &std::cell::RefCell<DocSlot>) -> DocSlot {
        // The slot is being written while this request arrives: no project is known for this request, so a
        // document path is a 404 rather than a panic inside the scheme handler.
        slot.try_borrow().map(|slot| slot.clone()).unwrap_or_default()
    }
}

/// The project a document's local images may come from: the window's root when it holds the document, else the
/// nearest directory upward from the document holding `.git` (a directory or a file), else the document's own
/// directory. Home, any directory above home and `/` are never the project: each would put every file of the
/// user in reach of the document. `None` when nothing is left, or when the document's directory cannot be
/// resolved.
///
/// Reads the filesystem (`canonicalize`, `symlink_metadata`), which can stall on a slow or hung mount: call it
/// only on a worker thread, never on the GTK thread.
pub fn project_dir(doc: &Path, window_root: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    let dir = std::fs::canonicalize(doc.parent()?).ok()?;
    let root = window_root.and_then(|p| std::fs::canonicalize(p).ok());
    let home = home.and_then(|p| std::fs::canonicalize(p).ok());
    // `/` is above every home; with no home known it is still refused.
    let too_wide = |d: &Path| d.parent().is_none() || home.as_deref().is_some_and(|h| h.starts_with(d));
    if let Some(root) = root {
        if dir.starts_with(&root) && !too_wide(&root) {
            return Some(root);
        }
    }
    for ancestor in dir.ancestors() {
        // Above this point every directory holds home (or is `/`), so no repository found there may be used.
        if too_wide(ancestor) {
            break;
        }
        if std::fs::symlink_metadata(ancestor.join(".git")).is_ok() {
            return Some(ancestor.to_path_buf());
        }
    }
    (!too_wide(&dir)).then_some(dir)
}

/// The generation and the decoded relative path a request for the document route names, or `None` when it is
/// not one, or not a clean one: the generation all ASCII digits that fit a `u64`, the path not empty, relative,
/// with no `\` and no empty, `.` or `..` component.
pub fn doc_request(id: &str, uri: &str) -> Option<(u64, String)> {
    let path = decoded(request_path(id, uri)?)?;
    let rest = path.strip_prefix(DOC_PREFIX)?;
    let (generation, rel) = rest.split_once('/')?;
    if generation.is_empty() || !generation.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let generation: u64 = generation.parse().ok()?;
    if rel.is_empty()
        || rel.starts_with('/')
        || rel.contains('\\')
        || rel.split('/').any(|c| c.is_empty() || c == "." || c == "..")
    {
        return None;
    }
    Some((generation, rel.to_owned()))
}

/// Whether `name`'s extension (after its last `.`, ASCII-case-insensitively) is one the route serves.
fn is_image_name(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| IMAGE_TYPES.contains(&ext.to_ascii_lowercase().as_str()))
}

/// The file a document-route path names under `project_dir`: a regular file whose canonical path stays inside
/// the project and whose name, both as asked for and as it resolves, has an image type. Reads the filesystem:
/// a worker thread only.
///
/// `project_dir` is the canonical directory that was approved (as `project_dir` returns it), and containment is
/// judged against exactly that path. It is not resolved again: if the directory has since been replaced by a
/// symlink, following it would move the boundary to wherever the link points, so a project that no longer
/// resolves to itself serves nothing.
pub fn doc_file(project_dir: &Path, rel: &str) -> Result<SiteFile, SiteRefusal> {
    if !is_image_name(rel) {
        return Err(SiteRefusal::UnknownType);
    }
    if std::fs::canonicalize(project_dir).ok().as_deref() != Some(project_dir) {
        return Err(SiteRefusal::OutsideSite);
    }
    let canonical = std::fs::canonicalize(project_dir.join(rel)).map_err(|_| SiteRefusal::NotFound)?;
    if !canonical.starts_with(project_dir) {
        return Err(SiteRefusal::OutsideSite);
    }
    // A link named like an image may point at any other file of the project; only an image is served.
    let target = canonical.to_string_lossy();
    if !is_image_name(&target) {
        return Err(SiteRefusal::UnknownType);
    }
    let mime = mime_for(&target).ok_or(SiteRefusal::UnknownType)?;
    let meta = std::fs::metadata(&canonical).map_err(|_| SiteRefusal::NotFound)?;
    if !meta.is_file() {
        return Err(SiteRefusal::NotFound);
    }
    Ok(SiteFile {
        path: canonical,
        mime,
        len: meta.len(),
    })
}

/// The addresses a page resolves a document's relative image paths against: `root` names the project of this
/// generation, `base` the document's own directory inside it (`root` itself when the document sits at the
/// project's top, or outside it).
pub fn doc_prefixes(id: &str, generation: u64, project_dir: &Path, doc_dir: &Path) -> DocPrefixes {
    let root = format!("{}/{DOC_PREFIX}{generation}/", origin(id));
    let base = match doc_dir.strip_prefix(project_dir) {
        Ok(rel) if !rel.as_os_str().is_empty() => format!("{root}{}/", encode_path(&rel.to_string_lossy())),
        _ => root.clone(),
    };
    DocPrefixes { root, base }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::test_support::Scratch;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }

    #[test]
    fn the_window_root_then_the_repository_then_the_documents_directory() {
        let s = Scratch::new("doc-project");
        let home = s.0.join("home");
        fs::create_dir_all(&home).unwrap();
        let proj = s.0.join("proj");
        fs::create_dir_all(proj.join(".git")).unwrap();
        let doc = proj.join("docs/a.md");
        touch(&doc);
        assert_eq!(project_dir(&doc, Some(&proj), Some(&home)), Some(proj.clone()));
        assert_eq!(
            project_dir(&doc, None, Some(&home)),
            Some(proj.clone()),
            "the .git ancestor"
        );
        // A worktree or submodule marks itself with a `.git` file; the nearest marker wins.
        fs::write(proj.join("docs/.git"), "gitdir: ../.git/worktrees/docs\n").unwrap();
        assert_eq!(project_dir(&doc, None, Some(&home)), Some(proj.join("docs")));
        let loose = s.0.join("loose/b.md");
        touch(&loose);
        assert_eq!(project_dir(&loose, None, Some(&home)), Some(s.0.join("loose")));
        // A window root that does not hold the document has no say.
        let other = s.0.join("other");
        fs::create_dir_all(&other).unwrap();
        assert_eq!(project_dir(&doc, Some(&other), Some(&home)), Some(proj.join("docs")));
        assert_eq!(project_dir(&loose, Some(&other), Some(&home)), Some(s.0.join("loose")));
        // A window root that does not exist counts as none.
        assert_eq!(
            project_dir(&loose, Some(&s.0.join("gone")), Some(&home)),
            Some(s.0.join("loose"))
        );
        // A document whose directory is gone has no project.
        assert_eq!(project_dir(&s.0.join("gone/c.md"), None, Some(&home)), None);
    }

    #[test]
    fn home_and_the_filesystem_root_are_never_the_project() {
        let s = Scratch::new("doc-home");
        let home = s.0.join("home");
        fs::create_dir_all(home.join(".git")).unwrap();
        let note = home.join("notes/n.md");
        touch(&note);
        assert_eq!(project_dir(&note, Some(&home), Some(&home)), Some(home.join("notes")));
        assert_eq!(project_dir(&note, None, Some(&home)), Some(home.join("notes")));
        let direct = home.join("n.md");
        touch(&direct);
        assert_eq!(project_dir(&direct, Some(&home), Some(&home)), None);
        assert_eq!(project_dir(&direct, None, Some(&home)), None);
        assert_eq!(project_dir(Path::new("/x.md"), None, None), None);
        // A directory above home, as a window root or as a repository, would reach all of home.
        fs::create_dir_all(s.0.join(".git")).unwrap();
        assert_eq!(project_dir(&note, Some(&s.0), Some(&home)), Some(home.join("notes")));
        let beside = s.0.join("beside/d.md");
        touch(&beside);
        assert_eq!(project_dir(&beside, None, Some(&home)), Some(s.0.join("beside")));
        // Without a known home, a repository above the document is used as usual.
        assert_eq!(project_dir(&beside, None, None), Some(s.0.clone()));
    }

    #[test]
    fn a_generation_and_a_clean_path_are_required() {
        assert_eq!(
            doc_request("markdown", "eitri-plugin://markdown/.eitri/doc/4/img/a%20b.png?x#y"),
            Some((4, "img/a b.png".to_owned()))
        );
        assert_eq!(
            doc_request("markdown", "eitri-plugin://markdown/%2Eeitri/doc/0/a.png"),
            Some((0, "a.png".to_owned()))
        );
        for path in [
            ".eitri/doc/x/a.png",
            ".eitri/doc/+4/a.png",
            ".eitri/doc/-4/a.png",
            ".eitri/doc//a.png",
            ".eitri/doc/4",
            ".eitri/doc/4/",
            ".eitri/doc/4//a.png",
            ".eitri/doc/4/../a.png",
            ".eitri/doc/4/a/./b.png",
            ".eitri/doc/4/a/",
            ".eitri/doc/4/a%2f..%2f..%2fb.png",
            ".eitri/doc/4/%2Fetc/a.png",
            ".eitri/doc/4/%00.png",
            ".eitri/doc/4/bad%zz.png",
            ".eitri/doc/4/a\\b.png",
            ".eitri/doc/4/a%5Cb.png",
            ".eitri/doc/99999999999999999999/a.png",
            ".eitri/keys.js",
            "x/.eitri/doc/4/a.png",
        ] {
            let uri = format!("eitri-plugin://markdown/{path}");
            assert_eq!(doc_request("markdown", &uri), None, "{path}");
        }
        assert_eq!(
            doc_request("markdown", "eitri-plugin://other/.eitri/doc/4/a.png"),
            None,
            "another origin"
        );
        assert_eq!(
            doc_request("markdown", "eitri-plugin://markdown.evil/.eitri/doc/4/a.png"),
            None,
            "an origin that only starts with the id"
        );
    }

    #[test]
    fn only_regular_image_files_inside_the_project_are_served() {
        let s = Scratch::new("doc-file");
        let proj = s.0.join("proj");
        fs::create_dir_all(proj.join("img")).unwrap();
        fs::write(proj.join("img/a.png"), "12345").unwrap();
        fs::write(proj.join("img/A.PNG"), "1").unwrap();
        fs::write(proj.join("img/x.svg"), "<svg/>").unwrap();
        fs::write(proj.join("notes.md"), "# n").unwrap();
        fs::write(proj.join("x.avif"), "1").unwrap();
        fs::write(proj.join("x.bmp"), "1").unwrap();
        fs::create_dir_all(proj.join("dir.png")).unwrap();
        fs::write(s.0.join("secret.png"), "s").unwrap();
        symlink(s.0.join("secret.png"), proj.join("out.png")).unwrap();
        symlink(proj.join("img/x.svg"), proj.join("in.svg")).unwrap();
        // A relative link that climbs out of the project is refused like an absolute one.
        symlink("../../secret.png", proj.join("img/up.png")).unwrap();

        let a = doc_file(&proj, "img/a.png").unwrap();
        assert_eq!((a.path, a.mime, a.len), (proj.join("img/a.png"), "image/png", 5));
        assert_eq!(doc_file(&proj, "img/A.PNG").unwrap().mime, "image/png");
        assert_eq!(doc_file(&proj, "notes.md"), Err(SiteRefusal::UnknownType));
        assert_eq!(doc_file(&proj, "noext"), Err(SiteRefusal::UnknownType));
        assert_eq!(doc_file(&proj, "img/missing.png"), Err(SiteRefusal::NotFound));
        assert_eq!(doc_file(&proj, "dir.png"), Err(SiteRefusal::NotFound));
        assert_eq!(doc_file(&proj, "out.png"), Err(SiteRefusal::OutsideSite));
        assert_eq!(doc_file(&proj, "img/up.png"), Err(SiteRefusal::OutsideSite));
        assert_eq!(doc_file(&proj, "../secret.png"), Err(SiteRefusal::OutsideSite));
        let inside = doc_file(&proj, "in.svg").unwrap();
        assert_eq!((inside.path, inside.mime), (proj.join("img/x.svg"), "image/svg+xml"));
        // A link named like an image that resolves to another kind of file of the project is not an image.
        symlink(proj.join("notes.md"), proj.join("pic.png")).unwrap();
        assert_eq!(doc_file(&proj, "pic.png"), Err(SiteRefusal::UnknownType));
        // The type served is the one of the file the link resolves to.
        symlink(proj.join("img/x.svg"), proj.join("logo.png")).unwrap();
        assert_eq!(doc_file(&proj, "logo.png").unwrap().mime, "image/svg+xml");
        assert_eq!(doc_file(&proj, "x.avif").unwrap().mime, "image/avif");
        assert_eq!(doc_file(&proj, "x.bmp").unwrap().mime, "image/bmp");
        for ext in IMAGE_TYPES {
            assert!(
                mime_for(&format!("a.{ext}")).is_some_and(|m| m.starts_with("image/")),
                "{ext}"
            );
        }
    }

    #[test]
    fn a_project_replaced_by_a_link_serves_nothing() {
        let s = Scratch::new("doc-swap");
        let proj = s.0.join("proj");
        fs::create_dir_all(proj.join("img")).unwrap();
        fs::write(proj.join("img/a.png"), "1").unwrap();
        let home = s.0.join("home");
        fs::create_dir_all(home.join("pictures")).unwrap();
        fs::write(home.join("pictures/private.png"), "p").unwrap();
        assert!(doc_file(&proj, "img/a.png").is_ok());
        // The approved directory moves away and a link to home takes its name.
        fs::rename(&proj, s.0.join("moved")).unwrap();
        symlink(&home, &proj).unwrap();
        assert_eq!(doc_file(&proj, "pictures/private.png"), Err(SiteRefusal::OutsideSite));
        // A directory that was never canonical is refused the same way, whatever it would resolve to.
        assert_eq!(
            doc_file(&s.0.join("home/../moved"), "img/a.png"),
            Err(SiteRefusal::OutsideSite)
        );
        assert!(doc_file(&s.0.join("moved"), "img/a.png").is_ok());
    }

    #[test]
    fn the_prefixes_name_the_generation_and_encode_the_documents_directory() {
        let p = Path::new("/w/proj");
        assert_eq!(
            doc_prefixes("markdown", 4, p, &p.join("docs/sub dir")),
            DocPrefixes {
                root: "eitri-plugin://markdown/.eitri/doc/4/".into(),
                base: "eitri-plugin://markdown/.eitri/doc/4/docs/sub%20dir/".into(),
            }
        );
        let top = doc_prefixes("markdown", 4, p, p);
        assert_eq!(top.base, top.root);
        let elsewhere = doc_prefixes("markdown", 7, p, Path::new("/w/other"));
        assert_eq!(elsewhere.base, "eitri-plugin://markdown/.eitri/doc/7/");
        // What the page resolves against the base comes back as the same relative path.
        let base = doc_prefixes("demo", 2, p, &p.join("a#b/c%d")).base;
        assert_eq!(base, "eitri-plugin://demo/.eitri/doc/2/a%23b/c%25d/");
        assert_eq!(
            doc_request("demo", &format!("{base}x.png")),
            Some((2, "a#b/c%d/x.png".to_owned()))
        );
    }

    #[test]
    fn the_slot_counts_each_change_of_directory() {
        let mut slot = DocSlot::default();
        assert_eq!((slot.generation, slot.dir.clone()), (0, None));
        assert!(slot.set_dir(Some(PathBuf::from("/a"))));
        assert_eq!(slot.generation, 1);
        assert!(!slot.set_dir(Some(PathBuf::from("/a"))));
        assert_eq!(slot.generation, 1);
        assert!(slot.set_dir(Some(PathBuf::from("/b"))));
        assert_eq!(slot.generation, 2);
        assert!(slot.set_dir(None));
        assert_eq!((slot.generation, slot.dir), (3, None));
    }
}
