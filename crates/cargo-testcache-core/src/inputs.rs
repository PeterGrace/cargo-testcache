//! Files tests read at runtime: matched by the configured `inputs` globs, and hashed.
//!
//! The workspace is walked once. Each file is matched against one compiled set holding the
//! workspace's globs (relative to its root) and every package's (relative to that package's
//! directory, rewritten to be relative to the root), and the matching files are hashed in
//! parallel. The target and build directories and `.git` are not walked, nor is any directory
//! that no glob's literal prefix leads to. Symlinked files and directories are followed, as a
//! test reading them would follow them; directories are compared to the target and build
//! directories by canonical path, because Cargo does not normalize the paths it reports.
//!
//! Before compiling, each glob's literal directories (those before its first wildcard) are
//! normalized: `.` is dropped and `..` resolved against the declaring package's directory, so
//! `../shared/**` in `crates/a` becomes `crates/shared/**`. A glob that would climb above the
//! workspace root, an absolute glob, and `..` after a wildcard are refused, since none of them
//! can ever match a walked file. A glob that matches no file is warned about once per run.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use globset::{GlobSet, GlobSetBuilder};
// The prelude adds `into_par_iter()`, rayon's parallel counterpart of `into_iter()`.
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::error::{Error, Result};
use crate::key::Digest;
use crate::manifest::glob;
use crate::workspace::Workspace;

/// Who declared a glob: the workspace, or the package at this index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Owner {
    Workspace,
    Package(usize),
}

/// Why a glob is refused when it reaches outside the workspace.
const OUTSIDE_WORKSPACE: &str = "inputs must be inside the workspace root, relative to the \
    package or workspace that declares them";

/// Why a glob is refused when `..` follows a wildcard.
const PARENT_AFTER_WILDCARD: &str = "`..` is only resolved before the first wildcard";

/// Characters that make a glob component a pattern rather than a literal name.
const WILDCARDS: [char; 5] = ['*', '?', '[', '{', '\\'];

/// One package's `inputs`, for [`InputPatterns::from_parts`].
#[derive(Debug, Clone)]
pub struct PackageGlobs<'a> {
    /// The package id.
    pub id: &'a str,
    /// The package name, for messages.
    pub name: &'a str,
    /// The package directory relative to the workspace root, with `/` separators; `None` when
    /// it is outside the root.
    pub dir: Option<String>,
    /// Its globs, relative to its directory.
    pub globs: &'a [String],
}

/// Every `inputs` glob, compiled into one set.
#[derive(Debug, Clone)]
pub struct InputPatterns {
    set: GlobSet,
    /// The owner of each glob in `set`, by index.
    owners: Vec<Owner>,
    /// Package id to its `Owner::Package` index.
    package_index: HashMap<String, usize>,
    /// Packages that declare inputs but lie outside the workspace root.
    outside: HashSet<String>,
    /// For each glob, the directory components before its first wildcard. An empty prefix means
    /// the glob can match anywhere.
    prefixes: Vec<Vec<String>>,
    /// For each glob, how it was written and the table that declares it, for messages.
    declared: Vec<(String, String)>,
}

impl InputPatterns {
    /// Compiles the workspace's and every package's globs.
    ///
    /// # Arguments
    ///
    /// * `workspace` - The workspace.
    ///
    /// # Returns
    ///
    /// The compiled patterns.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidGlob`] when a glob does not compile.
    pub fn new(workspace: &Workspace) -> Result<Self> {
        let packages = workspace.packages().map(|(id, package)| PackageGlobs {
            id,
            name: package.name(),
            dir: package
                .root()
                .strip_prefix(workspace.root())
                .ok()
                .map(slash_path),
            globs: &package.config().inputs,
        });
        Self::from_parts(&workspace.config().inputs, packages)
    }

