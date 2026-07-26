//! Attachment utilities shared by the reader and composers.

use crate::model::Attachment;
use anyhow::{Context as _, Result};
use std::{
    collections::HashSet,
    fs::File,
    io::{Seek, Write},
    path::{Path, PathBuf},
};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

fn inferred_mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("svg") => "image/svg+xml",
        Some("txt" | "md") => "text/plain",
        Some("zip") => "application/zip",
        Some("doc") => "application/msword",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xls") => "application/vnd.ms-excel",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("ppt") => "application/vnd.ms-powerpoint",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        Some("csv") => "text/csv",
        Some("ics") => "text/calendar",
        Some("eml") => "message/rfc822",
        Some("msg") => "application/vnd.ms-outlook",
        _ => "application/octet-stream",
    }
}

pub(super) fn mime_for_path(path: &Path) -> String {
    inferred_mime(path).to_string()
}

/// MIME type for image extensions supported by the editor.
pub(super) fn image_mime_for_path(path: &Path) -> Option<&'static str> {
    let mime = inferred_mime(path);
    mime.starts_with("image/").then_some(mime)
}

pub(super) fn format_size(size: u64) -> String {
    if size >= 1_000_000 {
        tr!("size-mb", { value: format!("{:.1}", size as f64 / 1_000_000.0) }).to_string()
    } else if size >= 1_000 {
        tr!("size-kb", { value: format!("{:.0}", size as f64 / 1_000.0) }).to_string()
    } else {
        tr!("size-bytes", { value: size }).to_string()
    }
}

pub(super) fn icon_name(attachment: &Attachment) -> &'static str {
    let extension = Path::new(&attachment.filename)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);

    if attachment.mime.starts_with("image/") {
        return "image";
    }
    if attachment.mime.starts_with("audio/") {
        return "file-audio";
    }
    if attachment.mime.starts_with("video/") {
        return "file-video";
    }

    match extension.as_deref() {
        Some("pdf") => "file-text",
        Some("doc" | "docx" | "odt" | "rtf") => "file-type",
        Some("xls" | "xlsx" | "ods" | "csv") => "file-spreadsheet",
        Some("ppt" | "pptx" | "odp") => "presentation",
        Some("zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz") => "archive",
        Some("mp3" | "wav" | "ogg" | "flac" | "m4a" | "aac") => "file-audio",
        Some("mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v") => "file-video",
        Some("ics") => "calendar",
        Some("eml" | "msg") => "mail",
        Some("html" | "htm" | "css" | "js" | "ts" | "json" | "xml" | "rs" | "py" | "sh") => {
            "square-terminal"
        }
        Some("txt" | "md") => "file-text",
        _ if attachment.mime == "application/pdf" => "file-text",
        _ if attachment.mime.contains("wordprocessingml")
            || attachment.mime == "application/msword" =>
        {
            "file-type"
        }
        _ if attachment.mime.contains("spreadsheetml")
            || attachment.mime == "application/vnd.ms-excel" =>
        {
            "file-spreadsheet"
        }
        _ if attachment.mime.contains("presentationml")
            || attachment.mime == "application/vnd.ms-powerpoint" =>
        {
            "presentation"
        }
        _ if attachment.mime.contains("zip") || attachment.mime.contains("compressed") => "archive",
        _ => "file",
    }
}

fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|character| {
            if matches!(character, '/' | '\\' | ':') {
                '_'
            } else {
                character
            }
        })
        .collect();
    if cleaned.is_empty() {
        tr!("attachment-default-filename").to_string()
    } else {
        cleaned
    }
}

pub(super) fn suggested_filename(attachment: &Attachment) -> String {
    sanitize_filename(&attachment.filename)
}

/// User-friendly initial directory for save dialogs.
pub(super) fn download_directory() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|directories| directories.download_dir().map(Path::to_path_buf))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

