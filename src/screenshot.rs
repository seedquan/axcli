//! Screenshot capture and image saving utilities.

use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use objc2_core_foundation::{CFMutableData, CFRetained, CFString, CGPoint, CGRect, CGSize};
#[allow(deprecated)]
use objc2_core_graphics::{
    CGImage, CGWindowImageOption, CGWindowListCreateImage, CGWindowListOption,
};
use objc2_image_io::CGImageDestination;

unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
}

/// Ensure the CoreGraphics window server connection is initialized.
/// Must be called before any CGImage/ImageIO operations in a CLI context.
pub fn ensure_cg_init() {
    unsafe {
        CGMainDisplayID();
    }
}

/// Capture a screenshot of the given screen rectangle.
#[allow(deprecated)]
pub fn capture(rect: CGRect) -> Option<CFRetained<CGImage>> {
    CGWindowListCreateImage(rect, CGWindowListOption(1), 0, CGWindowImageOption(0))
}

/// Capture a full-screen screenshot.
pub fn capture_full() -> Option<CFRetained<CGImage>> {
    capture(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(0.0, 0.0)))
}

fn unique_name(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{prefix}_{}_{nanos}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Choose a user-private location; never fall back to the shared /tmp directory.
pub fn default_output_path() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    default_output_path_in(Path::new(&home))
}

fn default_output_path_in(home: &Path) -> io::Result<PathBuf> {
    if !home.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "HOME must be absolute",
        ));
    }
    // Resolve the home directory once, then refuse symlinks below it.
    let mut directory = home.canonicalize()?;
    let owner = fs::metadata(&directory)?.uid();
    for component in ["Library", "Caches", "axcli"] {
        directory.push(component);
        match DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() || metadata.uid() != owner {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "screenshot directory must be an owned directory, not a symlink",
            ));
        }
        if component == "axcli" {
            let handle = File::open(&directory)?;
            let opened = handle.metadata()?;
            if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "screenshot directory changed while opening",
                ));
            }
            handle.set_permissions(Permissions::from_mode(0o700))?;
        } else if metadata.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "screenshot parent directory is writable by other users",
            ));
        }
    }
    Ok(directory.join(format!("{}.png", unique_name("ax_screenshot"))))
}

/// Stage a new private inode, then replace the output name without following it.
/// ImageIO never receives a filesystem URL, so it cannot recreate a 0644 file.
fn write_private_png(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "output must name a file",
        ));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    for _ in 0..16 {
        let staging = parent.join(format!(".{}.tmp", unique_name("axcli")));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staging)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let result = (|| {
            // Set permissions on the open inode before writing any image bytes.
            file.set_permissions(Permissions::from_mode(0o600))?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(&staging, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&staging);
        }
        return result;
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot allocate a private screenshot file",
    ))
}

