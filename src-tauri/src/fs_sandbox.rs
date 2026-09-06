//! Sandbox filesystem module.
//!
//! Every filesystem operation CodeForge performs on user project files must
//! funnel through this module. It guarantees that no matter what path a
//! caller (frontend JS, a script, a bug) requests, the resolved path can
//! never land outside the configured sandbox root.
//!
//! Strategy: canonicalize the requested path AND the sandbox root, then
//! verify the resolved path starts with the resolved root. Canonicalization
//! resolves `..`, `.`, and symlinks, so this catches traversal attempts that
//! naive string checks would miss (e.g. symlink pointing outside sandbox).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("path escapes sandbox root: {0}")]
    PathEscape(PathBuf),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("path does not exist yet, cannot canonicalize: {0}")]
    NotYetCreated(PathBuf),
}

pub type SandboxResult<T> = Result<T, SandboxError>;

/// Represents a validated sandbox root (e.g. `.../codeforge/sandbox`).
#[derive(Debug, Clone)]
pub struct SandboxRoot {
    canonical_root: PathBuf,
}

impl SandboxRoot {
    /// Create a new sandbox root, creating the directory if it doesn't exist.
    pub fn new(root: impl AsRef<Path>) -> SandboxResult<Self> {
        let root = root.as_ref();
        if !root.exists() {
            fs::create_dir_all(root)?;
        }
        let canonical_root = fs::canonicalize(root)?;
        Ok(Self { canonical_root })
    }

    pub fn root_path(&self) -> &Path {
        &self.canonical_root
    }

    /// Resolve a requested path (relative or absolute) against this sandbox
    /// root and verify it does not escape. This is the ONLY function that
    /// should be trusted to produce a "safe" path for further fs operations.
    ///
    /// `requested` may be:
    ///   - a relative path like "projectname/src/index.js"
    ///   - an absolute path that a caller claims is already inside the sandbox
    ///
    /// Either way, we join it against the canonical root (if relative) and
    /// then re-canonicalize + verify containment. For paths that don't exist
    /// yet (e.g. creating a new file), we canonicalize the parent directory
    /// instead and reattach the final component.
    pub fn resolve(&self, requested: impl AsRef<Path>) -> SandboxResult<PathBuf> {
        let requested = requested.as_ref();

        let candidate = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.canonical_root.join(requested)
        };

