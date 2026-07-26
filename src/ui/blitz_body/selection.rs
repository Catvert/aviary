//! Rich extraction of Blitz selections for the clipboard.

use super::{PaintState, SelectedContent};
use crate::{model::InlineImage, ui::util};
use blitz_html::HtmlDocument;

fn dom_element_id(doc: &HtmlDocument, mut id: usize) -> Option<usize> {
    loop {
        let node = doc.get_node(id)?;
        if node.is_element() {
            return Some(id);
        }
        id = node.parent?;
    }
}

fn element_chain(doc: &HtmlDocument, id: usize) -> Vec<usize> {
    let Some(mut current) = dom_element_id(doc, id) else {
        return Vec::new();
    };
    let mut chain = Vec::new();
    loop {
        chain.push(current);
        let Some(parent) = doc.get_node(current).and_then(|node| node.parent) else {
            break;
        };
        let Some(parent) = dom_element_id(doc, parent) else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent;
    }
    chain.reverse();
    chain
}

/// Roots of the DOM fragment between both endpoints. The starting and ending
/// elements are included in full to retain their styles and images.
fn selected_fragment_roots(
    doc: &HtmlDocument,
    anchor: usize,
    focus: usize,
) -> Option<(usize, Vec<usize>)> {
    let anchor_chain = element_chain(doc, anchor);
    let focus_chain = element_chain(doc, focus);
    let common_len = anchor_chain
        .iter()
        .zip(&focus_chain)
        .take_while(|(left, right)| left == right)
        .count();
    if common_len == 0 {
        return None;
    }
    let common = anchor_chain[common_len - 1];
    if common_len == anchor_chain.len() || common_len == focus_chain.len() {
        return Some((common, vec![common]));
    }

    let first = anchor_chain[common_len];
    let last = focus_chain[common_len];
    let children = &doc.get_node(common)?.children;
    let first_index = children.iter().position(|id| *id == first)?;
    let last_index = children.iter().position(|id| *id == last)?;
    let (lo, hi) = if first_index <= last_index {
        (first_index, last_index)
    } else {
        (last_index, first_index)
    };
    Some((common, children[lo..=hi].to_vec()))
}

pub(super) fn selected_html(
    doc: &HtmlDocument,
    anchor: usize,
    focus: usize,
) -> Option<(String, Vec<usize>)> {
    let (common, roots) = selected_fragment_roots(doc, anchor, focus)?;
    if roots.len() == 1 && roots[0] == common {
        return Some((doc.get_node(common)?.outer_html(), roots));
    }

    let mut inner = String::new();
    for id in &roots {
        doc.get_node(*id)?.write_outer_html(&mut inner);
    }
    let common_node = doc.get_node(common)?;
    let common_name = common_node
        .element_data()
        .map(|element| element.name.local.as_ref())
        .unwrap_or_default();
    if matches!(common_name, "html" | "body") {
        return Some((inner, roots));
    }

    let outer = common_node.outer_html();
    let open_end = outer.find('>')? + 1;
    let close_start = outer.rfind("</")?;
    let html = format!("{}{}{}", &outer[..open_end], inner, &outer[close_start..]);
    Some((html, roots))
}

/// HTML for the exact text selection. A partial range retains the block tag
/// but replaces its content with only the selected substring.
fn selected_text_html(doc: &HtmlDocument) -> Option<String> {
    let ranges = doc.get_text_selection_ranges();
    if ranges.is_empty() {
        return None;
    }

    let mut html = String::new();
    for (node_id, start, end) in ranges {
        let node = doc.get_node(node_id)?;
        let layout = node.element_data()?.inline_layout_data.as_ref()?;
        let selected = layout.text.get(start..end)?;
        let element_id = dom_element_id(doc, node_id)?;
        let element = doc.get_node(element_id)?;
        if start == 0
            && end == layout.text.len()
            && collect_image_nodes(doc, std::slice::from_ref(&element_id)).is_empty()
        {
            element.write_outer_html(&mut html);
            continue;
        }

        let outer = element.outer_html();
        let Some(open_end) = outer.find('>').map(|index| index + 1) else {
            html.push_str(&util::escape_html_text(selected));
            continue;
        };
        let Some(close_start) = outer.rfind("</") else {
            html.push_str(&util::escape_html_text(selected));
            continue;
        };
        html.push_str(&outer[..open_end]);
        html.push_str(&util::escape_html_text(selected));
        html.push_str(&outer[close_start..]);
    }
    (!html.trim().is_empty()).then_some(html)
}

