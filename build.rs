//! Build script. On macOS, copies the mediaremote-adapter packaging assets
//! (see assets/macos/mediaremote-adapter/README.md) next to the binary so
//! dev builds pick them up via the runtime search path. Release packaging
//! must carry the folder alongside the shipped binary the same way.

use std::path::{Path, PathBuf};

/// One filesystem entry to stage: a regular file, a symlink (rebuilt with the
/// same target), or a directory (recursed).
enum Staged {
    File(PathBuf),
    Link(PathBuf, PathBuf),
    Dir(PathBuf),
}

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    // OUT_DIR = <target>/<profile>/build/<crate>-<hash>/out — the binary
    // lives three levels up.
    let Some(bin_dir) = Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .map(Path::to_path_buf)
    else {
        return;
    };
    let src = Path::new(&manifest).join("assets/macos/mediaremote-adapter");
    let dst = bin_dir.join("mediaremote-adapter");
    if let Err(err) = copy_tree(&src, &dst) {
        println!("cargo:warning=cannot stage mediaremote-adapter assets: {err}");
    }
    println!("cargo:rerun-if-changed=assets/macos/mediaremote-adapter");
}

fn stage_entries(src: &Path) -> std::io::Result<Vec<Staged>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(entry.path())?;
            out.push(Staged::Link(entry.path(), target));
        } else if metadata.is_dir() {
            out.push(Staged::Dir(entry.path()));
        } else {
            out.push(Staged::File(entry.path()));
        }
    }
    Ok(out)
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for staged in stage_entries(src)? {
        match staged {
            Staged::File(from) => {
                let name = from.file_name().expect("entry name");
                // Copy unconditionally: the tree is small and this avoids a
                // stale framework after an asset update.
                std::fs::copy(&from, dst.join(name))?;
            }
            Staged::Link(from, target) => {
                let name = from.file_name().expect("entry name");
                let to = dst.join(name);
                if to.symlink_metadata().is_ok() {
                    std::fs::remove_file(&to)?;
                }
                // The staging pass only runs for macOS targets, but this
                // script must still compile on non-unix hosts.
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(&target, &to)?;
                }
                #[cfg(not(unix))]
                {
                    let _ = &target;
                }
            }
            Staged::Dir(from) => {
                let name = from.file_name().expect("entry name");
                copy_tree(&from, &dst.join(name))?;
            }
        }
    }
    Ok(())
}
