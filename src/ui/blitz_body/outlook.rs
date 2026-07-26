//! Targeted repairs for HTML produced by Exchange/Outlook and common mailing
//! platforms.

fn has_visible_html_text(element: scraper::ElementRef<'_>) -> bool {
    element.text().any(|text| {
        text.chars()
            .any(|character| !character.is_whitespace() && character != '\u{feff}')
    })
}

fn is_inline_attachment_marker(element: scraper::ElementRef<'_>) -> bool {
    if element.value().name() != "div" || element.child_elements().next().is_some() {
        return false;
    }
    let text = element.text().collect::<String>();
    let Some(filename) = text
        .trim()
        .strip_prefix('<')
        .and_then(|text| text.strip_suffix('>'))
    else {
        return false;
    };
    if filename.is_empty() || filename.contains(['<', '>', '/', '\\']) {
        return false;
    }
    let Some((stem, extension)) = filename.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && matches!(
            extension.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff"
        )
}

/// Repairs Outlook layout tables before Blitz computes their flow.
///
/// Blitz currently lays out the table itself at the right height, but a plain
/// block parent does not always include that height in its own flow. Content
/// after the wrapper can consequently paint over a signature image. A wrapper
/// with no attributes carries no styling or semantic information, so hoisting
/// its children preserves the email while avoiding that engine limitation.
///
/// Outlook also emits tiny fixed heights on cells containing large images.
/// Browsers treat those heights as minimums, whereas the current table layout
/// can treat them as exact values. Such a height is removed only when an image
/// in that cell is explicitly taller.
///
/// The legacy `height` attribute has the same minimum-height semantics on a
/// table. Materialize numeric values as `min-height` so a banner can make its
/// row taller instead of overflowing underneath the following section.
fn repair_table_layout(html: &str) -> String {
    if !html.contains("<table") {
        return html.to_string();
    }

    let mut document = scraper::Html::parse_document(html);
    let div_selector = scraper::Selector::parse("div").expect("div selector");
    let cell_selector = scraper::Selector::parse("td[style], th[style]").expect("cell selector");
    let image_selector = scraper::Selector::parse("img").expect("image selector");
    let table_selector = scraper::Selector::parse("table[height]").expect("table selector");
    let table_styles = document
        .select(&table_selector)
        .filter_map(|table| {
            let style = table.attr("style").unwrap_or_default().trim();
            if has_css_property(style, "height") || has_css_property(style, "min-height") {
                return None;
            }
            let height = table.attr("height").and_then(html_length_px)?;
            let separator = (!style.is_empty() && !style.ends_with(';')).then_some(";");
            Some((
                table.id(),
                format!(
                    "{}{}height:auto!important;min-height:{height}px",
                    style,
                    separator.unwrap_or_default()
                ),
            ))
        })
        .collect::<Vec<_>>();
    let cell_styles = document
        .select(&cell_selector)
        .filter_map(|cell| {
            let style = cell.attr("style")?;
            let cell_height = css_height_px(style)?;
            let image_height = cell
                .select(&image_selector)
                .filter_map(|image| {
                    image
                        .attr("style")
                        .and_then(css_height_px)
                        .or_else(|| image.attr("height").and_then(html_length_px))
                })
                .fold(0.0_f32, f32::max);
            (image_height > cell_height).then(|| (cell.id(), without_css_height(style)))
        })
        .collect::<Vec<_>>();
    let wrappers = document
        .select(&div_selector)
        .filter(|div| {
            div.value().attrs().next().is_none()
                && div
                    .children()
                    .filter_map(scraper::ElementRef::wrap)
                    .any(|child| child.value().name() == "table")
        })
        .map(|div| div.id())
        .collect::<Vec<_>>();

    if table_styles.is_empty() && cell_styles.is_empty() && wrappers.is_empty() {
        return html.to_string();
    }

    for (table_id, style) in table_styles {
        let mut node = document
            .tree
            .get_mut(table_id)
            .expect("table still present");
        let scraper::Node::Element(table) = node.value() else {
            continue;
        };
        if let Some((_, value)) = table
            .attrs
            .iter_mut()
            .find(|(name, _)| name.local.as_ref() == "style")
        {
            value.clear();
            value.push_slice(&style);
        } else if let Some((height_name, _)) = table
            .attrs
            .iter()
            .find(|(name, _)| name.local.as_ref() == "height")
        {
            let mut style_name = height_name.clone();
            style_name.local = "style".into();
            table.attrs.push((style_name, style.into()));
        }
    }

    for (cell_id, style) in cell_styles {
        let mut node = document
            .tree
            .get_mut(cell_id)
            .expect("table cell still present");
        let scraper::Node::Element(cell) = node.value() else {
            continue;
        };
        let Some((_, value)) = cell
            .attrs
            .iter_mut()
            .find(|(name, _)| name.local.as_ref() == "style")
        else {
            continue;
        };
        value.clear();
        value.push_slice(&style);
    }

    for wrapper_id in wrappers {
        let children = document
            .tree
            .get(wrapper_id)
            .expect("table wrapper still present")
            .children()
            .map(|child| child.id())
            .collect::<Vec<_>>();
        for child_id in children {
            document
                .tree
                .get_mut(wrapper_id)
                .expect("table wrapper still present")
                .insert_id_before(child_id);
        }
        document
            .tree
            .get_mut(wrapper_id)
            .expect("table wrapper still present")
            .detach();
    }

    document.html()
}

