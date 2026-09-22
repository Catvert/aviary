//! Attachment utilities shared by the reader and composers.

use crate::model::Attachment;
use anyhow::{Context as _, Result};
use gpui_kit::component::{dialog::DialogButtonProps, notification::Notification, WindowExt as _};
use gpui_kit::{div, prelude::*, App, Window};
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

/// Rend un nom de fichier proposé par l'expéditeur sûr à écrire sur disque.
///
/// Le nom vient d'un courriel, donc d'un inconnu : il peut contenir des
/// séparateurs de chemin, des caractères de contrôle, ou des caractères de mise
/// en forme bidirectionnelle (U+202E renverse l'affichage, si bien que
/// `facture\u{202E}fdp.exe` s'affiche « factureexe.pdf »). Sous Windows
/// s'ajoutent les caractères interdits, les noms de périphériques réservés et
/// les points/espaces finaux que le système retire en silence (`virus.exe.`
/// y *est* `virus.exe`).
fn sanitize_filename(name: &str) -> String {
    sanitize_filename_for(name, cfg!(windows))
}

/// Coeur testable de [`sanitize_filename`] : `windows` applique les règles du
/// système de fichiers Windows quelle que soit la plateforme de compilation.
fn sanitize_filename_for(name: &str, windows: bool) -> String {
    let mut cleaned: String = name
        .chars()
        .filter(|&character| !is_invisible_or_control(character))
        .map(|character| {
            let forbidden_on_windows = matches!(character, '<' | '>' | '"' | '|' | '?' | '*');
            if matches!(character, '/' | '\\' | ':') || (windows && forbidden_on_windows) {
                '_'
            } else {
                character
            }
        })
        .collect();
    if windows {
        let trimmed_len = cleaned.trim_end_matches(['.', ' ']).len();
        cleaned.truncate(trimmed_len);
        if is_reserved_windows_name(&cleaned) {
            cleaned.insert(0, '_');
        }
    }
    // `..` joint à un dossier en désignerait le parent.
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        tr!("attachment-default-filename").to_string()
    } else {
        cleaned
    }
}

/// Caractères de contrôle C0/C1 et marques de mise en forme bidirectionnelle.
fn is_invisible_or_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{200E}' | '\u{200F}' | '\u{061C}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        )
}

/// `CON`, `NUL.txt`, `com1.tar.gz`… : Windows résout ces noms vers un
/// périphérique, extension comprise, et quelle que soit la casse.
fn is_reserved_windows_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.len() == 4
            && matches!(upper.as_bytes()[3], b'1'..=b'9'))
}

/// Extensions qu'un double-clic exécute, installe, monte ou interprète — tout
/// ce que le gestionnaire système pourrait transformer en exécution de code au
/// lieu d'un simple affichage. Les documents Office à macros (`docm`, `xlsm`…)
/// en font partie : la macro est le vecteur, pas le document.
const EXECUTABLE_EXTENSIONS: &[&str] = &[
    // Windows : exécutables, installeurs, scripts et raccourcis.
    "exe",
    "com",
    "bat",
    "cmd",
    "msi",
    "msp",
    "mst",
    "scr",
    "pif",
    "cpl",
    "hta",
    "js",
    "jse",
    "vbs",
    "vbe",
    "wsf",
    "wsc",
    "wsh",
    "ps1",
    "ps1xml",
    "psc1",
    "psd1",
    "psm1",
    "msc",
    "lnk",
    "url",
    "inf",
    "reg",
    "scf",
    "chm",
    "gadget",
    "application",
    "appref-ms",
    "settingcontent-ms",
    "library-ms",
    "search-ms",
    "appx",
    "appxbundle",
    "msix",
    "msixbundle",
    // Multiplateforme / interprétés.
    "jar",
    "py",
    "pyw",
    "pl",
    // macOS et Linux.
    "app",
    "command",
    "tool",
    "workflow",
    "sh",
    "bash",
    "csh",
    "ksh",
    "zsh",
    "run",
    "bin",
    "appimage",
    "desktop",
    "deb",
    "rpm",
    "snap",
    "flatpakref",
    "pkg",
    "mpkg",
    // Images disque que le système monte d'un double-clic.
    "dmg",
    "iso",
    "img",
    "vhd",
    "vhdx",
    // Office : compléments et documents à macros.
    "xll",
    "xla",
    "xlam",
    "docm",
    "dotm",
    "xlsm",
    "xltm",
    "xlsb",
    "pptm",
    "potm",
    "ppam",
    "ppsm",
    "sldm",
];

/// Types MIME d'exécutables, pour un fichier dont le nom ne dit rien.
const EXECUTABLE_MIMES: &[&str] = &[
    "application/x-msdownload",
    "application/x-dosexec",
    "application/x-executable",
    "application/x-msi",
    "application/x-ms-installer",
    "application/x-sh",
    "application/x-shellscript",
    "application/x-apple-diskimage",
    "application/java-archive",
    "application/hta",
    "application/x-ms-shortcut",
    "application/vnd.microsoft.portable-executable",
];