fn unique_archive_filename(name: &str, used: &mut HashSet<String>) -> String {
    let sanitized = sanitize_filename(name);
    if used.insert(sanitized.to_lowercase()) {
        return sanitized;
    }

    let path = Path::new(&sanitized);
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(sanitized.as_str());
    let extension = path.extension().and_then(|extension| extension.to_str());
    for copy in 2.. {
        let candidate = match extension {
            Some(extension) => format!("{stem} ({copy}).{extension}"),
            None => format!("{stem} ({copy})"),
        };
        if used.insert(candidate.to_lowercase()) {
            return candidate;
        }
    }
    unreachable!("an unbounded numeric suffix always produces a unique filename")
}

fn write_zip<W>(writer: W, files: &[Attachment]) -> Result<W>
where
    W: Write + Seek,
{
    let mut archive = ZipWriter::new(writer);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let mut used_names = HashSet::new();

    for file in files {
        let Some(bytes) = file.bytes.as_deref() else {
            anyhow::bail!(tr!("viewer-attachment-content-unavailable", {
                filename: file.filename.clone()
            }));
        };
        let filename = unique_archive_filename(&file.filename, &mut used_names);
        archive.start_file(filename, options).with_context(|| {
            tr!("viewer-attachments-zip-entry-error", {
                filename: file.filename.clone()
            })
        })?;
        archive.write_all(bytes).with_context(|| {
            tr!("viewer-attachments-zip-entry-error", {
                filename: file.filename.clone()
            })
        })?;
    }

    archive
        .finish()
        .context(tr!("viewer-attachments-zip-finish-error"))
}

/// Compresses all attachment bytes into `path`. The caller runs this helper
/// on gpui's background executor so compression and disk I/O never block the
/// rendering thread.
pub(super) fn save_all_as_zip(path: &Path, files: &[Attachment]) -> Result<()> {
    let file = File::create(path).with_context(|| {
        tr!("viewer-attachments-file-create-error", {
            path: path.display().to_string()
        })
    })?;
    match write_zip(file, files) {
        Ok(file) => file.sync_all().with_context(|| {
            tr!("viewer-attachments-file-write-error", {
                path: path.display().to_string()
            })
        }),
        Err(error) => {
            let _ = std::fs::remove_file(path);
            Err(error)
        }
    }
}

/// Writes one attachment to a user-selected destination. The caller runs this
/// helper on gpui's background executor.
pub(super) fn save_as(path: &Path, attachment: &Attachment) -> Result<()> {
    let Some(bytes) = attachment.bytes.as_deref() else {
        anyhow::bail!(tr!("viewer-attachment-content-unavailable", {
            filename: attachment.filename.clone()
        }));
    };
    let mut file = File::create(path).with_context(|| {
        tr!("viewer-attachments-file-create-error", {
            path: path.display().to_string()
        })
    })?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = result {
        let _ = std::fs::remove_file(path);
        return Err(error).with_context(|| {
            tr!("viewer-attachments-file-write-error", {
                path: path.display().to_string()
            })
        });
    }
    Ok(())
}

/// Writes an attachment to temporary storage and opens it without blocking the UI.
pub(super) fn open(attachment: Attachment) {
    std::thread::spawn(move || {
        let Some(bytes) = attachment.bytes else {
            log::warn!("attachment has no content: {}", attachment.filename);
            return;
        };
        let path = match stage_temporary_attachment(&attachment.filename, &bytes) {
            Ok(path) => path,
            Err(error) => {
                log::warn!("failed to write attachment: {error:#}");
                return;
            }
        };
        if let Err(error) = open::that_detached(path) {
            log::warn!("failed to open attachment: {error:#}");
        }
    });
}

/// Materializes `bytes` under a private, single-use directory and returns the
/// path handed to the system viewer.
///
/// Every open gets its own randomly named `0700` directory. A shared, guessable
/// path in `/tmp` would let any local user read the document, let two
/// attachments with the same name overwrite each other, and let a pre-created
/// symlink redirect the write. `create_new` on both the directory and the file
/// makes each of those a hard error rather than a silent success.
fn stage_temporary_attachment(filename: &str, bytes: &[u8]) -> Result<PathBuf> {
    let root = std::env::temp_dir().join("aviary-attachments");
    create_private_dir_all(&root)?;

    let directory = root.join(unique_suffix());
    create_private_dir(&directory)?;

    let path = directory.join(sanitize_filename(filename));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(path)
}

