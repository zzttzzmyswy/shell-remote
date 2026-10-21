use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use std::fs;
use std::path::{Path, PathBuf};

use crate::proto::{FileEntry, FsResultPayload};

/// Strips leading path separators (`/` and `\`) so a user-supplied path can
/// be joined onto the file-manager root on both unix and Windows.
fn strip_leading_seps(s: &str) -> &str {
    s.trim_start_matches(['/', '\\'])
}

pub fn resolve_path(root: &Path, user_path: &str) -> Option<PathBuf> {
    let root = match root.canonicalize() {
        Ok(r) => r,
        Err(_) => return None,
    };

    let user_path_buf = PathBuf::from(user_path);
    if user_path_buf.is_absolute() {
        if let Ok(canon) = user_path_buf.canonicalize() {
            return Some(canon);
        }
        // File doesn't exist yet (e.g., pending write) — check parent directory
        if let Some(parent) = user_path_buf.parent() {
            if parent.canonicalize().is_ok() {
                return Some(user_path_buf);
            }
        }
        return None;
    }

    let combined = root.join(strip_leading_seps(user_path));
    let resolved = match combined.canonicalize() {
        Ok(r) => r,
        Err(_) => match combined.parent().and_then(|p| p.canonicalize().ok()) {
            Some(_parent) => combined,
            _ => return None,
        },
    };
    Some(resolved)
}

/// Resolve `user_path` *without* following a symlink in its last component.
///
/// Only the parent directory is canonicalized; the final name is then appended
/// verbatim. `resolve_path` canonicalizes the whole path, which turns a symlink
/// into its target — deleting a link to a directory would then
/// `remove_dir_all` the real directory. Destructive operations (delete/rename)
/// must act on the link itself, so they use this variant instead.
///
/// Returns `None` when the path has no usable final component (empty, `.`,
/// `..`) or when the parent directory does not exist.
pub fn resolve_path_nofollow(root: &Path, user_path: &str) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let raw = PathBuf::from(user_path);
    let name = raw.file_name()?;
    if name.is_empty() {
        return None;
    }
    let name_str = name.to_string_lossy();
    if name_str == "." || name_str == ".." {
        return None;
    }

    let dir = if raw.is_absolute() {
        PathBuf::from(raw.parent()?)
    } else {
        let parent = raw
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        root.join(strip_leading_seps(&parent))
    };
    // Parent must resolve (it may itself be a symlink — that is fine, we only
    // refuse to resolve the *last* component).
    let dir = dir.canonicalize().ok()?;
    Some(dir.join(name))
}

/// Resolve a path for a destructive operation (delete / rename).
///
/// Uses [`resolve_path_nofollow`] so a symlink's last component is never
/// followed. `resolve_path_nofollow` refuses paths without a real final name
/// (`.`, `..`, trailing separator); a symlink always has one, so falling back
/// to the historical whole-path resolution for those is safe and keeps
/// `..`-relative operations behaving exactly as before.
fn resolve_mutation_target(root: &Path, user_path: &str) -> Option<PathBuf> {
    resolve_path_nofollow(root, user_path).or_else(|| resolve_path(root, user_path))
}

