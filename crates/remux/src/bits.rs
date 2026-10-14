//! 按位读字节，帧头与 NAL 单元的解析共用。

/// 按位从高到低读字节。
pub(crate) struct Bits<'a> {
    data: &'a [u8],
    /// 已读的位数
    at: usize,
}

impl Bits<'_> {
    pub(crate) fn new(data: &[u8]) -> Bits<'_> {
        Bits { data, at: 0 }
    }

    /// 读 `n`（不超过 32）位；数据不够时为 None。
    pub(crate) fn read(&mut self, n: usize) -> Option<u32> {
        let mut value = 0;
        for _ in 0..n {
            let byte = self.data.get(self.at / 8)?;
            value = value << 1 | u32::from(byte >> (7 - self.at % 8) & 1);
            self.at += 1;
        }
        Some(value)
    }

    /// 读 `n` 位，须等于 `expected`。
    pub(crate) fn expect(&mut self, n: usize, expected: u32) -> Option<()> {
        (self.read(n)? == expected).then_some(())
    }

    /// 读一个无符号指数哥伦布码 ue(v)（H.264 9.1）：前导 0 超过 31 个或数据不够时为 None。
    pub(crate) fn read_ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.read(zeros)?;
        Some((1u32 << zeros) - 1 + rest)
    }
}