fn selection_endpoints(doc: &HtmlDocument, state: &PaintState) -> Option<(usize, usize)> {
    if state.rich_dragged {
        return Some((state.rich_anchor?, state.rich_focus?));
    }
    let ranges = doc.get_text_selection_ranges();
    Some((ranges.first()?.0, ranges.last()?.0))
}

fn collect_image_nodes(doc: &HtmlDocument, roots: &[usize]) -> Vec<usize> {
    fn visit(doc: &HtmlDocument, id: usize, images: &mut Vec<usize>) {
        let Some(node) = doc.get_node(id) else {
            return;
        };
        if node
            .element_data()
            .is_some_and(|element| element.name.local.as_ref() == "img")
        {
            images.push(id);
        }
        for child in &node.children {
            visit(doc, *child, images);
        }
    }

    let mut images = Vec::new();
    for root in roots {
        visit(doc, *root, &mut images);
    }
    images
}

pub(super) fn selected_image_nodes(doc: &HtmlDocument, state: &PaintState) -> Vec<usize> {
    let Some((anchor, focus)) = selection_endpoints(doc, state) else {
        return Vec::new();
    };
    let Some((_, roots)) = selected_fragment_roots(doc, anchor, focus) else {
        return Vec::new();
    };
    let mut images = collect_image_nodes(doc, &roots);
    let anchor_element = dom_element_id(doc, anchor);
    let focus_element = dom_element_id(doc, focus);
    let points = state.rich_anchor_point.zip(state.rich_focus_point);
    images.retain(|id| {
        if Some(*id) == anchor_element || Some(*id) == focus_element {
            return true;
        }
        let Some(((anchor_x, anchor_y), (focus_x, focus_y))) = points else {
            return true;
        };
        let Some(node) = doc.get_node(*id) else {
            return false;
        };
        let position = node.absolute_position(0.0, 0.0);
        let center_x = position.x + node.final_layout.size.width / 2.0;
        let center_y = position.y + node.final_layout.size.height / 2.0;
        if (focus_y - anchor_y).abs() <= 8.0 {
            let (left, right) = if anchor_x <= focus_x {
                (anchor_x, focus_x)
            } else {
                (focus_x, anchor_x)
            };
            center_x >= left
                && center_x <= right
                && anchor_y >= position.y - 4.0
                && anchor_y <= position.y + node.final_layout.size.height + 4.0
        } else {
            let (top, bottom) = if anchor_y <= focus_y {
                (anchor_y, focus_y)
            } else {
                (focus_y, anchor_y)
            };
            center_y >= top && center_y <= bottom
        }
    });
    images
}

pub(super) fn selected_content(
    doc: &HtmlDocument,
    state: &PaintState,
    available_images: &[InlineImage],
) -> Option<SelectedContent> {
    let selected_images = selected_image_nodes(doc, state);
    let html = if selected_images.is_empty() {
        selected_text_html(doc).or_else(|| {
            let (anchor, focus) = selection_endpoints(doc, state)?;
            selected_html(doc, anchor, focus).map(|(html, _)| html)
        })?
    } else {
        let (anchor, focus) = selection_endpoints(doc, state)?;
        selected_html(doc, anchor, focus)?.0
    };
    if html.trim().is_empty() {
        return None;
    }
    let text = doc
        .get_selected_text()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| html.clone());
    let images = crate::blocks::referenced_inline_images(&html, available_images);
    Some(SelectedContent { text, html, images })
}
