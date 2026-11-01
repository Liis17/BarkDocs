//! Нарезка markdown на чанки по заголовкам.

const MAX_CHUNK_CHARS: usize = 6000;

#[derive(Debug, PartialEq)]
pub struct Chunk {
    pub heading: String,
    pub content: String,
}

fn strip_front_matter(md: &str) -> &str {
    let Some(rest) = md.strip_prefix("---\n") else {
        return md;
    };
    match rest.find("\n---") {
        Some(end) => rest[end + 4..].trim_start_matches(['\r', '\n']),
        None => md,
    }
}

/// Уровень заголовка `#`..`###`; более мелкие остаются внутри раздела.
fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.bytes().take_while(|b| *b == b'#').count();
    if !(1..=3).contains(&level) {
        return None;
    }
    let title = line[level..]
        .strip_prefix(' ')?
        .trim()
        .trim_end_matches('#')
        .trim();
    (!title.is_empty()).then_some((level, title))
}

/// Заменяет `[текст](ссылка)` на `текст`: сайты оборачивают заголовки в якоря,
/// а адреса не должны попадать в заголовок и его поисковый вес.
fn strip_links(title: &str) -> String {
    let mut result = String::new();
    let mut rest = title;
    while let Some(open) = rest.find('[') {
        let parsed = rest[open..].split_once("](").and_then(|(text, tail)| {
            let (_, after) = tail.split_once(')')?;
            (!text[1..].contains('[')).then_some((&text[1..], after))
        });
        match parsed {
            Some((text, after)) => {
                result.push_str(&rest[..open]);
                result.push_str(text);
                rest = after;
            }
            None => {
                result.push_str(&rest[..=open]);
                rest = &rest[open + 1..];
            }
        }
    }
    result.push_str(rest);
    result.trim().to_string()
}

fn split_long(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0;
    for line in text.split_inclusive('\n') {
        let line_chars = line.chars().count();
        if current_chars + line_chars > MAX_CHUNK_CHARS && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        if line_chars > MAX_CHUNK_CHARS {
            let chars: Vec<char> = line.chars().collect();
            parts.extend(
                chars
                    .chunks(MAX_CHUNK_CHARS)
                    .map(|c| c.iter().collect::<String>()),
            );
        } else {
            current.push_str(line);
            current_chars += line_chars;
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

fn flush(section: &mut String, trail: &[(usize, String)], chunks: &mut Vec<Chunk>) {
    let text = section.trim();
    if !text.is_empty() {
        let title = trail
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join(" > ");
        for content in split_long(text) {
            chunks.push(Chunk {
                heading: title.clone(),
                content,
            });
        }
    }
    section.clear();
}

pub fn chunk_markdown(markdown: &str) -> Vec<Chunk> {
    // Postgres не принимает NUL в TEXT.
    let cleaned = markdown.replace('\0', "").replace("\r\n", "\n");
    let body = strip_front_matter(&cleaned);

    let mut chunks = Vec::new();
    let mut trail: Vec<(usize, String)> = Vec::new();
    let mut section = String::new();
    let mut fence: Option<&str> = None;

    for line in body.lines() {
        let trimmed = line.trim_start();
        match fence {
            Some(marker) if trimmed.starts_with(marker) => fence = None,
            None if trimmed.starts_with("```") => fence = Some("```"),
            None if trimmed.starts_with("~~~") => fence = Some("~~~"),
            _ => {}
        }
        if fence.is_none()
            && let Some((level, title)) = heading(line)
        {
            flush(&mut section, &trail, &mut chunks);
            trail.retain(|(l, _)| *l < level);
            trail.push((level, strip_links(title)));
            continue;
        }
        section.push_str(line);
        section.push('\n');
    }
    flush(&mut section, &trail, &mut chunks);
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_by_headings_and_keeps_trail() {
        let md = "intro\n# Guide\ntext a\n## Setup\ntext b\n### Deep\ntext c\n## Next\ntext d\n";
        let headings: Vec<_> = chunk_markdown(md).into_iter().map(|c| c.heading).collect();
        assert_eq!(
            headings,
            [
                "",
                "Guide",
                "Guide > Setup",
                "Guide > Setup > Deep",
                "Guide > Next"
            ]
        );
    }

    #[test]
    fn ignores_headings_inside_code_fences() {
        let md = "# Title\n```bash\n# not a heading\n```\nafter\n";
        let chunks = chunk_markdown(md);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.contains("# not a heading"));
        assert!(chunks[0].content.contains("after"));
    }

    #[test]
    fn tilde_fence_is_not_closed_by_backticks() {
        let md = "# T\n~~~\n```\n# inside\n~~~\n";
        assert_eq!(chunk_markdown(md).len(), 1);
    }

    #[test]
    fn strips_front_matter_and_nul_bytes() {
        let chunks = chunk_markdown("---\ntitle: x\n---\n# H\nbo\0dy\n");
        assert_eq!(chunks[0].heading, "H");
        assert_eq!(chunks[0].content, "body");
    }

    #[test]
    fn hash_without_space_is_not_a_heading() {
        let chunks = chunk_markdown("#hashtag\ntext\n");
        assert_eq!(chunks[0].heading, "");
    }

    #[test]
    fn splits_oversized_sections_on_char_boundaries() {
        let long_line = "я".repeat(MAX_CHUNK_CHARS * 2 + 5);
        let md = format!("# H\nshort\n{long_line}\nend\n");
        let chunks = chunk_markdown(&md);
        assert!(chunks.len() >= 3);
        assert!(
            chunks
                .iter()
                .all(|c| c.content.chars().count() <= MAX_CHUNK_CHARS)
        );
        assert!(chunks.iter().all(|c| c.heading == "H"));
    }

    #[test]
    fn removes_links_from_headings() {
        assert_eq!(
            strip_links("[](#field-attrs)Field attributes"),
            "Field attributes"
        );
        assert_eq!(
            strip_links("[Python versions](#python-versions)"),
            "Python versions"
        );
        assert_eq!(strip_links("Use [a](x) and [b](y) now"), "Use a and b now");
        assert_eq!(strip_links("Array [T; 3] syntax"), "Array [T; 3] syntax");
        assert_eq!(strip_links("[unclosed](link"), "[unclosed](link");
        assert_eq!(strip_links("[x] then [y](z)"), "[x] then y");
    }

    #[test]
    fn empty_input_gives_no_chunks() {
        assert!(chunk_markdown("  \n\n").is_empty());
    }
}
