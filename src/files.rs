//! The two installs that need no administrator rights: replacing an AppImage
//! and extracting a tarball over an existing install.

use crate::error::{Error, ErrorCode, Result};
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

fn fail(msg: impl Into<String>) -> Error {
    Error::new(ErrorCode::InstallFailed, msg)
}

fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

fn denied(path: &Path, e: &io::Error) -> Error {
    if e.kind() == io::ErrorKind::PermissionDenied {
        fail(format!("praeco may not write to {}. Its folder is not writable for you.", path.display()))
    } else {
        fail(format!("Could not write {}: {e}", path.display()))
    }
}

/// Replace `dest` with the contents of `src` atomically: copy to a hidden file
/// beside `dest`, fsync it, give it `dest`'s permissions, rename over `dest`,
/// fsync the directory. A running AppImage keeps working (its inode lives on
/// until it exits). On any failure `dest` is untouched and the temp file is removed.
pub fn replace_file_atomically(src: &Path, dest: &Path) -> Result<()> {
    let dir = dest.parent().ok_or_else(|| fail("The install location has no folder"))?;
    let name = dest.file_name().ok_or_else(|| fail("The install location has no file name"))?.to_string_lossy().into_owned();
    // Keep the old mode, but make sure the owner can run it.
    let mode = fs::metadata(dest).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o755) | 0o100;
    let tmp = dir.join(format!(".{name}.praeco-new-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut from = File::open(src)?;
        let mut to = fs::OpenOptions::new().write(true).create_new(true).open(&tmp).map_err(|e| denied(dir, &e))?;
        io::copy(&mut from, &mut to).map_err(|e| denied(&tmp, &e))?;
        to.set_permissions(fs::Permissions::from_mode(mode))?;
        to.sync_all()?;
        drop(to);
        fs::rename(&tmp, dest).map_err(|e| denied(dest, &e))?;
        sync_dir(dir);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn clean_components(p: &Path) -> Option<Vec<String>> {
    let mut v = Vec::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => v.push(s.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(v)
}

fn open_tar(path: &Path) -> Result<tar::Archive<flate2::read::GzDecoder<File>>> {
    Ok(tar::Archive::new(flate2::read::GzDecoder::new(File::open(path)?)))
}

fn bad_archive(e: impl std::fmt::Display) -> Error {
    fail(format!("The downloaded archive could not be read: {e}"))
}

/// Where the tarball goes: which archive prefix to drop and which directory to
/// extract into.
#[derive(Debug, PartialEq, Eq)]
struct Layout {
    prefix: Vec<String>,
    root: PathBuf,
}

/// Match the archive against the installed executable. The archive entry for
/// the executable (same file name, longest common path suffix with `exe`)
/// tells how the archive is laid out: everything above that suffix in the
/// archive is dropped, everything above it in `exe`'s path is the install root.
fn find_layout(archive: &Path, exe: &Path) -> Result<Layout> {
    let exe_parts = clean_components(&exe.components().skip(1).collect::<PathBuf>()).ok_or_else(|| fail("Unexpected executable path"))?;
    let exe_name = exe_parts.last().ok_or_else(|| fail("Unexpected executable path"))?.clone();
    let mut best: Option<(usize, Vec<String>)> = None;
    let mut ar = open_tar(archive)?;
    for ent in ar.entries().map_err(bad_archive)? {
        let ent = ent.map_err(bad_archive)?;
        if !ent.header().entry_type().is_file() {
            continue;
        }
        let path = ent.path().map_err(bad_archive)?.into_owned();
        let Some(parts) = clean_components(&path) else { continue };
        if parts.last() != Some(&exe_name) {
            continue;
        }
        let k = parts.iter().rev().zip(exe_parts.iter().rev()).take_while(|(a, b)| a == b).count();
        let better = match &best {
            None => true,
            Some((bk, bp)) => k > *bk || (k == *bk && parts.len() < bp.len()),
        };
        if better {
            best = Some((k, parts));
        }
    }
    let (k, parts) = best.ok_or_else(|| fail(format!("The update archive does not contain {exe_name}, so praeco cannot tell where to put it")))?;
    if k >= exe_parts.len() {
        return Err(fail("Unexpected executable path"));
    }
    let root: PathBuf = Path::new("/").join(exe_parts[..exe_parts.len() - k].join("/"));
    Ok(Layout { prefix: parts[..parts.len() - k].to_vec(), root })
}

/// Extract `archive` over the install that contains `exe`.
///
/// 1. Work out the install root from the archive layout (see `find_layout`).
/// 2. Extract into a private staging directory inside that root (same file
///    system), refusing absolute paths, `..`, hard links, devices, and
///    symlinks that point outside their own tree.
/// 3. Rename each staged file over its destination. Each rename is atomic, so
///    a running program never sees a half-written file; the tree as a whole
///    is not one transaction. Files that exist on disk but not in the archive
///    are left alone.
pub fn install_tarball(archive: &Path, exe: &Path) -> Result<()> {
    let exe = fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let layout = find_layout(archive, &exe)?;
    let root = &layout.root;
    if root.components().count() < 3 {
        return Err(fail(format!("Refusing to unpack over {}", root.display())));
    }
    let stage = tempfile::Builder::new().prefix(".praeco-stage-").tempdir_in(root).map_err(|e| denied(root, &e))?;

    let mut ar = open_tar(archive)?;
    let mut extracted = 0usize;
    for ent in ar.entries().map_err(bad_archive)? {
        let mut ent = ent.map_err(bad_archive)?;
        let path = ent.path().map_err(bad_archive)?.into_owned();
        let Some(parts) = clean_components(&path) else {
            return Err(fail(format!("The update archive contains an unsafe path ({}); nothing was installed", path.display())));
        };
        if !parts.starts_with(&layout.prefix) {
            continue;
        }
        let rel = &parts[layout.prefix.len()..];
        if rel.is_empty() {
            continue;
        }
        let out = stage.path().join(rel.join("/"));
        let ty = ent.header().entry_type();
        if ty.is_dir() {
            fs::create_dir_all(&out)?;
        } else if ty.is_file() {
            if let Some(p) = out.parent() {
                fs::create_dir_all(p)?;
            }
            let mode = ent.header().mode().unwrap_or(0o644) & 0o777;
            let mut f = File::create(&out)?;
            io::copy(&mut ent, &mut f).map_err(bad_archive)?;
            f.set_permissions(fs::Permissions::from_mode(mode))?;
            f.sync_all()?;
            extracted += 1;
        } else if ty.is_symlink() {
            let target = ent.link_name().map_err(bad_archive)?.ok_or_else(|| fail("The update archive has a broken symlink"))?.into_owned();
            if target.is_absolute() || target.components().any(|c| matches!(c, Component::ParentDir)) {
                return Err(fail(format!("The update archive has a symlink that points outside itself ({}); nothing was installed", path.display())));
            }
            if let Some(p) = out.parent() {
                fs::create_dir_all(p)?;
            }
            std::os::unix::fs::symlink(&target, &out)?;
            extracted += 1;
        } else {
            return Err(fail(format!("The update archive contains something praeco will not unpack ({}); nothing was installed", path.display())));
        }
    }
    if extracted == 0 {
        return Err(fail("The update archive was empty"));
    }
    commit_tree(stage.path(), root)?;
    sync_dir(root);
    Ok(())
}

fn commit_tree(stage: &Path, dest: &Path) -> Result<()> {
    for ent in fs::read_dir(stage)? {
        let ent = ent?;
        let from = ent.path();
        let to = dest.join(ent.file_name());
        let ty = ent.file_type()?;
        if ty.is_dir() {
            match fs::symlink_metadata(&to) {
                Ok(m) if !m.is_dir() => return Err(fail(format!("{} is in the way (not a folder)", to.display()))),
                Ok(_) => {}
                Err(_) => fs::create_dir_all(&to).map_err(|e| denied(&to, &e))?,
            }
            commit_tree(&from, &to)?;
        } else {
            if fs::symlink_metadata(&to).map(|m| m.is_dir()).unwrap_or(false) {
                return Err(fail(format!("{} is in the way (a folder)", to.display())));
            }
            fs::rename(&from, &to).map_err(|e| denied(&to, &e))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn make_tgz(path: &Path, entries: &[(&str, &[u8], u32)], links: &[(&str, &str)]) {
        let gz = flate2::write::GzEncoder::new(File::create(path).unwrap(), flate2::Compression::fast());
        let mut b = tar::Builder::new(gz);
        for (name, data, mode) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(*mode);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            b.append_data(&mut h, name, *data).unwrap();
        }
        for (name, target) in links {
            let mut h = tar::Header::new_gnu();
            h.set_size(0);
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_mode(0o777);
            b.append_link(&mut h, name, target).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn atomic_replace_keeps_mode_and_leaves_no_temp() {
        let d = tempfile::tempdir().unwrap();
        let dest = d.path().join("App.AppImage");
        fs::write(&dest, "old").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o750)).unwrap();
        let src = d.path().join("new");
        fs::write(&src, "new contents").unwrap();
        replace_file_atomically(&src, &dest).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new contents");
        assert_eq!(fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o750);
        let names: Vec<_> = fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(!names.iter().any(|n| n.contains("praeco-new")), "{names:?}");
    }

    #[test]
    fn atomic_replace_failure_leaves_original() {
        let d = tempfile::tempdir().unwrap();
        let dest = d.path().join("App.AppImage");
        fs::write(&dest, "old").unwrap();
        let err = replace_file_atomically(&d.path().join("missing"), &dest).unwrap_err();
        assert_eq!(err.code, ErrorCode::Io);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old");
        let n = fs::read_dir(d.path()).unwrap().count();
        assert_eq!(n, 1);
    }

    #[test]
    fn layout_with_top_dir_and_bin() {
        let d = tempfile::tempdir().unwrap();
        let tgz = d.path().join("a.tar.gz");
        make_tgz(&tgz, &[("fairfax-1.0.5/bin/fairfax", b"x", 0o755), ("fairfax-1.0.5/share/a", b"y", 0o644)], &[]);
        let l = find_layout(&tgz, Path::new("/home/u/.local/bin/fairfax")).unwrap();
        assert_eq!(l.root, PathBuf::from("/home/u/.local"));
        assert_eq!(l.prefix, ["fairfax-1.0.5"]);
    }

    #[test]
    fn tarball_extracts_over_install() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("opt/fairfax");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/fairfax"), "old").unwrap();
        fs::write(root.join("keep.txt"), "mine").unwrap();
        let tgz = d.path().join("a.tar.gz");
        make_tgz(
            &tgz,
            &[("fairfax-linux/bin/fairfax", b"new", 0o755), ("fairfax-linux/lib/x.so.1", b"lib", 0o644)],
            &[("fairfax-linux/lib/x.so", "x.so.1")],
        );
        install_tarball(&tgz, &root.join("bin/fairfax")).unwrap();
        assert_eq!(fs::read_to_string(root.join("bin/fairfax")).unwrap(), "new");
        assert_eq!(fs::metadata(root.join("bin/fairfax")).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(fs::read_to_string(root.join("lib/x.so.1")).unwrap(), "lib");
        assert_eq!(fs::read_link(root.join("lib/x.so")).unwrap(), PathBuf::from("x.so.1"));
        assert_eq!(fs::read_to_string(root.join("keep.txt")).unwrap(), "mine");
        let stray: Vec<_> = fs::read_dir(&root).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with(".praeco")).collect();
        assert!(stray.is_empty());
    }

    #[test]
    fn tarball_refuses_escapes_and_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("opt/fairfax");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/fairfax"), "old").unwrap();
        let tgz = d.path().join("evil.tar.gz");
        make_tgz(&tgz, &[("bin/fairfax", b"new", 0o755)], &[("bin/evil", "../../../etc/passwd")]);
        let err = install_tarball(&tgz, &root.join("bin/fairfax")).unwrap_err();
        assert_eq!(err.code, ErrorCode::InstallFailed);
        assert_eq!(fs::read_to_string(root.join("bin/fairfax")).unwrap(), "old");
    }

    #[test]
    fn tarball_without_the_executable_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("opt/fairfax");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/fairfax"), "old").unwrap();
        let tgz = d.path().join("a.tar.gz");
        make_tgz(&tgz, &[("README", b"hi", 0o644)], &[]);
        assert!(install_tarball(&tgz, &root.join("bin/fairfax")).is_err());
    }
}
