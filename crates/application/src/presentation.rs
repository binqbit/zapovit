use domain::Block;

/// Split by Telegram's UTF-16 length limit without changing any scalar value.
pub fn split_text(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut units = 0;
    for ch in text.chars() {
        let n = ch.len_utf16();
        if units + n > limit && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            units = 0;
        }
        current.push(ch);
        units += n;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}
pub fn delivery_blocks(blocks: Vec<Block>) -> Vec<Block> {
    let mut out = Vec::new();
    for mut block in blocks {
        match &mut block {
            Block::Text { text } => out.extend(
                split_text(text, 3500)
                    .into_iter()
                    .map(|text| Block::Text { text }),
            ),
            Block::Copyable { text } => out.extend(
                split_text(text, 3500)
                    .into_iter()
                    .map(|text| Block::Copyable { text }),
            ),
            Block::Spoiler { text } => out.extend(
                split_text(text, 3500)
                    .into_iter()
                    .map(|text| Block::Spoiler { text }),
            ),
            Block::File {
                file,
                name,
                caption,
            } => {
                let mut captions = split_text(caption, 1024).into_iter();
                let first = captions.next().unwrap_or_default();
                out.push(Block::File {
                    file: file.clone(),
                    name: std::mem::take(name),
                    caption: first,
                });
                out.extend(captions.map(|text| Block::Text { text }));
            }
        }
    }
    out
}
#[cfg(test)]
mod tests {
    #[test]
    fn utf16_splitting_preserves_exact_content() {
        let text = "💙e\u{301}\n".repeat(2000);
        let parts = super::split_text(&text, 3500);
        assert!(parts.iter().all(|p| p.encode_utf16().count() <= 3500));
        assert_eq!(parts.concat(), text);
    }
}
