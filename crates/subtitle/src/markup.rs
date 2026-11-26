//! WebVTT cue 正文里的标签与字符引用（W3C WebVTT 4.2.2）换成 SRT 的写法或纯文字。

/// 换成 SRT 正文：粗体、斜体、下划线保留为 `<b>`、`<i>`、`<u>`（SRT 播放器普遍认得），其余标签（类、说话人、语言、注音、
/// 时间戳）去掉、保留其中的文字；字符引用换成字符。
pub fn srt_text(text: &str) -> String {
    convert(text, true)
}

/// 换成纯文字：标签都去掉，字符引用换成字符。
pub fn plain_text(text: &str) -> String {
    convert(text, false)
}

fn convert(text: &str, keep_style: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(['<', '&']) {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if rest.starts_with('<') {
            let Some(end) = rest.find('>') else {
                // 没有闭合的 `<`：按 WebVTT 的规则一直到结尾都是标签
                rest = "";
                break;
            };
            let tag = &rest[1..end];
            rest = &rest[end + 1..];
            let (closing, name) = match tag.strip_prefix('/') {
                Some(name) => (true, name),
                None => (false, tag),
            };
            let name = name
                .split(['.', ' ', '\t', '\n'])
                .next()
                .expect("split 至少给出一段");
            if keep_style && matches!(name, "b" | "i" | "u") {
                out.push('<');
                if closing {
                    out.push('/');
                }
                out.push_str(name);
                out.push('>');
            }
        } else {
            let (decoded, len) = reference(rest);
            out.push_str(decoded);
            rest = &rest[len..];
        }
    }
    out.push_str(rest);
    out
}

/// 开头的字符引用（`rest` 以 `&` 开头）：换出的文字与它占的字节数；认不出的原样保留 `&`。
fn reference(rest: &str) -> (&'static str, usize) {
    const NAMED: [(&str, &str); 6] = [
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&nbsp;", "\u{A0}"),
        ("&lrm;", "\u{200E}"),
        ("&rlm;", "\u{200F}"),
    ];
    NAMED
        .iter()
        .find(|(name, _)| rest.starts_with(name))
        .map_or(("&", 1), |&(name, text)| (text, name.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styling_is_kept_for_srt_and_dropped_for_plain_text() {
        let text = "<v Roger Bingham><b>粗</b> <c.yellow.bg_blue>色</c> <i.x>斜</i>\n\
                    <ruby>漢<rt>かん</rt></ruby> <00:01.500>a &amp; b &lt;c&gt; & d";
        assert_eq!(
            srt_text(text),
            "<b>粗</b> 色 <i>斜</i>\n漢かん a & b <c> & d"
        );
        assert_eq!(plain_text(text), "粗 色 斜\n漢かん a & b <c> & d");
        assert_eq!(plain_text("未闭合 <b"), "未闭合 ");
    }
}