fn css_height_px(style: &str) -> Option<f32> {
    style.split(';').find_map(|declaration| {
        let (property, value) = declaration.split_once(':')?;
        property
            .trim()
            .eq_ignore_ascii_case("height")
            .then(|| html_length_px(value.trim().trim_end_matches("!important").trim()))
            .flatten()
    })
}

fn html_length_px(value: &str) -> Option<f32> {
    let value = value.trim();
    let split = value
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(value.len());
    let number = value[..split].parse::<f32>().ok()?;
    let unit = value[split..].trim().to_ascii_lowercase();
    match unit.as_str() {
        "" | "px" => Some(number),
        "pt" => Some(number * 96.0 / 72.0),
        "in" => Some(number * 96.0),
        "cm" => Some(number * 96.0 / 2.54),
        "mm" => Some(number * 96.0 / 25.4),
        _ => None,
    }
}

fn without_css_height(style: &str) -> String {
    style
        .split(';')
        .filter(|declaration| {
            declaration
                .split_once(':')
                .is_none_or(|(property, _)| !property.trim().eq_ignore_ascii_case("height"))
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// Dynamics 365 can leave structural personalization fragments HTML-escaped
/// inside `msdynmkt_personalization` spans. Browsers correctly display that as
/// text, but the fragments are clearly the missing opening/closing rows around
/// the adjacent real footer table. Decode only spans which start with a known
/// table tag; ordinary personalized values remain untouched.
fn repair_dynamics_personalization(html: &str) -> String {
    if !html.contains("msdynmkt_personalization") {
        return html.to_string();
    }

    let document = scraper::Html::parse_document(html);
    let selector = scraper::Selector::parse("span.msdynmkt_personalization")
        .expect("Dynamics personalization selector");
    let replacements = document
        .select(&selector)
        .filter_map(|span| {
            let decoded = span.text().collect::<String>();
            is_encoded_table_fragment(&decoded).then(|| (span.html(), decoded))
        })
        .collect::<Vec<_>>();
    if replacements.is_empty() {
        return html.to_string();
    }

    // Work from html5ever's serialization so `ElementRef::html()` is an exact
    // substring even when the original used unquoted or oddly-cased attrs.
    let mut repaired = document.html();
    for (encoded_span, decoded) in replacements {
        if let Some(start) = repaired.find(&encoded_span) {
            repaired.replace_range(start..start + encoded_span.len(), &decoded);
        }
    }
    repaired
}

fn is_encoded_table_fragment(value: &str) -> bool {
    let value = value.trim_start().to_ascii_lowercase();
    [
        "<table", "</table", "<tbody", "</tbody", "<thead", "</thead", "<tfoot", "</tfoot", "<tr",
        "</tr", "<td", "</td", "<th", "</th",
    ]
    .iter()
    .any(|tag| value.starts_with(tag))
}

/// Removes invisible tracking images before the network provider sees them.
/// Apart from protecting privacy, this prevents an unreachable beacon from
/// consuming the renderer's complete remote-resource timeout. Visible icons
/// and spacers larger than one pixel are retained.
fn remove_tracking_images(html: &str) -> String {
    if !html.contains("<img") {
        return html.to_string();
    }

    let mut document = scraper::Html::parse_document(html);
    let selector = scraper::Selector::parse("img").expect("image selector");
    let tracking = document
        .select(&selector)
        .filter(|image| {
            if image.attr("data-tracking").is_some() {
                return true;
            }
            let width = image.attr("width").and_then(html_length_px);
            let height = image.attr("height").and_then(html_length_px);
            matches!((width, height), (Some(width), Some(height)) if width <= 1.0 && height <= 1.0)
        })
        .map(|image| image.id())
        .collect::<Vec<_>>();
    if tracking.is_empty() {
        return html.to_string();
    }

    for image_id in tracking {
        document
            .tree
            .get_mut(image_id)
            .expect("tracking image still present")
            .detach();
    }
    document.html()
}

/// Materializes legacy image dimensions as CSS when the sender did not
/// already provide the corresponding inline declaration.
///
/// Blitz normally reads `width`/`height` attributes while measuring replaced
/// elements. A block-level image, however, receives the table cell's known
/// width before that fallback runs and is stretched across the cell. Browsers
/// still honor the legacy dimension in that case; email footers commonly rely
/// on this for small social icons (`display:block; width="…"`).
fn repair_block_image_dimensions(html: &str) -> String {
    if !html.contains("<img") || (!html.contains("width=") && !html.contains("height=")) {
        return html.to_string();
    }

    let mut document = scraper::Html::parse_document(html);
    let selector = scraper::Selector::parse("img[width], img[height]").expect("image selector");
    let repairs = document
        .select(&selector)
        .filter_map(|image| {
            let mut style = image.attr("style").unwrap_or_default().trim().to_string();
            if !css_property_equals(&style, "display", "block") {
                return None;
            }
            let width = (!has_css_property(&style, "width"))
                .then(|| image.attr("width").and_then(html_image_dimension_css))
                .flatten();
            let height = (!has_css_property(&style, "height"))
                .then(|| image.attr("height").and_then(html_image_dimension_css))
                .flatten();
            if width.is_none() && height.is_none() {
                return None;
            }

            if !style.is_empty() && !style.ends_with(';') {
                style.push(';');
            }
            if let Some(width) = width {
                style.push_str("width:");
                style.push_str(&width);
                style.push(';');
            }
            if let Some(height) = height {
                style.push_str("height:");
                style.push_str(&height);
                style.push(';');
            }

            Some((image.id(), style))
        })
        .collect::<Vec<_>>();
    if repairs.is_empty() {
        return html.to_string();
    }

    for (image_id, style) in repairs {
        let mut node = document
            .tree
            .get_mut(image_id)
            .expect("image still present");
        let scraper::Node::Element(image) = node.value() else {
            continue;
        };
        let (_, value) = image
            .attrs
            .iter_mut()
            .find(|(name, _)| name.local.as_ref() == "style")
            .expect("block image style still present");
        value.clear();
        value.push_slice(&style);
    }

    document.html()
}

/// Removes the extreme fluid-column `calc()` hack emitted by some email
/// builders when a normal width fallback precedes it in the same style.
///
/// A declaration such as `width:50%; width:calc(230400px - 48000%)` is meant
/// to exploit differences between legacy mail clients. Browsers retain a
/// usable column through their table/min-width rules, while Blitz resolves the
/// enormous negative result to zero and collapses the complete feature card.
/// Regular calculations such as `calc(100% - 20px)` are left untouched.
fn repair_extreme_calc_widths(html: &str) -> String {
    if !html.contains("calc(") {
        return html.to_string();
    }

    let mut document = scraper::Html::parse_document(html);
    let selector = scraper::Selector::parse("[style]").expect("style selector");
    let repairs = document
        .select(&selector)
        .filter_map(|element| {
            let style = element.attr("style")?;
            let has_width_fallback = style.split(';').any(|declaration| {
                css_declaration(declaration).is_some_and(|(property, value)| {
                    property.eq_ignore_ascii_case("width")
                        && !value.to_ascii_lowercase().contains("calc(")
                })
            });
            if !has_width_fallback {
                return None;
            }

            let repaired = style
                .split(';')
                .filter(|declaration| {
                    !css_declaration(declaration).is_some_and(|(property, value)| {
                        property.eq_ignore_ascii_case("width") && has_extreme_calc_percentage(value)
                    })
                })
                .collect::<Vec<_>>()
                .join(";");
            (repaired != style).then(|| (element.id(), repaired))
        })
        .collect::<Vec<_>>();
    if repairs.is_empty() {
        return html.to_string();
    }

    for (element_id, style) in repairs {
        let mut node = document
            .tree
            .get_mut(element_id)
            .expect("styled element still present");
        let scraper::Node::Element(element) = node.value() else {
            continue;
        };
        let (_, value) = element
            .attrs
            .iter_mut()
            .find(|(name, _)| name.local.as_ref() == "style")
            .expect("style still present");
        value.clear();
        value.push_slice(&style);
    }

    document.html()
}

/// Restores table-cell display for hybrid email columns.
///
/// Email builders often assign `display:inline-block` to `<th>` elements so
/// media queries can stack them on narrow clients. Blitz does not yet perform
/// the anonymous-table-box fixup browsers apply here, so the cell and all of
/// its descendants receive a zero-sized layout. An inline `table-cell`
/// declaration preserves the intended desktop column grid and overrides the
/// sender's low-specificity `.columns` rule.
fn repair_inline_block_table_cells(html: &str) -> String {
    if !html.contains("columns") && !html.contains("inline-block") {
        return html.to_string();
    }

    let mut document = scraper::Html::parse_document(html);
    let selector = scraper::Selector::parse("th[style], td[style]").expect("table cell selector");
    let repairs = document
        .select(&selector)
        .filter_map(|cell| {
            let style = cell.attr("style")?;
            let is_hybrid_column = cell.attr("class").is_some_and(|classes| {
                classes
                    .split_ascii_whitespace()
                    .any(|class| class == "columns")
            }) || css_property_equals(style, "display", "inline-block");
            if !is_hybrid_column {
                return None;
            }

            let mut replaced_display = false;
            let mut declarations = style
                .split(';')
                .filter(|declaration| !declaration.trim().is_empty())
                .map(|declaration| {
                    if css_declaration(declaration)
                        .is_some_and(|(property, _)| property.eq_ignore_ascii_case("display"))
                    {
                        replaced_display = true;
                        "display:table-cell".to_string()
                    } else {
                        declaration.to_string()
                    }
                })
                .collect::<Vec<_>>();
            if !replaced_display {
                declarations.push("display:table-cell".into());
            }
            Some((cell.id(), declarations.join(";")))
        })
        .collect::<Vec<_>>();
    if repairs.is_empty() {
        return html.to_string();
    }

    for (cell_id, style) in repairs {
        let mut node = document
            .tree
            .get_mut(cell_id)
            .expect("table cell still present");
        let scraper::Node::Element(cell) = node.value() else {
            continue;
        };
        let (_, value) = cell
            .attrs
            .iter_mut()
            .find(|(name, _)| name.local.as_ref() == "style")
            .expect("table cell style still present");
        value.clear();
        value.push_slice(&style);
    }

    document.html()
}

fn css_declaration(declaration: &str) -> Option<(&str, &str)> {
    let (property, value) = declaration.split_once(':')?;
    Some((property.trim(), value.trim()))
}

fn has_extreme_calc_percentage(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    if !value.contains("calc(") {
        return false;
    }
    value.match_indices('%').any(|(percent, _)| {
        let number = value[..percent]
            .trim_end()
            .rsplit_once(|character: char| !character.is_ascii_digit() && character != '.')
            .map_or(value[..percent].trim(), |(_, number)| number);
        number
            .parse::<f32>()
            .is_ok_and(|percentage| percentage > 1000.0)
    })
}

fn has_css_property(style: &str, expected: &str) -> bool {
    style.split(';').any(|declaration| {
        declaration
            .split_once(':')
            .is_some_and(|(property, _)| property.trim().eq_ignore_ascii_case(expected))
    })
}

fn css_property_equals(style: &str, expected: &str, expected_value: &str) -> bool {
    style.split(';').any(|declaration| {
        declaration
            .split_once(':')
            .is_some_and(|(property, value)| {
                property.trim().eq_ignore_ascii_case(expected)
                    && value
                        .trim()
                        .strip_suffix("!important")
                        .unwrap_or(value.trim())
                        .trim()
                        .eq_ignore_ascii_case(expected_value)
            })
    })
}

fn html_image_dimension_css(value: &str) -> Option<String> {
    let value = value.trim();
    if let Some(percent) = value.strip_suffix('%') {
        let percent = percent.trim().parse::<f32>().ok()?;
        return (percent >= 0.0).then(|| format!("{percent}%"));
    }
    let pixels = html_length_px(value)?;
    (pixels >= 0.0).then(|| format!("{pixels}px"))
}

/// Applies the targeted email-client repairs needed before Blitz parses the
/// body.
pub(crate) fn repair_outlook_html(html: &str) -> String {
    let html = repair_dynamics_personalization(html);
    let html = remove_tracking_images(&html);
    let html = repair_block_image_dimensions(&html);
    let html = repair_extreme_calc_widths(&html);
    let html = repair_inline_block_table_cells(&html);
    let html = repair_table_layout(&html);
    repair_fragmented_outlook_cids(&html)
}

/// Repairs HTML bodies produced by Exchange when a message forwarded from
/// Apple Mail is split around its inline images.
///
/// The pattern deliberately remains strict: at least two pairs are required,
/// the image must be the only element of its `div` directly under the body,
/// and the preceding fragment must be an Outlook textbox or quote.
pub(crate) fn repair_fragmented_outlook_cids(html: &str) -> String {
    if !html.contains("cid:") || !html.contains("role=\"textbox\"") {
        return html.to_string();
    }

    let mut document = scraper::Html::parse_document(html);
    let image_selector =
        scraper::Selector::parse(r#"img[src^="cid:"]"#).expect("fragmented CID image selector");
    let placeholder_selector = scraper::Selector::parse(r#"a, div[style*="overflow"]"#)
        .expect("fragmented CID placeholder selector");
    let citation_selector =
        scraper::Selector::parse(r#"blockquote[type="cite"]"#).expect("citation selector");

    let mut moves = Vec::new();
    for image in document.select(&image_selector) {
        let Some(wrapper_node) = image.parent() else {
            continue;
        };
        let Some(wrapper) = scraper::ElementRef::wrap(wrapper_node) else {
            continue;
        };
        if wrapper.value().name() != "div" || has_visible_html_text(wrapper) {
            continue;
        }
        let mut wrapper_children = wrapper.child_elements();
        if wrapper_children.next().map(|child| child.id()) != Some(image.id())
            || wrapper_children.next().is_some()
        {
            continue;
        }
        let Some(wrapper_parent) = wrapper.parent().and_then(scraper::ElementRef::wrap) else {
            continue;
        };
        if wrapper_parent.value().name() != "body" {
            continue;
        }

        let mut sibling = wrapper.prev_sibling();
        let previous = loop {
            let Some(node) = sibling else {
                break None;
            };
            if let Some(element) = scraper::ElementRef::wrap(node) {
                break Some(element);
            }
            sibling = node.prev_sibling();
        };
        let Some(previous) = previous else {
            continue;
        };
        let is_textbox = previous
            .attr("role")
            .is_some_and(|role| role.eq_ignore_ascii_case("textbox"));
        if previous.value().name() != "div"
            || (!is_textbox && previous.select(&citation_selector).next().is_none())
        {
            continue;
        }

        let Some(target) = previous
            .select(&placeholder_selector)
            .filter(|candidate| {
                !has_visible_html_text(*candidate) && candidate.child_elements().next().is_none()
            })
            .last()
        else {
            continue;
        };
        let Some(target_parent) = target.parent() else {
            continue;
        };
        moves.push((
            image.id(),
            wrapper.id(),
            target.id(),
            target_parent.id(),
            previous.id(),
            target.value().name() == "a",
            has_visible_html_text(previous),
        ));
    }

    // An isolated pair may be an intentional layout choice.
    if moves.len() < 2 {
        return html.to_string();
    }

    let mut footer_host = None;
    for (
        image_id,
        wrapper_id,
        target_id,
        target_parent_id,
        previous_id,
        target_is_anchor,
        previous_has_text,
    ) in moves
    {
        document
            .tree
            .get_mut(target_id)
            .expect("CID target still present")
            .append_id(image_id);
        document
            .tree
            .get_mut(wrapper_id)
            .expect("CID wrapper still present")
            .detach();

        if !target_is_anchor {
            continue;
        }
        if previous_has_text {
            footer_host = Some(target_parent_id);
        } else if let Some(host_id) = footer_host {
            document
                .tree
                .get_mut(host_id)
                .expect("footer host still present")
                .append_id(target_id);
            document
                .tree
                .get_mut(previous_id)
                .expect("continuation fragment still present")
                .detach();
        }
    }

    // The final fragment, after the last image, is not part of any
    // pair. Remove its empty table and `<image-name.png>` markers.
    let textbox_selector =
        scraper::Selector::parse(r#"div[role="textbox"]"#).expect("textbox selector");
    let table_selector = scraper::Selector::parse("table").expect("table selector");
    let div_selector = scraper::Selector::parse("div").expect("div selector");
    let mut empty_tables = Vec::new();
    let mut attachment_markers = Vec::new();
    for textbox in document.select(&textbox_selector) {
        let markers = textbox
            .select(&div_selector)
            .filter(|child| is_inline_attachment_marker(*child))
            .map(|child| child.id())
            .collect::<Vec<_>>();
        if markers.len() >= 2 {
            attachment_markers.extend(markers);
        }
        if textbox.select(&image_selector).next().is_some() {
            continue;
        }
        empty_tables.extend(
            textbox
                .select(&table_selector)
                .filter(|table| !has_visible_html_text(*table))
                .map(|table| table.id()),
        );
    }
    let empty_roots = empty_tables
        .iter()
        .copied()
        .filter(|table_id| {
            let mut ancestor = document.tree.get(*table_id).and_then(|node| node.parent());
            while let Some(node) = ancestor {
                if empty_tables.contains(&node.id()) {
                    return false;
                }
                ancestor = node.parent();
            }
            true
        })
        .collect::<Vec<_>>();
    for table_id in empty_roots {
        document
            .tree
            .get_mut(table_id)
            .expect("empty table still present")
            .detach();
    }
    for marker_id in attachment_markers {
        document
            .tree
            .get_mut(marker_id)
            .expect("attachment marker still present")
            .detach();
    }

    document.html()
}