/// Creates the shared parent, tolerating a directory we already own. It only
/// ever holds per-open subdirectories, so a hostile pre-creation cannot expose
/// anything by itself — but keeping it `0700` avoids leaking the names.
fn create_private_dir_all(path: &Path) -> Result<()> {
    match create_private_dir(path) {
        Err(error) if path.is_dir() => {
            log::debug!("reusing temporary attachment root: {error:#}");
            Ok(())
        }
        result => result,
    }
}

fn create_private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .with_context(|| format!("creating {}", path.display()))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path).with_context(|| format!("creating {}", path.display()))
    }
}

fn unique_suffix() -> String {
    let mut bytes = [0_u8; 12];
    if getrandom::fill(&mut bytes).is_err() {
        return format!("pid-{}", std::process::id());
    }
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        image_mime_for_path, inferred_mime, sanitize_filename, stage_temporary_attachment,
        unique_archive_filename, write_zip,
    };
    use crate::model::Attachment;
    use std::{collections::HashSet, io::Cursor, path::Path};

    /// A document handed to the system viewer stays readable by its owner only,
    /// and two attachments sharing a name must not land on the same path.
    #[test]
    fn staged_attachments_are_private_and_never_collide() {
        let first = stage_temporary_attachment("facture.pdf", b"first").expect("first staging");
        let second = stage_temporary_attachment("facture.pdf", b"second").expect("second staging");

        assert_ne!(first, second);
        assert_eq!(std::fs::read(&first).expect("first read"), b"first");
        assert_eq!(std::fs::read(&second).expect("second read"), b"second");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for path in [&first, &second] {
                let mode = std::fs::metadata(path)
                    .expect("metadata")
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "unexpected mode {:o}", mode & 0o777);
            }
        }

        for path in [&first, &second] {
            if let Some(parent) = path.parent() {
                std::fs::remove_dir_all(parent).ok();
            }
        }
    }

    #[test]
    fn infers_known_file_types_case_insensitively() {
        assert_eq!(inferred_mime(Path::new("photo.JPEG")), "image/jpeg");
        assert_eq!(
            inferred_mime(Path::new("report.docx")),
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        );
        assert_eq!(
            inferred_mime(Path::new("unknown.bin")),
            "application/octet-stream"
        );
    }

    #[test]
    fn restricts_inline_images_to_image_mimes() {
        assert_eq!(
            image_mime_for_path(Path::new("illustration.svg")),
            Some("image/svg+xml")
        );
        assert_eq!(image_mime_for_path(Path::new("document.pdf")), None);
    }

    #[test]
    fn sanitizes_path_separators() {
        assert_eq!(
            sanitize_filename("folder\\unsafe:name.txt"),
            "folder_unsafe_name.txt"
        );
    }

    #[test]
    fn deduplicates_archive_filenames_case_insensitively() {
        let mut used = HashSet::new();
        assert_eq!(
            unique_archive_filename("report.pdf", &mut used),
            "report.pdf"
        );
        assert_eq!(
            unique_archive_filename("REPORT.pdf", &mut used),
            "REPORT (2).pdf"
        );
        assert_eq!(
            unique_archive_filename("report.pdf", &mut used),
            "report (3).pdf"
        );
    }

    #[test]
    fn writes_each_attachment_to_a_zip_entry() {
        let files = vec![
            Attachment {
                id: String::new(),
                filename: "notes.txt".into(),
                mime: "text/plain".into(),
                size: 5,
                bytes: Some(b"hello".to_vec()),
            },
            Attachment {
                id: String::new(),
                filename: "notes.txt".into(),
                mime: "text/plain".into(),
                size: 5,
                bytes: Some(b"world".to_vec()),
            },
        ];
        let cursor = write_zip(Cursor::new(Vec::new()), &files).expect("zip should be written");
        let mut archive =
            zip::ZipArchive::new(Cursor::new(cursor.into_inner())).expect("zip should be readable");

        assert_eq!(archive.len(), 2);
        assert_eq!(archive.by_index(0).unwrap().name(), "notes.txt");
        assert_eq!(archive.by_index(1).unwrap().name(), "notes (2).txt");
    }
}