    /// Compiles globs given directly.
    ///
    /// # Arguments
    ///
    /// * `workspace_globs` - Globs relative to the workspace root.
    /// * `packages` - Each package's globs and directory.
    ///
    /// # Returns
    ///
    /// The compiled patterns.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidGlob`] when a glob does not compile, is absolute, climbs above the
    /// workspace root, or has `..` after a wildcard.
    pub fn from_parts<'a>(
        workspace_globs: &[String],
        packages: impl IntoIterator<Item = PackageGlobs<'a>>,
    ) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        let mut owners = Vec::new();
        let mut package_index = HashMap::new();
        let mut outside = HashSet::new();
        let mut prefixes = Vec::new();
        let mut declared = Vec::new();
        // One closure adds a glob for either kind of table. It borrows the collections above
        // mutably until its last use, so they are moved into `Self` only after that.
        let mut add = |dir: &[&str], pattern: &str, owner: Owner, table: &str| -> Result<()> {
            let rooted = root_glob(dir, pattern)?;
            builder.add(glob(&rooted.glob)?);
            owners.push(owner);
            prefixes.push(rooted.prefix);
            declared.push((pattern.to_owned(), table.to_owned()));
            Ok(())
        };
        for pattern in workspace_globs {
            add(
                &[],
                pattern,
                Owner::Workspace,
                "[workspace.metadata.testcache]",
            )?;
        }
        for (index, package) in packages.into_iter().enumerate() {
            if package.globs.is_empty() {
                continue;
            }
            package_index.insert(package.id.to_owned(), index);
            let Some(dir) = package.dir else {
                outside.insert(package.id.to_owned());
                continue;
            };
            let dir_components: Vec<&str> =
                dir.split('/').filter(|part| !part.is_empty()).collect();
            let table = format!("[package.metadata.testcache] of `{}`", package.name);
            for pattern in package.globs {
                add(&dir_components, pattern, Owner::Package(index), &table)?;
            }
        }
        let set = builder.build().map_err(|source| Error::InvalidGlob {
            pattern: "inputs".to_owned(),
            reason: source.to_string(),
        })?;
        Ok(Self {
            set,
            owners,
            package_index,
            outside,
            prefixes,
            declared,
        })
    }

    /// Whether a directory could hold a file some glob matches: it lies on the way to a glob's
    /// literal prefix, or inside it. A glob without a literal prefix makes every directory
    /// possible.
    ///
    /// # Arguments
    ///
    /// * `relative` - The directory, relative to the workspace root, with `/` separators; empty
    ///   for the root.
    ///
    /// # Returns
    ///
    /// `false` only when no file below the directory can match.
    fn could_contain(&self, relative: &str) -> bool {
        self.prefixes.iter().any(|prefix| {
            relative
                .split('/')
                .filter(|part| !part.is_empty())
                .zip(prefix)
                .all(|(part, literal)| part == literal)
        })
    }

    /// The owners of the globs a workspace-relative path matches, sorted, without repeats.
    #[cfg(test)]
    fn owners_of(&self, relative: &str) -> Vec<Owner> {
        self.owners_of_globs(&self.set.matches(relative))
    }

    /// The owners of the globs at these indices, sorted, without repeats.
    fn owners_of_globs(&self, globs: &[usize]) -> Vec<Owner> {
        let mut owners: Vec<Owner> = globs.iter().map(|index| self.owners[*index]).collect();
        owners.sort_unstable();
        owners.dedup();
        owners
    }

    /// Warns once for each glob that matched no file.
    ///
    /// # Arguments
    ///
    /// * `matched` - Whether each glob, by index, matched a file.
    fn warn_unmatched(&self, matched: &[bool]) {
        for ((pattern, table), _) in self
            .declared
            .iter()
            .zip(matched)
            .filter(|(_, matched)| !**matched)
        {
            tracing::warn!("inputs glob {pattern:?} in {table} matched no file");
        }
    }
}

/// A glob made relative to the workspace root.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RootedGlob {
    /// The glob, its literal directories escaped.
    glob: String,
    /// Its literal components before the first wildcard, normalized.
    prefix: Vec<String>,
}

