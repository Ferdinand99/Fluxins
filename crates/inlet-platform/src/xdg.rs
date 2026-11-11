//! XDG base directories and desktop entry quoting (Linux).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or(Error::Unsupported("HOME is not set"))
}

fn base(var: &str, fallback: &str) -> Result<PathBuf> {
    // The spec says relative values are invalid and must be ignored.
    match std::env::var_os(var).map(PathBuf::from) {
        Some(p) if p.is_absolute() => Ok(p),
        _ => Ok(home()?.join(fallback)),
    }
}

pub(crate) fn config_home() -> Result<PathBuf> {
    base("XDG_CONFIG_HOME", ".config")
}

pub(crate) fn data_home() -> Result<PathBuf> {
    base("XDG_DATA_HOME", ".local/share")
}

/// Quotes a path for the `Exec` key of a desktop entry: the argument is put in
/// double quotes with `"`, `` ` ``, `$` and `\` escaped, `%` is doubled, and then
/// the string-value escaping turns every backslash into two.
pub(crate) fn exec_arg(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .ok_or(Error::InvalidInput("executable path is not valid Unicode"))?;
    if text.chars().any(char::is_control) {
        return Err(Error::InvalidInput(
            "executable path contains a control character",
        ));
    }
    let mut quoted = String::from("\"");
    for c in text.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    Ok(quoted.replace('\\', "\\\\"))
}

/// Writes through a temporary file in the same directory and renames it, so a
/// crash never leaves half a file behind.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic_with(path, bytes, None)
}

/// Replaces a file the user may have set up by hand (`mimeapps.list` kept in a
/// dotfiles folder, say): a symbolic link is followed and stays a link, the file it
/// points to is replaced atomically, and that file's permissions are kept.
pub(crate) fn replace_keeping(path: &Path, bytes: &[u8]) -> Result<()> {
    let target = resolve_links(path)?;
    let permissions = std::fs::metadata(&target).ok().map(|m| m.permissions());
    write_atomic_with(&target, bytes, permissions)
}

/// The file a path ends at after following its symbolic links (at most 16). A
/// relative link target counts from the link's own folder. A dangling link gives
/// the path it points to, so the file is created there.
fn resolve_links(path: &Path) -> Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..16 {
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(&current)?;
                current = if target.is_absolute() {
                    target
                } else {
                    current
                        .parent()
                        .ok_or(Error::InvalidInput("path has no parent"))?
                        .join(target)
                };
            }
            _ => return Ok(current),
        }
    }
    Err(Error::InvalidInput("too many symbolic links"))
}

fn write_atomic_with(
    path: &Path,
    bytes: &[u8],
    permissions: Option<std::fs::Permissions>,
) -> Result<()> {
    let dir = path
        .parent()
        .ok_or(Error::InvalidInput("path has no parent"))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or(Error::InvalidInput("path has no file name"))?;
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(result?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_arguments_are_quoted_and_escaped() {
        assert_eq!(
            exec_arg(Path::new("/usr/bin/inlet")).unwrap(),
            "\"/usr/bin/inlet\""
        );
        assert_eq!(
            exec_arg(Path::new("/opt/a \"b\" $c `d` 100%/e\\f")).unwrap(),
            "\"/opt/a \\\\\"b\\\\\" \\\\$c \\\\`d\\\\` 100%%/e\\\\\\\\f\""
        );
        assert!(exec_arg(Path::new("/opt/a\nb")).is_err());
    }
}