/// Une pièce jointe qu'Aviary refuse de confier directement au gestionnaire
/// système : l'ouvrir pourrait exécuter du code plutôt que l'afficher.
///
/// L'extension est lue sur le nom **assaini** et privé de ses points/espaces
/// finaux sur toutes les plateformes, `virus.exe.` étant exécuté comme
/// `virus.exe` par Windows.
pub(super) fn is_executable_attachment(filename: &str, mime: &str) -> bool {
    let sanitized = sanitize_filename_for(filename, true);
    let extension = Path::new(&sanitized)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    let mime = mime.split(';').next().unwrap_or(mime).trim();
    extension
        .as_deref()
        .is_some_and(|extension| EXECUTABLE_EXTENSIONS.contains(&extension))
        || EXECUTABLE_MIMES
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(mime))
}

pub(super) fn suggested_filename(attachment: &Attachment) -> String {
    sanitize_filename(&attachment.filename)
}

/// Un courriel joint (`.eml`) : le lecteur l'ouvre dans un onglet plutôt que
/// de le confier au système, qui n'a généralement pas de gestionnaire pour ce
/// type.
pub(super) fn is_email_attachment(attachment: &Attachment) -> bool {
    attachment.mime.eq_ignore_ascii_case("message/rfc822")
        || Path::new(&attachment.filename)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("eml"))
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
        Ok(file) => {
            file.sync_all().with_context(|| {
                tr!("viewer-attachments-file-write-error", {
                    path: path.display().to_string()
                })
            })?;
            drop(file);
            // Windows propage la marque d'une archive aux fichiers extraits.
            #[cfg(windows)]
            write_mark_of_the_web(path);
            Ok(())
        }
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
    #[cfg(windows)]
    write_mark_of_the_web(path);
    Ok(())
}

/// Ouvre une pièce jointe avec le programme système, sauf si elle est
/// exécutable ou scriptable ([`is_executable_attachment`]) : un dialogue
/// explique alors le risque et propose de l'enregistrer à la place.
///
/// C'est le point d'entrée des vues ; [`open`] reste le chemin sans fenêtre et
/// refuse ces mêmes fichiers, sans explication faute de fenêtre où l'afficher.
pub(super) fn open_or_confirm(attachment: Attachment, window: &mut Window, cx: &mut App) {
    if !is_executable_attachment(&attachment.filename, &attachment.mime) {
        open(attachment);
        return;
    }
    let filename = attachment.filename.clone();
    window.open_dialog(cx, move |dialog, _window, _cx| {
        let attachment = attachment.clone();
        dialog
            .title(tr!("attachment-dangerous-title"))
            .button_props(
                DialogButtonProps::default()
                    .show_cancel(true)
                    .ok_text(tr!("viewer-attachment-save-as")),
            )
            .overlay_closable(false)
            .close_button(false)
            .child(div().child(tr!("attachment-dangerous-body", {
                filename: filename.clone()
            })))
            .on_ok(move |_, window, cx| {
                save_with_picker(attachment.clone(), window, cx);
                true
            })
    });
}

/// Demande une destination puis enregistre la pièce jointe hors du thread UI.
fn save_with_picker(attachment: Attachment, window: &mut Window, cx: &mut App) {
    let directory = download_directory();
    let suggested_name = suggested_filename(&attachment);
    let destination = cx.prompt_for_new_path(&directory, Some(&suggested_name));
    let window_handle = window.window_handle();

    cx.spawn(async move |cx| {
        let path = match destination.await {
            Ok(Ok(Some(path))) => path,
            Ok(Ok(None)) | Err(_) => return,
            Ok(Err(error)) => {
                let _ = cx.update_window(window_handle, |_, window, cx| {
                    window.push_notification(
                        Notification::error(tr!("viewer-attachments-picker-error", {
                            error: error
                        })),
                        cx,
                    );
                });
                return;
            }
        };
        let filename = attachment.filename.clone();
        let saved = cx
            .background_executor()
            .spawn(async move { save_as(&path, &attachment) })
            .await;
        let _ = cx.update_window(window_handle, |_, window, cx| match saved {
            Ok(()) => window.push_notification(
                Notification::success(tr!("viewer-attachment-save-success", {
                    filename: filename
                })),
                cx,
            ),
            Err(error) => window.push_notification(
                Notification::error(tr!("viewer-attachment-save-error", { error: error })),
                cx,
            ),
        });
    })
    .detach();
}