pub fn list_dir(root: &Path, user_path: &str) -> FsResultPayload {
    let path = match resolve_path(root, user_path) {
        Some(p) => p,
        None => {
            return FsResultPayload {
                kind: None,
                success: false,
                error: Some("Invalid path".to_string()),
                entries: None,
                content: None,
                path: Some(user_path.to_string()),
                new_path: None,
            }
        }
    };

    match fs::read_dir(&path) {
        Ok(entries) => {
            let mut file_entries: Vec<FileEntry> = Vec::new();
            for entry in entries.flatten() {
                let entry_path = entry.path();
                // symlink_metadata: never follow the entry itself, so a link to
                // a directory is reported as a link (and its type/size describe
                // the link, not the target's contents).
                let metadata = match fs::symlink_metadata(&entry_path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let is_symlink = metadata.file_type().is_symlink();
                // A link's own metadata says "file"; resolve the target so the
                // frontend can still navigate into linked directories.
                let target_meta = if is_symlink {
                    fs::metadata(&entry_path).ok()
                } else {
                    None
                };
                let effective = target_meta.as_ref().unwrap_or(&metadata);

                let entry_type = if effective.is_dir() {
                    "directory"
                } else {
                    "file"
                };

                let mode = format_mode(&metadata);
                let owner = get_owner(&metadata);

                file_entries.push(FileEntry {
                    name: entry.file_name().to_string_lossy().to_string(),
                    path: entry_path.to_string_lossy().to_string(),
                    entry_type: entry_type.to_string(),
                    size: effective.len(),
                    mode,
                    owner,
                    is_symlink,
                });
            }

            file_entries.sort_by(|a, b| {
                if a.entry_type != b.entry_type {
                    a.entry_type.cmp(&b.entry_type)
                } else {
                    a.name.cmp(&b.name)
                }
            });

            FsResultPayload {
                kind: None,
                success: true,
                error: None,
                entries: Some(file_entries),
                content: None,
                path: Some(path.to_string_lossy().to_string()),
                new_path: None,
            }
        }
        Err(e) => FsResultPayload {
            kind: None,
            success: false,
            error: Some(format!("Failed to list directory: {}", e)),
            entries: None,
            content: None,
            path: Some(user_path.to_string()),
            new_path: None,
        },
    }
}

#[allow(dead_code)]
pub fn read_file(root: &Path, user_path: &str) -> FsResultPayload {
    let path = match resolve_path(root, user_path) {
        Some(p) => p,
        None => {
            return FsResultPayload {
                kind: None,
                success: false,
                error: Some("Invalid path".to_string()),
                entries: None,
                content: None,
                path: Some(user_path.to_string()),
                new_path: None,
            }
        }
    };

    if !path.is_file() {
        return FsResultPayload {
            kind: None,
            success: false,
            error: Some("Path is not a file".to_string()),
            entries: None,
            content: None,
            path: Some(user_path.to_string()),
            new_path: None,
        };
    }

    match fs::read(&path) {
        Ok(data) => {
            let content = encode_b64(&data);
            FsResultPayload {
                kind: None,
                success: true,
                error: None,
                entries: None,
                content: Some(content),
                path: Some(user_path.to_string()),
                new_path: None,
            }
        }
        Err(e) => FsResultPayload {
            kind: None,
            success: false,
            error: Some(format!("Failed to read file: {}", e)),
            entries: None,
            content: None,
            path: Some(user_path.to_string()),
            new_path: None,
        },
    }
}

pub fn write_file(root: &Path, user_path: &str, content_b64: &str) -> FsResultPayload {
    let path = match resolve_path(root, user_path) {
        Some(p) => p,
        None => {
            return FsResultPayload {
                kind: None,
                success: false,
                error: Some("Invalid path".to_string()),
                entries: None,
                content: None,
                path: Some(user_path.to_string()),
                new_path: None,
            }
        }
    };

    let content = match decode_b64(content_b64) {
        Some(data) => data,
        None => {
            return FsResultPayload {
                kind: None,
                success: false,
                error: Some("Invalid base64 content".to_string()),
                entries: None,
                content: None,
                path: Some(user_path.to_string()),
                new_path: None,
            }
        }
    };

    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return FsResultPayload {
                kind: None,
                success: false,
                error: Some(format!("Failed to create parent directory: {}", e)),
                entries: None,
                content: None,
                path: Some(user_path.to_string()),
                new_path: None,
            };
        }
    }

    match fs::write(&path, &content) {
        Ok(()) => FsResultPayload {
            kind: None,
            success: true,
            error: None,
            entries: None,
            content: None,
            path: Some(user_path.to_string()),
            new_path: None,
        },
        Err(e) => FsResultPayload {
            kind: None,
            success: false,
            error: Some(format!("Failed to write file: {}", e)),
            entries: None,
            content: None,
            path: Some(user_path.to_string()),
            new_path: None,
        },
    }
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn write_file_bytes(root: &Path, user_path: &str, data: &[u8]) -> FsResultPayload {
    let path = match resolve_path(root, user_path) {
        Some(p) => p,
        None => {
            return FsResultPayload {
                kind: None,
                success: false,
                error: Some("Invalid path".into()),
                entries: None,
                content: None,
                path: Some(user_path.to_string()),
                new_path: None,
            }
        }
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::write(&path, data) {
        Ok(()) => FsResultPayload {
            kind: None,
            success: true,
            error: None,
            entries: None,
            content: None,
            path: Some(user_path.to_string()),
            new_path: None,
        },
        Err(e) => FsResultPayload {
            kind: None,
            success: false,
            error: Some(format!("Failed to write file: {}", e)),
            entries: None,
            content: None,
            path: Some(user_path.to_string()),
            new_path: None,
        },
    }
}

pub fn delete_path(root: &Path, user_path: &str) -> FsResultPayload {
    // nofollow: deleting a symlink must remove the link, never its target.
    let path = match resolve_mutation_target(root, user_path) {
        Some(p) => p,
        None => {
            return FsResultPayload {
                kind: None,
                success: false,
                error: Some("Invalid path".to_string()),
                entries: None,
                content: None,
                path: Some(user_path.to_string()),
                new_path: None,
            }
        }
    };

    let root_canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if path == root_canonical || path == root {
        return FsResultPayload {
            kind: None,
            success: false,
            error: Some("Cannot delete root directory".to_string()),
            entries: None,
            content: None,
            path: Some(user_path.to_string()),
            new_path: None,
        };
    }

    // symlink_metadata: decide on the link itself, not on what it points to.
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) => {
            let kind = if e.kind() == std::io::ErrorKind::NotFound {
                "not_found"
            } else {
                "other"
            };
            return FsResultPayload {
                kind: Some(kind.to_string()),
                success: false,
                error: Some(format!("Failed to delete: {}", e)),
                entries: None,
                content: None,
                path: Some(user_path.to_string()),
                new_path: None,
            };
        }
    };

    let result = if meta.file_type().is_symlink() {
        // Unix removes any link with remove_file; Windows needs remove_dir for
        // directory links/junctions and remove_file for file links.
        #[cfg(windows)]
        {
            let target_is_dir = fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
            if target_is_dir {
                fs::remove_dir(&path)
            } else {
                fs::remove_file(&path)
            }
        }
        #[cfg(not(windows))]
        {
            fs::remove_file(&path)
        }
    } else if meta.is_dir() {
        fs::remove_dir_all(&path)
    } else {
        fs::remove_file(&path)
    };

    match result {
        Ok(()) => FsResultPayload {
            kind: None,
            success: true,
            error: None,
            entries: None,
            content: None,
            path: Some(user_path.to_string()),
            new_path: None,
        },
        Err(e) => FsResultPayload {
            kind: None,
            success: false,
            error: Some(format!("Failed to delete: {}", e)),
            entries: None,
            content: None,
            path: Some(user_path.to_string()),
            new_path: None,
        },
    }
}