/// Roots a glob at its table's directory and normalizes its literal part.
///
/// # Arguments
///
/// * `dir` - The declaring package's directory components, relative to the workspace root;
///   empty for the workspace's own table.
/// * `pattern` - The glob as written, relative to that directory.
///
/// # Returns
///
/// The glob relative to the workspace root: `.` components before the first wildcard are
/// dropped, and each `..` there removes the component before it.
///
/// # Errors
///
/// [`Error::InvalidGlob`] when the glob is absolute, climbs above the workspace root, or has
/// `..` after its first wildcard.
fn root_glob(dir: &[&str], pattern: &str) -> Result<RootedGlob> {
    let invalid = |reason: &str| Error::InvalidGlob {
        pattern: pattern.to_owned(),
        reason: reason.to_owned(),
    };
    let as_path = Path::new(pattern);
    if pattern.starts_with('/') || as_path.has_root() || as_path.is_absolute() {
        return Err(invalid(OUTSIDE_WORKSPACE));
    }
    let mut prefix: Vec<String> = dir.iter().map(|part| (*part).to_owned()).collect();
    let mut components = pattern.split('/');
    let mut rest = Vec::new();
    // `by_ref` borrows the iterator, so the loop can stop at the first wildcard and the
    // remaining components are still there for `extend` below.
    for component in components.by_ref() {
        if component.contains(WILDCARDS) {
            rest.push(component);
            break;
        }
        match component {
            "" | "." => {}
            ".." => {
                if prefix.pop().is_none() {
                    return Err(invalid(OUTSIDE_WORKSPACE));
                }
            }
            name => prefix.push(name.to_owned()),
        }
    }
    rest.extend(components);
    if rest.contains(&"..") {
        return Err(invalid(PARENT_AFTER_WILDCARD));
    }
    let glob = prefix
        .iter()
        .map(|part| escape_literal(part))
        .chain(rest.iter().map(|part| (*part).to_owned()))
        .collect::<Vec<_>>()
        .join("/");
    Ok(RootedGlob { glob, prefix })
}

/// A literal name as a glob that matches only itself. `globset::escape` makes a name such as
/// `we[i]rd` literal; it leaves `\` alone, which these globs treat as an escape, so that is
/// wrapped in a class.
fn escape_literal(name: &str) -> String {
    globset::escape(name).replace('\\', "[\\]")
}

/// One matched file.
#[derive(Debug, Clone)]
struct InputFile {
    /// Workspace-relative, with `/` separators: the name it has in a key.
    relative: String,
    owners: Vec<Owner>,
    /// Its digest, or why it could not be read.
    digest: std::result::Result<Digest, String>,
}

/// Every matched input file, hashed.
#[derive(Debug, Clone)]
pub struct InputIndex {
    patterns: InputPatterns,
    files: Vec<InputFile>,
}

impl InputIndex {
    /// Walks the workspace and hashes every file an `inputs` glob matches.
    ///
    /// The root, target and build directories are canonicalized first (the raw path when that
    /// fails), because Cargo reports them as given. Symlinks to files and directories are
    /// followed; dangling symlinks and symlinks into unsearchable directories are ignored with
    /// a warning, since a test cannot read them either.
    ///
    /// # Arguments
    ///
    /// * `workspace` - The workspace.
    ///
    /// # Returns
    ///
    /// The index. With no globs configured, nothing is walked.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidGlob`] when a glob does not compile. Unreadable directories are skipped
    /// with a warning: a test running as the same user could not read them either.
    pub fn collect(workspace: &Workspace) -> Result<Self> {
        let patterns = InputPatterns::new(workspace)?;
        let matched = if patterns.owners.is_empty() {
            Vec::new()
        } else {
            matching_files(workspace, &patterns)
        };
        let files = matched
            .into_par_iter()
            .map(|found| InputFile {
                digest: if found.utf8 {
                    Digest::of_file(&found.path).map_err(|error| error.to_string())
                } else {
                    Err("the path is not UTF-8".to_owned())
                },
                relative: found.relative,
                owners: found.owners,
            })
            .collect();
        Ok(Self { patterns, files })
    }