/// Save a CGImage as a private PNG, atomically replacing an existing output.
pub fn save_png(image: &CGImage, path: impl AsRef<Path>) -> bool {
    let Some(data) = CFMutableData::new(None, 0) else {
        return false;
    };
    let png_type = CFString::from_str("public.png");
    // SAFETY: The mutable buffer outlives the encoder and options is None.
    let dest = unsafe { CGImageDestination::with_data(&data, &png_type, 1, None) };
    let Some(dest) = dest else { return false };
    // SAFETY: image is a live CGImage and no options dictionary is supplied.
    unsafe {
        dest.add_image(image, None);
        if !dest.finalize() {
            return false;
        }
    }
    drop(dest);
    write_private_png(path.as_ref(), &data.to_vec()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new() -> Self {
            let path = std::env::temp_dir().join(unique_name("axcli_test"));
            DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().mode() & 0o7777
    }

    #[test]
    fn default_paths_are_unique_and_inside_a_private_cache_directory() {
        let home = Sandbox::new();
        let first = default_output_path_in(&home.0).unwrap();
        let second = default_output_path_in(&home.0).unwrap();
        let cache = home.0.join("Library/Caches/axcli");
        assert_eq!(first.parent(), Some(cache.as_path()));
        assert_eq!(second.parent(), Some(cache.as_path()));
        assert_eq!(mode(&cache), 0o700);
        assert_ne!(first, second);
        assert_eq!(first.extension().unwrap(), "png");
        write_private_png(&first, b"private image").unwrap();
        assert_eq!(mode(&first), 0o600);
    }

    #[test]
    fn existing_cache_directory_permissions_are_tightened() {
        let home = Sandbox::new();
        let first = default_output_path_in(&home.0).unwrap();
        let cache = first.parent().unwrap();
        fs::set_permissions(cache, Permissions::from_mode(0o755)).unwrap();
        default_output_path_in(&home.0).unwrap();
        assert_eq!(mode(cache), 0o700);
    }

    #[test]
    fn symlinked_cache_components_are_rejected_without_changing_the_target() {
        for component in ["Library", "Library/Caches", "Library/Caches/axcli"] {
            let home = Sandbox::new();
            let target = Sandbox::new();
            fs::set_permissions(&target.0, Permissions::from_mode(0o755)).unwrap();
            let link = home.0.join(component);
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(link.parent().unwrap())
                .unwrap();
            symlink(&target.0, &link).unwrap();
            assert_eq!(
                default_output_path_in(&home.0).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            assert_eq!(mode(&target.0), 0o755);
            assert_eq!(fs::read_dir(&target.0).unwrap().count(), 0);
        }
    }

    #[test]
    fn insecure_parent_and_relative_home_are_rejected() {
        let home = Sandbox::new();
        let library = home.0.join("Library");
        fs::create_dir(&library).unwrap();
        fs::set_permissions(&library, Permissions::from_mode(0o777)).unwrap();
        assert_eq!(
            default_output_path_in(&home.0).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            default_output_path_in(Path::new("relative-home"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn replacement_is_private_and_does_not_change_other_hard_links() {
        let sandbox = Sandbox::new();
        let output = sandbox.0.join("output.png");
        let alias = sandbox.0.join("old-image.png");
        fs::write(&output, b"old content").unwrap();
        fs::set_permissions(&output, Permissions::from_mode(0o644)).unwrap();
        fs::hard_link(&output, &alias).unwrap();
        write_private_png(&output, b"new private content").unwrap();
        assert_eq!(fs::read(&output).unwrap(), b"new private content");
        assert_eq!(mode(&output), 0o600);
        assert_eq!(fs::read(&alias).unwrap(), b"old content");
        assert_eq!(mode(&alias), 0o644);
        assert_eq!(fs::read_dir(&sandbox.0).unwrap().count(), 2);
    }

    #[test]
    fn output_symlink_is_replaced_without_writing_its_target() {
        let sandbox = Sandbox::new();
        let output = sandbox.0.join("output.png");
        let target = sandbox.0.join("protected.txt");
        fs::write(&target, b"must not change").unwrap();
        symlink(&target, &output).unwrap();
        write_private_png(&output, b"private image").unwrap();
        assert!(!fs::symlink_metadata(&output).unwrap().is_symlink());
        assert_eq!(mode(&output), 0o600);
        assert_eq!(fs::read(&target).unwrap(), b"must not change");
    }

    #[test]
    fn failed_replacement_removes_staging_file() {
        let sandbox = Sandbox::new();
        let directory = sandbox.0.join("output.png");
        fs::create_dir(&directory).unwrap();
        assert!(write_private_png(&directory, b"private image").is_err());
        assert_eq!(fs::read_dir(&sandbox.0).unwrap().count(), 1);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
    }

    #[test]
    fn concurrent_replacements_leave_one_complete_private_file() {
        let sandbox = Sandbox::new();
        let output = sandbox.0.join("output.png");
        let workers: Vec<_> = (0..8u8)
            .map(|value| {
                let output = output.clone();
                std::thread::spawn(move || write_private_png(&output, &vec![value; 4096]).unwrap())
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let bytes = fs::read(&output).unwrap();
        assert_eq!(bytes.len(), 4096);
        assert!(bytes.iter().all(|byte| *byte == bytes[0]));
        assert_eq!(mode(&output), 0o600);
        assert_eq!(fs::read_dir(&sandbox.0).unwrap().count(), 1);
    }

    #[test]
    fn synthetic_image_is_encoded_as_a_private_png_without_screen_capture() {
        use objc2_core_foundation::CFData;
        use objc2_core_graphics::{
            CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider,
        };

        let sandbox = Sandbox::new();
        // An explicitly chosen shared directory must not broaden file access.
        fs::set_permissions(&sandbox.0, Permissions::from_mode(0o755)).unwrap();
        let output = sandbox.0.join("output.png");
        let pixels = CFData::from_bytes(&[0, 0, 0, 255]);
        let provider = CGDataProvider::with_cf_data(Some(&pixels)).unwrap();
        let space = CGColorSpace::new_device_rgb().unwrap();
        // SAFETY: provider owns one complete RGBA pixel and decode is null.
        let image = unsafe {
            CGImage::new(
                1,
                1,
                8,
                32,
                4,
                Some(&space),
                CGBitmapInfo(1),
                Some(&provider),
                std::ptr::null(),
                false,
                CGColorRenderingIntent(0),
            )
        }
        .unwrap();
        assert!(save_png(&image, &output));
        assert!(fs::read(&output).unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(mode(&output), 0o600);
        assert_eq!(mode(&sandbox.0), 0o755);
        assert_eq!(fs::read_dir(&sandbox.0).unwrap().count(), 1);
    }
}