pub fn rename_path(root: &Path, from_path: &str, to_path: &str) -> FsResultPayload {
    // nofollow on both ends: renaming a symlink must move the link itself, not
    // the file/directory it points at.
    let from = match resolve_mutation_target(root, from_path) {
        Some(p) => p,
        None => {
            return FsResultPayload {
                kind: None,
                success: false,
                error: Some("Source path resolution failed".to_string()),
                entries: None,
                content: None,
                path: Some(from_path.to_string()),
                new_path: Some(to_path.to_string()),
            }
        }
    };

    let to = match resolve_mutation_target(root, to_path) {
        Some(p) => p,
        None => {
            return FsResultPayload {
                kind: None,
                success: false,
                error: Some("Destination path resolution failed".to_string()),
                entries: None,
                content: None,
                path: Some(from_path.to_string()),
                new_path: Some(to_path.to_string()),
            }
        }
    };

    match fs::rename(&from, &to) {
        Ok(()) => FsResultPayload {
            kind: None,
            success: true,
            error: None,
            entries: None,
            content: None,
            path: Some(from_path.to_string()),
            new_path: Some(to_path.to_string()),
        },
        Err(e) => FsResultPayload {
            kind: None,
            success: false,
            error: Some(format!("Failed to rename: {}", e)),
            entries: None,
            content: None,
            path: Some(from_path.to_string()),
            new_path: Some(to_path.to_string()),
        },
    }
}

