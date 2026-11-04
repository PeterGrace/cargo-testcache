//! Files tests read at runtime: matched by the configured `inputs` globs, and hashed.
//!
//! The workspace is walked once. Each file is matched against one compiled set holding the
//! workspace's globs (relative to its root) and every package's (relative to that package's
//! directory, rewritten to be relative to the root), and the matching files are hashed in
//! parallel. The target and build directories and `.git` are not walked, nor is any directory
//! that no glob's literal prefix leads to. Symlinked files and directories are followed, as a
//! test reading them would follow them; directories are compared to the target and build
//! directories by canonical path, because Cargo does not normalize the paths it reports.

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

/// One package's `inputs`, for [`InputPatterns::from_parts`].
#[derive(Debug, Clone)]
pub struct PackageGlobs<'a> {
    /// The package id.
    pub id: &'a str,
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
    /// [`Error::InvalidGlob`] when a glob does not compile.
    pub fn from_parts<'a>(
        workspace_globs: &[String],
        packages: impl IntoIterator<Item = PackageGlobs<'a>>,
    ) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        let mut owners = Vec::new();
        let mut package_index = HashMap::new();
        let mut outside = HashSet::new();
        let mut prefixes = Vec::new();
        for pattern in workspace_globs {
            builder.add(glob(pattern)?);
            owners.push(Owner::Workspace);
            prefixes.push(literal_prefix(&[], pattern));
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
            // `escape` makes a directory such as `we[i]rd` match itself literally; it leaves
            // `\` alone, which our globs treat as an escape, so that is wrapped in a class.
            let escaped_dir = globset::escape(&dir).replace('\\', "[\\]");
            for pattern in package.globs {
                let rooted = if dir.is_empty() {
                    pattern.clone()
                } else {
                    format!("{escaped_dir}/{pattern}")
                };
                builder.add(glob(&rooted)?);
                owners.push(Owner::Package(index));
                prefixes.push(literal_prefix(&dir_components, pattern));
            }
        }
        let set = builder.build().map_err(|source| Error::InvalidGlob {
            pattern: "inputs".to_owned(),
            source,
        })?;
        Ok(Self {
            set,
            owners,
            package_index,
            outside,
            prefixes,
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
    fn owners_of(&self, relative: &str) -> Vec<Owner> {
        let mut owners: Vec<Owner> = self
            .set
            .matches(relative)
            .into_iter()
            .map(|index| self.owners[index])
            .collect();
        owners.sort_unstable();
        owners.dedup();
        owners
    }
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
    let skipped = [
        canonical_or_raw(workspace.target_dir()),
        canonical_or_raw(workspace.build_dir()),
    ];
    WalkDir::new(&root)
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
            // Descendants of the canonical root are canonical, except symlinks.
            let is_skipped = if entry.path_is_symlink() {
                skipped.contains(&canonical_or_raw(entry.path()))
            } else {
                skipped.iter().any(|dir| dir == entry.path())
            };
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
            let owners = patterns.owners_of(&relative);
            if owners.is_empty() {
                return None;
            }
            let utf8 = relative_path.to_str().is_some();
            Some(Found {
                utf8,
                relative,
                owners,
                path: entry.into_path(),
            })
        })
        .collect()
}

/// The directory components before a glob's first wildcard, after its package directory.
fn literal_prefix(dir_components: &[&str], pattern: &str) -> Vec<String> {
    dir_components
        .iter()
        .copied()
        .chain(
            pattern
                .split('/')
                .take_while(|part| !part.is_empty() && !part.contains(['*', '?', '[', '{', '\\'])),
        )
        .map(str::to_owned)
        .collect()
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
                    dir: Some(a_dir.to_owned()),
                    globs: &fixtures,
                },
                PackageGlobs {
                    id: "b",
                    dir: Some("crates/b".to_owned()),
                    globs: &fixtures,
                },
                PackageGlobs {
                    id: "c",
                    dir: None,
                    globs: &fixtures,
                },
                PackageGlobs {
                    id: "d",
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
}
