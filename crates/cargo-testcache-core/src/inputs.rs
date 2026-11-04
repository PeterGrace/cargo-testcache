//! Files tests read at runtime: matched by the configured `inputs` globs, and hashed.
//!
//! The workspace is walked once. Each file is matched against one compiled set holding the
//! workspace's globs (relative to its root) and every package's (relative to that package's
//! directory, rewritten to be relative to the root), and the matching files are hashed in
//! parallel. The target and build directories and `.git` are not walked.

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
        for pattern in workspace_globs {
            builder.add(glob(pattern)?);
            owners.push(Owner::Workspace);
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
            for pattern in package.globs {
                // `escape` makes a directory such as `we[i]rd` match itself literally.
                let rooted = if dir.is_empty() {
                    pattern.clone()
                } else {
                    format!("{}/{pattern}", globset::escape(&dir))
                };
                builder.add(glob(&rooted)?);
                owners.push(Owner::Package(index));
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
            .map(|(relative, owners, path)| InputFile {
                digest: Digest::of_file(&path).map_err(|error| error.to_string()),
                relative,
                owners,
            })
            .collect();
        Ok(Self { patterns, files })
    }

    /// A package's inputs: the files the workspace's globs or its own match.
    ///
    /// # Arguments
    ///
    /// * `package_id` - The package.
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

/// Walks the workspace for files the patterns match.
///
/// # Returns
///
/// Each match's workspace-relative name, owners and path.
fn matching_files(
    workspace: &Workspace,
    patterns: &InputPatterns,
) -> Vec<(String, Vec<Owner>, PathBuf)> {
    let skipped = [workspace.target_dir(), workspace.build_dir()];
    WalkDir::new(workspace.root())
        .into_iter()
        // `filter_entry` stops the walk from descending into the directories it rejects.
        .filter_entry(|entry| {
            !(entry.file_type().is_dir()
                && (entry.file_name() == ".git" || skipped.contains(&entry.path())))
        })
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry),
            Err(error) => {
                tracing::warn!(%error, "skipping an unreadable path while looking for inputs");
                None
            }
        })
        .filter(|entry| {
            entry.file_type().is_file() || (entry.path_is_symlink() && entry.path().is_file())
        })
        .filter_map(|entry| {
            let relative = slash_path(entry.path().strip_prefix(workspace.root()).ok()?);
            let owners = patterns.owners_of(&relative);
            (!owners.is_empty()).then(|| (relative, owners, entry.into_path()))
        })
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
        let fixtures = ["fixtures/**".to_owned()];
        InputPatterns::from_parts(
            &["data/**".to_owned(), "*.txt".to_owned()],
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
        let a_dir = tc.draw(gs::sampled_from(&["crates/a", "we[i]rd/*a", ""]).print_as_debug());
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
}
