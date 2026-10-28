use std::fs::File;
use std::io;
use std::path::Path;

use super::libraries::NativeJar;

pub fn extract_natives(jars: &[NativeJar], dest: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(dest)? {
        let path = entry?.path();
        if path.is_dir() && !path.is_symlink() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    for jar in jars {
        let mut archive = zip::ZipArchive::new(File::open(&jar.path)?).map_err(io::Error::other)?;
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(io::Error::other)?;
            if entry.is_dir() {
                continue;
            }
            let Some(rel) = entry.enclosed_name() else {
                continue;
            };
            let name = rel.to_string_lossy().replace('\\', "/");
            if name.starts_with("META-INF/") || jar.exclude.iter().any(|ex| name.starts_with(ex)) {
                continue;
            }
            let target = dest.join(rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            io::copy(&mut entry, &mut File::create(&target)?)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_jar(path: &Path, entries: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn extract_natives_skips_meta_inf_and_excludes() {
        let tmp = tempfile::tempdir().unwrap();
        let jar = tmp.path().join("n.jar");
        make_jar(
            &jar,
            &[
                ("libfoo.so", b"so"),
                ("META-INF/MANIFEST.MF", b"m"),
                ("sub/x.txt", b"x"),
            ],
        );
        let dest = tmp.path().join("natives");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("stale.so"), b"old").unwrap();
        let jars = [NativeJar {
            path: jar,
            exclude: vec!["sub/".into()],
        }];
        extract_natives(&jars, &dest).unwrap();
        let names: Vec<String> = std::fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["libfoo.so".to_string()]);
        assert_eq!(std::fs::read(dest.join("libfoo.so")).unwrap(), b"so");
    }

    #[test]
    fn extract_natives_blocks_zip_slip() {
        let tmp = tempfile::tempdir().unwrap();
        let jar = tmp.path().join("evil.jar");
        make_jar(&jar, &[("../evil.so", b"x"), ("ok.so", b"y")]);
        let dest = tmp.path().join("natives");
        extract_natives(
            &[NativeJar {
                path: jar,
                exclude: vec![],
            }],
            &dest,
        )
        .unwrap();
        assert!(!tmp.path().join("evil.so").exists());
        assert!(dest.join("ok.so").exists());
    }
}