pub fn encode_b64(data: &[u8]) -> String {
    BASE64.encode(data)
}

pub fn decode_b64(encoded: &str) -> Option<Vec<u8>> {
    BASE64.decode(encoded).ok()
}

#[cfg(unix)]
fn get_owner(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}:{}", metadata.uid(), metadata.gid())
}

#[cfg(not(unix))]
fn get_owner(_metadata: &std::fs::Metadata) -> String {
    "0:0".to_string()
}

pub fn create_dir(root: &Path, user_path: &str) -> FsResultPayload {
    let path = match resolve_path(root, user_path) {
        Some(p) => p,
        None => {
            return FsResultPayload {
                kind: None,
                success: false,
                error: Some("Invalid path".into()),
                entries: None,
                content: None,
                path: Some(user_path.to_string()),
                new_path: None,
            }
        }
    };
    match fs::create_dir_all(&path) {
        Ok(()) => FsResultPayload {
            kind: None,
            success: true,
            error: None,
            entries: None,
            content: None,
            path: Some(user_path.to_string()),
            new_path: None,
        },
        Err(e) => FsResultPayload {
            kind: None,
            success: false,
            error: Some(format!("{}", e)),
            entries: None,
            content: None,
            path: Some(user_path.to_string()),
            new_path: None,
        },
    }
}

#[cfg(unix)]
fn format_mode(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode();
    let file_type = if metadata.is_dir() {
        'd'
    } else if metadata.is_symlink() {
        'l'
    } else {
        '-'
    };

    let owner_read = if mode & 0o400 != 0 { 'r' } else { '-' };
    let owner_write = if mode & 0o200 != 0 { 'w' } else { '-' };
    let owner_exec = if mode & 0o100 != 0 { 'x' } else { '-' };
    let group_read = if mode & 0o040 != 0 { 'r' } else { '-' };
    let group_write = if mode & 0o020 != 0 { 'w' } else { '-' };
    let group_exec = if mode & 0o010 != 0 { 'x' } else { '-' };
    let other_read = if mode & 0o004 != 0 { 'r' } else { '-' };
    let other_write = if mode & 0o002 != 0 { 'w' } else { '-' };
    let other_exec = if mode & 0o001 != 0 { 'x' } else { '-' };

    format!(
        "{}{}{}{}{}{}{}{}{}{}",
        file_type,
        owner_read,
        owner_write,
        owner_exec,
        group_read,
        group_write,
        group_exec,
        other_read,
        other_write,
        other_exec
    )
}