    /// A package's inputs: the files the workspace's globs or its own match.
    ///
    /// # Arguments
    ///
    /// * `package_id` - The package. An id that is not known gets only the workspace's inputs.
    ///
    /// # Returns
    ///
    /// Workspace-relative path to digest, or why the inputs cannot be known (an unreadable
    /// file, or a package outside the workspace root that declares inputs).
    pub fn for_package(
        &self,
        package_id: &str,
    ) -> std::result::Result<BTreeMap<String, Digest>, String> {
        if self.patterns.outside.contains(package_id) {
            return Err("the package declares inputs but is outside the workspace root".to_owned());
        }
        let own = self
            .patterns
            .package_index
            .get(package_id)
            .copied()
            .map(Owner::Package);
        self.files
            .iter()
            .filter(|file| {
                file.owners
                    .iter()
                    .any(|owner| *owner == Owner::Workspace || Some(*owner) == own)
            })
            .map(|file| match &file.digest {
                Ok(digest) => Ok((file.relative.clone(), *digest)),
                Err(error) => Err(format!("cannot read input {}: {error}", file.relative)),
            })
            .collect()
    }
}

/// A file the walk matched.
struct Found {
    /// Workspace-relative, with `/` separators (lossy when the path is not UTF-8).
    relative: String,
    owners: Vec<Owner>,
    path: PathBuf,
    /// Whether the relative path is valid UTF-8, and so usable in a key.
    utf8: bool,
}

/// A path as the OS reports it, or canonical when that fails to resolve.
fn canonical_or_raw(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Walks the workspace for files the patterns match.
///
/// # Returns
///
/// Each match.
fn matching_files(workspace: &Workspace, patterns: &InputPatterns) -> Vec<Found> {
    let root = canonical_or_raw(workspace.root());
    let skipped = SkippedDirs::new([workspace.target_dir(), workspace.build_dir()]);
    let mut matched = vec![false; patterns.owners.len()];
    let found = WalkDir::new(&root)
        .follow_links(true)
        .into_iter()
        // `filter_entry` stops the walk from descending into the directories it rejects.
        .filter_entry(|entry| {
            if entry.file_name() == ".git" {
                return false;
            }
            if !entry.file_type().is_dir() || entry.depth() == 0 {
                return true;
            }
            let is_skipped = skipped.contains(entry);
            let could_match = entry
                .path()
                .strip_prefix(&root)
                .is_ok_and(|relative| patterns.could_contain(&slash_path(relative)));
            !is_skipped && could_match
        })
        // Dangling symlinks and loops are errors when links are followed.
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry),
            Err(error) => {
                tracing::warn!(%error, "skipping an unreadable path while looking for inputs");
                None
            }
        })
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            let relative_path = entry.path().strip_prefix(&root).ok()?;
            let relative = slash_path(relative_path);
            let globs = patterns.set.matches(&relative);
            if globs.is_empty() {
                return None;
            }
            for index in &globs {
                matched[*index] = true;
            }
            let utf8 = relative_path.to_str().is_some();
            Some(Found {
                utf8,
                owners: patterns.owners_of_globs(&globs),
                relative,
                path: entry.into_path(),
            })
        })
        .collect();
    patterns.warn_unmatched(&matched);
    found
}

/// The directories the walk must not enter: the target and build directories.
#[cfg(unix)]
struct SkippedDirs(Vec<(u64, u64)>);

#[cfg(unix)]
impl SkippedDirs {
    /// Records the identity (device and inode) of each directory that exists; one that cannot
    /// be examined does not exist, so nothing is skipped for it. Identity holds however the
    /// walk reaches the directory, even through a symlink to one of its parents.
    fn new<'a>(dirs: impl IntoIterator<Item = &'a Path>) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self(
            dirs.into_iter()
                .filter_map(|dir| dir.metadata().ok())
                .map(|metadata| (metadata.dev(), metadata.ino()))
                .collect(),
        )
    }

    /// Whether a walked directory is one of them. `walkdir` follows links here, so the
    /// metadata is the link target's.
    fn contains(&self, entry: &walkdir::DirEntry) -> bool {
        use std::os::unix::fs::MetadataExt;
        entry
            .metadata()
            .is_ok_and(|metadata| self.0.contains(&(metadata.dev(), metadata.ino())))
    }
}