        self.verify_contained(&candidate)
    }

    /// Verify an already-constructed absolute path is contained within the
    /// sandbox root, handling the not-yet-existing case (needed for create
    /// operations where the target file doesn't exist yet to canonicalize).
    fn verify_contained(&self, candidate: &Path) -> SandboxResult<PathBuf> {
        if candidate.exists() {
            let canonical = fs::canonicalize(candidate)?;
            if canonical.starts_with(&self.canonical_root) {
                Ok(canonical)
            } else {
                Err(SandboxError::PathEscape(canonical))
            }
        } else {
            // Walk up to find the nearest existing ancestor, canonicalize
            // that, then reattach the non-existent tail. This lets "create
            // new file at path X" be validated before X exists.
            let mut existing_ancestor = candidate.parent();
            let mut tail_components: Vec<std::ffi::OsString> = vec![candidate
                .file_name()
                .ok_or_else(|| SandboxError::NotYetCreated(candidate.to_path_buf()))?
                .to_os_string()];

            loop {
                match existing_ancestor {
                    None => return Err(SandboxError::NotYetCreated(candidate.to_path_buf())),
                    Some(anc) if anc.exists() => {
                        let canonical_anc = fs::canonicalize(anc)?;
                        if !canonical_anc.starts_with(&self.canonical_root) {
                            return Err(SandboxError::PathEscape(canonical_anc));
                        }
                        let mut result = canonical_anc;
                        for component in tail_components.iter().rev() {
                            result.push(component);
                        }
                        // Final safety check on the fully reconstructed path.
                        if !result.starts_with(&self.canonical_root) {
                            return Err(SandboxError::PathEscape(result));
                        }
                        return Ok(result);
                    }
                    Some(anc) => {
                        tail_components.push(
                            anc.file_name()
                                .ok_or_else(|| {
                                    SandboxError::NotYetCreated(candidate.to_path_buf())
                                })?
                                .to_os_string(),
                        );
                        existing_ancestor = anc.parent();
                    }
                }
            }
        }
    }

    pub fn read_file(&self, requested: impl AsRef<Path>) -> SandboxResult<Vec<u8>> {
        let safe = self.resolve(requested)?;
        Ok(fs::read(safe)?)
    }

    pub fn write_file(&self, requested: impl AsRef<Path>, contents: &[u8]) -> SandboxResult<()> {
        let safe = self.resolve(requested)?;
        if let Some(parent) = safe.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(safe, contents)?;
        Ok(())
    }

    pub fn create_dir(&self, requested: impl AsRef<Path>) -> SandboxResult<()> {
        let safe = self.resolve(requested)?;
        fs::create_dir_all(safe)?;
        Ok(())
    }

    pub fn delete(&self, requested: impl AsRef<Path>) -> SandboxResult<()> {
        let safe = self.resolve(requested)?;
        if safe.is_dir() {
            fs::remove_dir_all(safe)?;
        } else {
            fs::remove_file(safe)?;
        }
        Ok(())
    }

    pub fn rename(
        &self,
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
    ) -> SandboxResult<()> {
        let safe_from = self.resolve(from)?;
        let safe_to = self.resolve(to)?;
        fs::rename(safe_from, safe_to)?;
        Ok(())
    }

    /// Recursively list all files under a project, returning paths relative
    /// to the sandbox root (e.g. "projectname/src/index.js"). This is what
    /// feeds both the file tree UI and the upload tree-builder.
    pub fn list_files_recursive(
        &self,
        requested: impl AsRef<Path>,
    ) -> SandboxResult<Vec<PathBuf>> {
        let safe_root = self.resolve(requested)?;
        let mut results = Vec::new();
        self.walk(&safe_root, &mut results)?;
        Ok(results
            .into_iter()
            .map(|p| {
                p.strip_prefix(&self.canonical_root)
                    .unwrap_or(&p)
                    .to_path_buf()
            })
            .collect())
    }

    fn walk(&self, dir: &Path, results: &mut Vec<PathBuf>) -> SandboxResult<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                self.walk(&path, results)?;
            } else {
                results.push(path);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn temp_sandbox(name: &str) -> SandboxRoot {
        let mut dir = env::temp_dir();
        dir.push(format!("codeforge_test_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        SandboxRoot::new(dir).unwrap()
    }

    #[test]
    fn allows_normal_relative_path() {
        let sb = temp_sandbox("normal");
        sb.write_file("projectA/src/index.js", b"console.log(1)").unwrap();
        let content = sb.read_file("projectA/src/index.js").unwrap();
        assert_eq!(content, b"console.log(1)");
    }

    #[test]
    fn blocks_simple_traversal() {
        let sb = temp_sandbox("traversal");
        sb.write_file("projectA/file.txt", b"hi").unwrap();
        let result = sb.resolve("projectA/../../../../etc/passwd");
        assert!(matches!(result, Err(SandboxError::PathEscape(_))));
    }

    #[test]
    fn blocks_absolute_escape() {
        let sb = temp_sandbox("abs_escape");
        let result = sb.resolve("/etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn blocks_traversal_via_dotdot_in_new_file_path() {
        let sb = temp_sandbox("new_file_traversal");
        // File doesn't exist yet, but contains a `..` that would escape.
        let result = sb.write_file("projectA/../../evil.txt", b"pwned");
        assert!(result.is_err());
    }

    #[test]
    fn allows_creating_new_nested_file_that_does_not_escape() {
        let sb = temp_sandbox("new_nested_ok");
        let result = sb.write_file("projectA/deep/nested/newfile.txt", b"ok");
        assert!(result.is_ok());
        let content = sb.read_file("projectA/deep/nested/newfile.txt").unwrap();
        assert_eq!(content, b"ok");
    }

    #[test]
    fn list_files_preserves_relative_structure() {
        let sb = temp_sandbox("listing");
        sb.write_file("myproj/index.js", b"a").unwrap();
        sb.write_file("myproj/src/utils/helpers.ts", b"b").unwrap();
        let mut files = sb.list_files_recursive("myproj").unwrap();
        files.sort();
        let strs: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert!(strs.contains(&"myproj/index.js".to_string()));
        assert!(strs.contains(&"myproj/src/utils/helpers.ts".to_string()));
    }

    #[test]
    fn rename_blocked_if_destination_escapes() {
        let sb = temp_sandbox("rename_escape");
        sb.write_file("proj/a.txt", b"x").unwrap();
        let result = sb.rename("proj/a.txt", "../../outside.txt");
        assert!(result.is_err());
    }
}
