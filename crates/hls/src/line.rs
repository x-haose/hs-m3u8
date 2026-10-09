//! 行与属性列表的词法解析。格式上的不规范在这一层容忍：BOM、CRLF、首尾空白、属性名大小写、值不加引号。

use crate::SyntaxError;

/// 播放列表中的一行有效内容；`number` 为原文行号，从 1 开始。
pub(crate) struct Line<'a> {
    pub number: usize,
    pub kind: LineKind<'a>,
}

pub(crate) enum LineKind<'a> {
    /// `#EXT...` 标签；`name` 为大写的标签名（不含 `#`），`value` 为第一个冒号之后的内容
    Tag {
        name: String,
        value: Option<&'a str>,
    },
    /// 不以 `#` 开头的 URI 行
    Uri(&'a str),
}

/// 有效行：去掉开头的 BOM，跳过空行与不以 `#EXT` 开头的注释行。
pub(crate) fn lines(text: &str) -> impl Iterator<Item = Line<'_>> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    text.split('\n').enumerate().filter_map(|(i, raw)| {
        let line = raw.trim();
        if line.is_empty() {
            return None;
        }
        let kind = match line.strip_prefix('#') {
            Some(tag) if tag.starts_with("EXT") => match tag.split_once(':') {
                Some((name, value)) => LineKind::Tag {
                    name: name.trim().to_ascii_uppercase(),
                    value: Some(value.trim()),
                },
                None => LineKind::Tag {
                    name: tag.to_ascii_uppercase(),
                    value: None,
                },
            },
            Some(_) => return None,
            None => LineKind::Uri(line),
        };
        Some(Line {
            number: i + 1,
            kind,
        })
    })
}

/// 属性列表 `NAME=VALUE,...` 解析结果。属性名已转大写；值已去掉引号。
pub(crate) struct Attributes<'a> {
    items: Vec<(String, &'a str)>,
}

impl<'a> Attributes<'a> {
    /// VALUE 为带引号的字符串（可含逗号）或到下一个逗号为止的裸值。属性名重复时报错。
    pub(crate) fn parse(text: &'a str) -> Result<Self, SyntaxError> {
        // 只记原因、不带原文：属性里常有带令牌的地址；出错的行号由调用方给出
        let bad = |reason: &str| SyntaxError::Attributes(reason.to_owned());
        let mut items: Vec<(String, &'a str)> = Vec::new();
        let mut rest = text.trim_start();
        while !rest.is_empty() {
            let (name, after) = rest.split_once('=').ok_or_else(|| bad("属性缺少 '='"))?;
            let name = name.trim().to_ascii_uppercase();
            if name.is_empty() {
                return Err(bad("属性名为空"));
            }
            let after = after.trim_start();
            let (value, remaining) = match after.strip_prefix('"') {
                Some(quoted) => {
                    let end = quoted.find('"').ok_or_else(|| bad("引号未闭合"))?;
                    (&quoted[..end], &quoted[end + 1..])
                }
                None => {
                    let end = after.find(',').unwrap_or(after.len());
                    (after[..end].trim(), &after[end..])
                }
            };
            if items.iter().any(|(n, _)| *n == name) {
                return Err(bad(&format!("属性 {name} 重复")));
            }
            items.push((name, value));
            let remaining = remaining.trim_start();
            rest = match remaining.strip_prefix(',') {
                Some(next) => next.trim_start(),
                None if remaining.is_empty() => remaining,
                None => return Err(bad("属性值之后应为逗号")),
            };
        }
        Ok(Attributes { items })
    }

    pub(crate) fn get(&self, name: &str) -> Option<&'a str> {
        self.items.iter().find(|(n, _)| n == name).map(|(_, v)| *v)
    }

    pub(crate) fn require(
        &self,
        tag: &'static str,
        name: &'static str,
    ) -> Result<&'a str, SyntaxError> {
        self.get(name)
            .ok_or(SyntaxError::MissingAttribute { tag, name })
    }
}
