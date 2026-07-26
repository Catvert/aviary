//! Document model for email bodies.
//!
//! Owns the `Block`/`BlockKind` document model and the markdown/HTML
//! (de)serialization used by the compose pipeline, signatures and templates.
//! The gpui block editor (`ui/block_editor.rs`) edits this model directly;
//! `markdown_to_blocks` / `blocks_to_markdown` convert at the boundaries
//! (drafts, signatures, templates) and `build_html_body` remains the single
//! producer of the HTML sent via `Cmd::SendMail`.
//!
//! `html_to_blocks` is the return path: a draft comes back from the provider
//! as HTML, and rebuilding the document from the markup — rather than from
//! its Markdown conversion — is what keeps its structure, its signature and
//! its quoted original intact when it is reopened.

mod html;
mod markdown;
mod model;

/// Metrics shared by the editable block view and its generated HTML.
pub(crate) const COMPOSE_BODY_FONT_SIZE: f32 = 14.0;
pub(crate) const COMPOSE_BODY_LINE_HEIGHT: f32 = 20.0;
pub(crate) const COMPOSE_LIST_INDENT: f32 = 24.0;

pub(crate) use html::{html_text, html_to_blocks, HtmlImport};
pub(crate) use markdown::{
    blocks_to_markdown, build_html_body, markdown_to_blocks, referenced_inline_images,
};
pub(crate) use model::{Block, BlockKind, ListItem, TEMPLATE_CURSOR_PLACEHOLDER};