#[cfg(not(unix))]
fn format_mode(metadata: &std::fs::Metadata) -> String {
    if metadata.is_dir() {
        "d---------".to_string()
    } else {
        "----------".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_resolve_path_valid() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        std::fs::create_dir_all(root.join("subdir")).unwrap();
        std::fs::write(root.join("test.txt"), b"hello").unwrap();

        let resolved = resolve_path(root, "test.txt");
        assert!(resolved.is_some());

        let resolved = resolve_path(root, "/subdir");
        assert!(resolved.is_some());
    }

    #[test]
    fn test_resolve_path_traversal_resolves() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("root_dir");
        std::fs::create_dir_all(&root).unwrap();

        // "../outside" resolves to dir.path(); canonicalize resolves '..' correctly
        let resolved = resolve_path(&root, "../outside");
        assert!(resolved.is_some());
    }

    #[test]
    fn test_resolve_path_absolute_resolves() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("root_dir");
        std::fs::create_dir_all(&root).unwrap();

        // Absolute paths resolve normally
        let resolved = resolve_path(&root, "/etc/passwd");
        assert!(resolved.is_some());
    }

    #[test]
    fn test_list_dir() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        std::fs::write(root.join("a.txt"), b"content").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/b.txt"), b"nested").unwrap();

        let result = list_dir(root, ".");
        assert!(result.success);
        let entries = result.entries.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "sub");
        assert_eq!(entries[0].entry_type, "directory");
        assert_eq!(entries[1].name, "a.txt");
        assert_eq!(entries[1].entry_type, "file");
    }

    #[test]
    fn test_list_dir_parent_dir() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("root_dir");
        std::fs::create_dir_all(&root).unwrap();

        // Listing parent directory is allowed
        let result = list_dir(&root, "../");
        assert!(result.success);
    }

    #[test]
    fn test_read_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let content = "Hello, World!\nThis is a test.";
        std::fs::write(root.join("test.txt"), content).unwrap();

        let result = read_file(root, "test.txt");
        assert!(result.success);
        let decoded = decode_b64(&result.content.unwrap()).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), content);
    }

    #[test]
    fn test_read_file_not_found() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        let result = read_file(root, "nonexistent.txt");
        assert!(!result.success);
    }

    #[test]
    fn test_write_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let content_b64 = encode_b64(b"new file content");

        let result = write_file(root, "output.txt", &content_b64);
        assert!(result.success);

        let read_back = std::fs::read_to_string(root.join("output.txt")).unwrap();
        assert_eq!(read_back, "new file content");
    }

    #[test]
    fn test_write_file_parent_dir() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("root_dir");
        std::fs::create_dir_all(&root).unwrap();
        let content_b64 = encode_b64(b"escape");

        // Writing to parent directory is allowed
        let result = write_file(&root, "../outside.txt", &content_b64);
        assert!(result.success);
    }

    #[test]
    fn test_write_file_invalid_b64() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        let result = write_file(root, "test.txt", "!!!invalid!!!");
        assert!(!result.success);
    }

    #[test]
    fn test_delete_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("delete_me.txt"), b"delete").unwrap();

        let result = delete_path(root, "delete_me.txt");
        assert!(result.success);
        assert!(!root.join("delete_me.txt").exists());
    }

    #[test]
    fn test_delete_directory() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("delete_me")).unwrap();
        std::fs::write(root.join("delete_me/file.txt"), b"x").unwrap();

        let result = delete_path(root, "delete_me");
        assert!(result.success);
        assert!(!root.join("delete_me").exists());
    }

    #[test]
    fn test_delete_parent_dir() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("root_dir");
        std::fs::create_dir_all(&root).unwrap();

        // Deleting parent directory contents is allowed
        let result = delete_path(&root, "../");
        assert!(result.success);
    }

    #[test]
    fn test_delete_root_rejected() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        let result = delete_path(root, "");
        assert!(!result.success);
        assert!(root.exists());
    }

    #[test]
    fn test_rename() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("old.txt"), b"content").unwrap();

        let result = rename_path(root, "old.txt", "new.txt");
        assert!(result.success);
        assert!(!root.join("old.txt").exists());
        assert!(root.join("new.txt").exists());
    }

    #[test]
    fn test_rename_parent_dir() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("root_dir");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("file.txt"), b"x").unwrap();

        // Renaming to parent directory is allowed
        let result = rename_path(&root, "file.txt", "../outside.txt");
        assert!(result.success);
    }

    #[cfg(unix)]
    #[test]
    fn test_delete_symlink_keeps_target_directory() {
        // C1：删除指向目录的符号链接必须只删链接本身。旧实现走 resolve_path
        // canonicalize 最后一个分量，会把 link 解析成 dir，从而 remove_dir_all
        // 掉真实目录。
        use std::os::unix::fs::symlink;
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("dir")).unwrap();
        std::fs::write(root.join("dir/keep.txt"), b"keep").unwrap();
        symlink(root.join("dir"), root.join("link")).unwrap();

        let result = delete_path(root, "link");
        assert!(result.success, "delete link failed: {:?}", result.error);

        assert!(!root.join("link").exists(), "link itself must be removed");
        assert!(root.join("dir").is_dir(), "target directory must survive");
        assert_eq!(
            std::fs::read_to_string(root.join("dir/keep.txt")).unwrap(),
            "keep",
            "target directory contents must survive"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_rename_symlink_moves_link_not_target() {
        // C1：重命名链接移动的是链接本身，目标文件/目录原地不动。
        use std::os::unix::fs::symlink;
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("dir")).unwrap();
        std::fs::write(root.join("dir/keep.txt"), b"keep").unwrap();
        symlink(root.join("dir"), root.join("link")).unwrap();

        let result = rename_path(root, "link", "link2");
        assert!(result.success, "rename failed: {:?}", result.error);

        assert!(!root.join("link").exists(), "old link name must be gone");
        let moved = root.join("link2");
        assert!(
            std::fs::symlink_metadata(&moved)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false),
            "link2 must still be a symlink"
        );
        assert!(
            root.join("dir").is_dir(),
            "target directory must stay at its original path"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("dir/keep.txt")).unwrap(),
            "keep"
        );
        // And it still points at the original target.
        let target = std::fs::read_link(&moved).unwrap();
        assert_eq!(target, root.join("dir").canonicalize().unwrap());
    }

    #[test]
    fn test_resolve_path_nofollow_rejects_dot_and_dotdot() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        assert!(resolve_path_nofollow(root, "").is_none());
        assert!(resolve_path_nofollow(root, ".").is_none());
        assert!(resolve_path_nofollow(root, "..").is_none());
        assert!(resolve_path_nofollow(root, "sub/..").is_none());
        assert!(resolve_path_nofollow(root, "a.txt").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn test_list_dir_reports_is_symlink() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("dir")).unwrap();
        std::fs::write(root.join("plain.txt"), b"x").unwrap();
        symlink(root.join("dir"), root.join("dlink")).unwrap();
        symlink(root.join("plain.txt"), root.join("flink")).unwrap();

        let result = list_dir(root, ".");
        assert!(result.success);
        let entries = result.entries.unwrap();
        let by_name = |n: &str| entries.iter().find(|e| e.name == n).unwrap();
        assert!(!by_name("plain.txt").is_symlink);
        assert!(!by_name("dir").is_symlink);
        assert!(by_name("dlink").is_symlink);
        assert!(by_name("flink").is_symlink);
        // 链接的 type 仍按目标上报，前端才能双击进入被链接的目录。
        assert_eq!(by_name("dlink").entry_type, "directory");
        assert_eq!(by_name("flink").entry_type, "file");
    }

    #[test]
    fn test_b64_roundtrip() {
        let original = b"Hello, base64 world!\0\x01\xff";
        let encoded = encode_b64(original);
        let decoded = decode_b64(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_format_mode() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("mode_test.txt");
        std::fs::write(&file_path, b"test").unwrap();

        let metadata = file_path.metadata().unwrap();
        let mode = format_mode(&metadata);
        assert!(mode.starts_with('-'));

        let dir_metadata = dir.path().metadata().unwrap();
        let dir_mode = format_mode(&dir_metadata);
        assert!(dir_mode.starts_with('d'));
    }

    #[test]
    fn test_strip_leading_seps_unix() {
        assert_eq!(strip_leading_seps("/sub/dir"), "sub/dir");
        assert_eq!(strip_leading_seps("sub/dir"), "sub/dir");
    }

    #[test]
    fn test_strip_leading_seps_windows() {
        assert_eq!(strip_leading_seps("\\sub\\dir"), "sub\\dir");
        assert_eq!(strip_leading_seps("/\\mixed"), "mixed");
    }
}