/// Writes an attachment to temporary storage and opens it without blocking the UI.
///
/// Refuses executable and scriptable attachments: handing `facture.exe` to the
/// system opener would run it. Views go through [`open_or_confirm`], which
/// explains the refusal and offers to save the file instead.
pub(super) fn open(attachment: Attachment) {
    if is_executable_attachment(&attachment.filename, &attachment.mime) {
        log::warn!(
            "refusing to open an executable attachment with the system handler: {}",
            attachment.filename
        );
        return;
    }
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

/// Pose le Mark-of-the-Web sur un fichier venu d'un courriel : le flux NTFS
/// `Zone.Identifier` classe le fichier en zone Internet (3), ce qui active
/// SmartScreen, le mode protégé d'Office et l'avertissement d'exécution — ce
/// que font navigateurs et clients de messagerie pour tout fichier reçu.
/// Un système de fichiers sans flux alternatifs (FAT, exFAT, partage réseau)
/// refuse l'écriture : on journalise et on continue, le marquage étant une
/// défense de plus et non une condition d'ouverture.
#[cfg(windows)]
fn write_mark_of_the_web(path: &Path) {
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    if let Err(error) = std::fs::write(&stream, b"[ZoneTransfer]\r\nZoneId=3\r\n") {
        log::debug!(
            "could not write Mark-of-the-Web on {}: {error}",
            path.display()
        );
    }
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
    drop(file);
    #[cfg(windows)]
    write_mark_of_the_web(&path);
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
        image_mime_for_path, inferred_mime, is_executable_attachment, sanitize_filename,
        sanitize_filename_for, stage_temporary_attachment, unique_archive_filename, write_zip,
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

    /// Un nom renversé par U+202E afficherait « factureexe.pdf » pour un
    /// exécutable ; contrôles et marques bidi disparaissent sur toutes les
    /// plateformes.
    #[test]
    fn strips_control_and_bidi_formatting_characters() {
        for windows in [false, true] {
            assert_eq!(
                sanitize_filename_for("facture\u{202E}fdp.exe", windows),
                "facturefdp.exe"
            );
            assert_eq!(
                sanitize_filename_for("a\u{0000}b\u{001F}c\u{007F}d\u{0085}e\u{009F}.txt", windows),
                "abcde.txt"
            );
            assert_eq!(
                sanitize_filename_for(
                    "\u{200E}x\u{200F}\u{061C}\u{202A}\u{202B}\u{202C}\u{202D}\u{2066}\u{2067}\u{2068}\u{2069}y.pdf",
                    windows
                ),
                "xy.pdf"
            );
        }
    }

    #[test]
    fn applies_windows_rules_only_when_asked() {
        assert_eq!(
            sanitize_filename_for("a<b>c\"d|e?f*.txt", true),
            "a_b_c_d_e_f_.txt"
        );
        assert_eq!(sanitize_filename_for("a<b>.txt", false), "a<b>.txt");
        assert_eq!(sanitize_filename_for("virus.exe. . ", true), "virus.exe");
        assert_eq!(sanitize_filename_for("notes. ", false), "notes. ");
    }

    #[test]
    fn prefixes_reserved_windows_device_names() {
        for name in [
            "CON",
            "con.txt",
            "Nul.tar.gz",
            "aux",
            "PRN.pdf",
            "COM1",
            "lpt9.doc",
            "CON .txt",
        ] {
            assert_eq!(
                sanitize_filename_for(name, true),
                format!("_{name}"),
                "{name}"
            );
        }
        for name in [
            "CONSOLE.txt",
            "COM0",
            "COM10",
            "LPT",
            "icon.png",
            "com1x.txt",
        ] {
            assert_eq!(sanitize_filename_for(name, true), name, "{name}");
        }
        assert_eq!(sanitize_filename_for("CON", false), "CON");
    }

    /// `..` joint au dossier temporaire en désignerait le parent.
    #[test]
    fn never_yields_a_directory_reference_or_an_empty_name() {
        for windows in [false, true] {
            for name in ["", ".", "..", "\u{202E}"] {
                let sanitized = sanitize_filename_for(name, windows);
                assert!(!sanitized.is_empty());
                assert_ne!(sanitized, ".");
                assert_ne!(sanitized, "..");
            }
        }
        // Sous Windows, « ... » se réduit à rien une fois les points retirés.
        assert!(!sanitize_filename_for("...", true).is_empty());
    }

    #[test]
    fn flags_executable_and_scriptable_attachments() {
        for name in [
            "setup.exe",
            "FACTURE.PDF.EXE",
            "run.bat",
            "script.ps1",
            "lettre.vbs",
            "raccourci.lnk",
            "installer.msi",
            "tool.jar",
            "outil.AppImage",
            "lanceur.desktop",
            "image.iso",
            "disque.dmg",
            "budget.xlsm",
            "rapport.docm",
            "virus.exe.",
            "virus.exe . ",
            "facture\u{202E}fdp.exe",
        ] {
            assert!(
                is_executable_attachment(name, "application/octet-stream"),
                "{name:?} should be flagged"
            );
        }
        for name in [
            "facture.pdf",
            "photo.jpeg",
            "notes.txt",
            "budget.xlsx",
            "archive.zip",
            "sans-extension",
        ] {
            assert!(
                !is_executable_attachment(name, "application/octet-stream"),
                "{name:?} should not be flagged"
            );
        }
    }

    #[test]
    fn flags_executables_by_mime_when_the_name_says_nothing() {
        assert!(is_executable_attachment(
            "document",
            "application/x-msdownload"
        ));
        assert!(is_executable_attachment(
            "document",
            "Application/X-Sh; charset=utf-8"
        ));
        assert!(!is_executable_attachment("document", "application/pdf"));
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