/// The directories the walk must not enter: the target and build directories.
#[cfg(not(unix))]
struct SkippedDirs(Vec<PathBuf>);

#[cfg(not(unix))]
impl SkippedDirs {
    /// Records each directory's canonical path (the raw path when that fails).
    fn new<'a>(dirs: impl IntoIterator<Item = &'a Path>) -> Self {
        Self(dirs.into_iter().map(canonical_or_raw).collect())
    }

    /// Whether a walked directory is one of them. Descendants of the canonical root are
    /// canonical, except symlinks.
    fn contains(&self, entry: &walkdir::DirEntry) -> bool {
        if entry.path_is_symlink() {
            self.0.contains(&canonical_or_raw(entry.path()))
        } else {
            self.0.iter().any(|dir| dir == entry.path())
        }
    }
}

/// A relative path with `/` separators on every platform, as globs and keys spell it.
fn slash_path(path: &Path) -> String {
    path.iter()
        .map(|component| component.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    use crate::key::testing::draw_digest;

    /// The workspace has `data/**`; packages a and b each have `fixtures/**` under their own
    /// directory (which may need escaping); package c is outside the workspace root.
    fn patterns(a_dir: &str) -> InputPatterns {
        patterns_with(
            a_dir,
            &[
                "data/**".to_owned(),
                "*.txt".to_owned(),
                "**/shared.txt".to_owned(),
            ],
        )
    }

    /// Like [`patterns`], with the given workspace globs.
    fn patterns_with(a_dir: &str, workspace_globs: &[String]) -> InputPatterns {
        let fixtures = ["fixtures/**".to_owned()];
        InputPatterns::from_parts(
            workspace_globs,
            [
                PackageGlobs {
                    id: "a",
                    name: "a",
                    dir: Some(a_dir.to_owned()),
                    globs: &fixtures,
                },
                PackageGlobs {
                    id: "b",
                    name: "b",
                    dir: Some("crates/b".to_owned()),
                    globs: &fixtures,
                },
                PackageGlobs {
                    id: "c",
                    name: "c",
                    dir: None,
                    globs: &fixtures,
                },
                PackageGlobs {
                    id: "d",
                    name: "d",
                    dir: Some("crates/d".to_owned()),
                    globs: &[],
                },
            ],
        )
        .expect("valid globs")
    }

    fn draw_name(tc: &TestCase) -> String {
        tc.draw(gs::text().alphabet("xyz.").min_size(1).max_size(6))
    }

    // Property: a file is owned by every glob that matches it and by no other, package globs
    // are rooted at their package's directory even when it holds glob metacharacters, and `*`
    // does not cross `/`.
    #[hegel::test]
    fn files_are_owned_by_the_globs_that_match_them(tc: TestCase) {
        let a_dir =
            tc.draw(gs::sampled_from(&["crates/a", "we[i]rd/*a", "we\\ird", ""]).print_as_debug());
        let name = draw_name(&tc);
        let patterns = patterns(a_dir);
        let in_a = if a_dir.is_empty() {
            format!("fixtures/{name}")
        } else {
            format!("{a_dir}/fixtures/{name}")
        };

        assert_eq!(patterns.owners_of(&in_a), [Owner::Package(0)]);
        assert_eq!(
            patterns.owners_of(&format!("crates/b/fixtures/{name}")),
            [Owner::Package(1)]
        );
        assert_eq!(
            patterns.owners_of("crates/b/fixtures/shared.txt"),
            [Owner::Workspace, Owner::Package(1)]
        );
        assert_eq!(
            patterns.owners_of(&format!("data/{name}")),
            [Owner::Workspace]
        );
        assert!(
            patterns
                .owners_of(&format!("crates/d/fixtures/{name}"))
                .is_empty()
        );
        assert!(
            patterns
                .owners_of(&format!("crates/b/{name}.txt"))
                .is_empty()
        );
        assert_eq!(
            patterns.owners_of(&format!("{name}.txt")),
            [Owner::Workspace]
        );
    }

    // Property: a package's inputs are the workspace's plus its own; an unreadable one, or a
    // package outside the workspace root, makes them unavailable.
    #[hegel::test]
    fn package_inputs_are_the_workspace_and_its_own(tc: TestCase) {
        let patterns = patterns("crates/a");
        let (shared, own_a, own_b) = (draw_digest(&tc), draw_digest(&tc), draw_digest(&tc));
        let a_readable = tc.draw(gs::booleans());
        let file = |relative: &str, digest: std::result::Result<Digest, String>| InputFile {
            owners: patterns.owners_of(relative),
            relative: relative.to_owned(),
            digest,
        };
        let files = vec![
            file("data/x", Ok(shared)),
            file(
                "crates/a/fixtures/y",
                if a_readable {
                    Ok(own_a)
                } else {
                    Err("denied".into())
                },
            ),
            file("crates/b/fixtures/z", Ok(own_b)),
        ];
        let index = InputIndex { patterns, files };

        let for_a = index.for_package("a");
        let for_b = index.for_package("b").expect("b's inputs are readable");

        if a_readable {
            let expected = BTreeMap::from([
                ("crates/a/fixtures/y".to_owned(), own_a),
                ("data/x".to_owned(), shared),
            ]);
            assert_eq!(for_a, Ok(expected));
        } else {
            assert!(
                for_a
                    .expect_err("unreadable")
                    .contains("crates/a/fixtures/y")
            );
        }
        assert_eq!(
            for_b.keys().collect::<Vec<_>>(),
            ["crates/b/fixtures/z", "data/x"]
        );
        assert!(index.for_package("c").is_err());
        assert_eq!(index.for_package("d").map(|inputs| inputs.len()), Ok(1));
    }

    // Property: every directory on the way to a matched file could contain matches, and a
    // directory outside every literal prefix could not.
    #[hegel::test]
    fn walk_pruning_keeps_the_way_to_every_match(tc: TestCase) {
        let a_dir =
            tc.draw(gs::sampled_from(&["crates/a", "we[i]rd/*a", "we\\ird", ""]).print_as_debug());
        let name = draw_name(&tc);
        let patterns = patterns_with(a_dir, &["data/**".to_owned()]);
        let in_a = if a_dir.is_empty() {
            format!("fixtures/{name}")
        } else {
            format!("{a_dir}/fixtures/{name}")
        };

        for file in [
            in_a,
            format!("crates/b/fixtures/{name}"),
            format!("data/{name}"),
        ] {
            assert!(!patterns.owners_of(&file).is_empty(), "{file} is matched");
            let mut directory = String::new();
            assert!(patterns.could_contain(&directory));
            for part in file.split('/').take(file.split('/').count() - 1) {
                if !directory.is_empty() {
                    directory.push('/');
                }
                directory.push_str(part);
                assert!(
                    patterns.could_contain(&directory),
                    "{directory} leads to {file}"
                );
            }
        }
        for outside in [
            "elsewhere",
            "crates/c",
            "crates/d",
            "data2",
            &format!("{name}/data"),
        ] {
            assert!(!patterns.could_contain(outside), "{outside} leads nowhere");
        }
    }

    // Property: a glob that starts with a wildcard keeps every directory.
    #[hegel::test]
    fn a_leading_wildcard_prunes_nothing(tc: TestCase) {
        let directory = format!("{}/{}", draw_name(&tc), draw_name(&tc));
        let patterns = patterns_with("crates/a", &["**/x".to_owned()]);

        assert!(patterns.could_contain(&directory));
    }

    /// One literal component of a drawn glob.
    #[derive(Debug, Clone)]
    enum Step {
        Name(String),
        Dot,
        Parent,
        Empty,
    }

    // Property: rooting a glob normalizes its literal part as a path would be: `.` and inner
    // empty components are dropped, `..` removes the component before it (the package
    // directory's included), and climbing above the workspace root, or a leading `/`, is
    // refused. The rooted glob matches the file it names, and only from the declaring package's
    // table.
    #[hegel::test]
    fn literal_parts_are_normalized_against_the_package_dir(tc: TestCase) {
        let dir = tc.draw(gs::vecs(gs::text().alphabet("ab").min_size(1).max_size(2)).max_size(3));
        let steps: Vec<Step> = tc.draw(
            gs::vecs(hegel::one_of!(
                gs::text()
                    .alphabet("xy")
                    .min_size(1)
                    .max_size(2)
                    .map(Step::Name),
                gs::just(Step::Dot),
                gs::just(Step::Parent),
                gs::just(Step::Empty),
            ))
            .max_size(5),
        );
        let wildcard = tc.draw(gs::booleans());
        let mut pattern_parts: Vec<&str> = steps
            .iter()
            .map(|step| match step {
                Step::Name(name) => name.as_str(),
                Step::Dot => ".",
                Step::Parent => "..",
                Step::Empty => "",
            })
            .collect();
        pattern_parts.push(if wildcard { "*.txt" } else { "f.txt" });
        let pattern = pattern_parts.join("/");
        let dir_components: Vec<&str> = dir.iter().map(String::as_str).collect();
        // A leading empty component makes the glob absolute (`/...`), which is refused.
        let absolute = matches!(steps.first(), Some(Step::Empty));
        let mut expected: Option<Vec<String>> = (!absolute).then(|| dir.clone());
        for step in &steps {
            // `as_mut()` turns `&mut Option<Vec>` into `Option<&mut Vec>`; `None` stays `None`
            // once a `..` climbed out.
            if let Some(stack) = expected.as_mut() {
                match step {
                    Step::Name(name) => stack.push(name.clone()),
                    Step::Dot | Step::Empty => {}
                    Step::Parent => {
                        if stack.pop().is_none() {
                            expected = None;
                        }
                    }
                }
            }
        }

        let rooted = root_glob(&dir_components, &pattern);

        let Some(mut expected) = expected else {
            let refused = matches!(
                &rooted,
                Err(Error::InvalidGlob { reason, .. }) if reason == OUTSIDE_WORKSPACE
            );
            assert!(refused, "{pattern:?} from {dir:?}: {rooted:?}");
            return;
        };
        let rooted = rooted.expect("the glob stays inside the workspace");
        let file = expected
            .iter()
            .cloned()
            .chain(["f.txt".to_owned()])
            .collect::<Vec<_>>();
        if !wildcard {
            expected.push("f.txt".to_owned());
        }
        assert_eq!(rooted.prefix, expected, "{pattern:?} from {dir:?}");
        let globs = [pattern.clone()];
        let patterns = InputPatterns::from_parts(
            &[],
            [PackageGlobs {
                id: "a",
                name: "a",
                dir: Some(dir.join("/")),
                globs: &globs,
            }],
        )
        .expect("valid globs");
        assert_eq!(patterns.owners_of(&file.join("/")), [Owner::Package(0)]);
    }

    // Property: an absolute glob, and `..` after a wildcard, are refused in either table.
    #[hegel::test]
    fn globs_that_can_never_match_are_refused(tc: TestCase) {
        let tail = tc.draw(gs::text().alphabet("xy").min_size(1).max_size(3));
        let (pattern, reason) = tc.draw(gs::sampled_from(vec![
            (format!("/{tail}/**"), OUTSIDE_WORKSPACE),
            (format!("/{tail}"), OUTSIDE_WORKSPACE),
            (format!("{tail}/*/../x"), PARENT_AFTER_WILDCARD),
            (format!("**/../{tail}"), PARENT_AFTER_WILDCARD),
        ]));
        let in_package = tc.draw(gs::booleans());
        let globs = [pattern.clone()];
        let (workspace_globs, package_globs): (&[String], &[String]) = if in_package {
            (&[], &globs)
        } else {
            (&globs, &[])
        };

        let compiled = InputPatterns::from_parts(
            workspace_globs,
            [PackageGlobs {
                id: "a",
                name: "a",
                dir: Some("crates/a".to_owned()),
                globs: package_globs,
            }],
        );

        let refused = matches!(
            &compiled,
            Err(Error::InvalidGlob { pattern: at, reason: why }) if *at == pattern && why == reason
        );
        assert!(refused, "{compiled:?}");
    }
}
