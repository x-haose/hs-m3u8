//! 回调：站点适配、init 段经 on_init 还原、只差查询串的 init 段。

use std::sync::Arc;

use hs_m3u8_core::{HookError, Hooks, Purpose, RequestParts, Url};
use hs_m3u8_remux::{DiscontinuityGroup, Streams};

use super::{TS_A, expected_ts_a};
use crate::server::Server;
use crate::{assert_output, encrypt, engine, expected, fixture, request, run, test_dir, track};

/// 站点适配：改写播放列表、给每个请求签名、解开变换过的 key、去掉分片前的伪装字节。
struct SiteHooks;

const DISGUISE: &[u8] = b"\x89PNG\r\n\x1a\n";

impl Hooks for SiteHooks {
    fn on_playlist(&self, _url: &Url, body: &[u8]) -> Result<Option<String>, HookError> {
        Ok(Some(std::str::from_utf8(body)?.replace("{segment}", "seg")))
    }

    fn on_request(&self, _purpose: Purpose, request: &mut RequestParts) -> Result<(), HookError> {
        request.headers.push(("x-token".into(), "secret".into()));
        Ok(())
    }

    fn on_key(&self, _url: &Url, data: &[u8]) -> Result<Option<[u8; 16]>, HookError> {
        let key: [u8; 16] = data.try_into()?;
        Ok(Some(key.map(|b| b ^ 0xFF)))
    }

    fn on_segment(&self, _url: &Url, data: Vec<u8>) -> Result<Vec<u8>, HookError> {
        data.strip_prefix(DISGUISE)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| "缺少伪装前缀".into())
    }
}

/// 经站点适配回调（服务器要求签名请求头、key 变换过、分片带伪装前缀）下载：成片与明文样本合并的一致。
#[tokio::test(flavor = "multi_thread")]
async fn hooks_adapt_site() {
    let dir = test_dir("hooks");
    let server = Server::start().await;
    server.require_header("x-token", "secret");
    let key = [0x5A; 16];
    server.put("key", key.map(|b| b ^ 0xFF));
    for (i, name) in TS_A.iter().enumerate() {
        let iv = (i as u128).to_be_bytes();
        let cipher = encrypt(&fixture(&format!("ts_a/{name}")), &key, &iv);
        let body = [DISGUISE, cipher.as_slice()].concat();
        server.put(name, body);
    }
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"key\"\n\
         #EXTINF:1,\n{segment}0.ts\n#EXTINF:1,\n{segment}1.ts\n#EXT-X-ENDLIST\n",
    );

    let mut req = request(server.url("index.m3u8"), &dir);
    req.source.hooks = Arc::new(SiteHooks);
    // 探测经同样的回调：带上签名请求头，播放列表经改写
    let probe = engine().probe(&req.source).await.unwrap();
    assert_eq!(probe.master, None);
    assert_eq!(
        (probe.tracks[0].segments, probe.tracks[0].encrypted),
        (2, true)
    );
    let output = run(req).await.unwrap();

    let want = expected_ts_a(&dir);
    assert_output(&output, &want);
}

/// 去掉 init 段与分片前伪装成图片的字节。
struct StripDisguise;

impl Hooks for StripDisguise {
    fn on_init(&self, _url: &Url, data: Vec<u8>) -> Result<Vec<u8>, HookError> {
        Ok(data.strip_prefix(DISGUISE).ok_or("缺少伪装前缀")?.to_vec())
    }

    fn on_segment(&self, _url: &Url, data: Vec<u8>) -> Result<Vec<u8>, HookError> {
        Ok(data.strip_prefix(DISGUISE).ok_or("缺少伪装前缀")?.to_vec())
    }
}

/// init 段与分片都带伪装前缀：init 段经 on_init、分片经 on_segment 还原后照常校验、合并。
#[tokio::test(flavor = "multi_thread")]
async fn init_sections_pass_through_on_init() {
    let dir = test_dir("on_init");
    let server = Server::start().await;
    for name in ["init.mp4", "seg0.m4s", "seg1.m4s"] {
        let data = [DISGUISE, &fixture(&format!("fmp4_a/video/{name}"))].concat();
        server.put(name, data);
    }
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"init.mp4\"\n\
         #EXTINF:1,\nseg0.m4s\n#EXTINF:1,\nseg1.m4s\n#EXT-X-ENDLIST\n",
    );
    let mut req = request(server.url("index.m3u8"), &dir);
    req.source.hooks = Arc::new(StripDisguise);

    let output = run(req).await.unwrap();

    let segments = ["seg0.m4s", "seg1.m4s"];
    let tracks = vec![track("fmp4_a/video", Some("init.mp4"), &segments)];
    let want = expected(&dir, &[Streams::All], &[DiscontinuityGroup { tracks }]);
    assert_output(&output, &want);
}

/// 把 init 段地址 `init?p=<目录>` 改写为 `<目录>/init.mp4`：模拟按查询串区分内容的站点。
struct InitByQuery;

impl Hooks for InitByQuery {
    fn on_request(&self, purpose: Purpose, request: &mut RequestParts) -> Result<(), HookError> {
        if purpose == Purpose::Init
            && let Some(program) = request.url.query().and_then(|q| q.strip_prefix("p="))
        {
            request.url = request.url.join(&format!("{program}/init.mp4"))?;
            request.url.set_query(None);
        }
        Ok(())
    }
}

/// 两个 init 段的地址只差查询串（站点按查询串区分内容）：各拉各的，不当成同一个。
#[tokio::test(flavor = "multi_thread")]
async fn inits_differing_only_in_query_are_distinct() {
    let dir = test_dir("init_query");
    let server = Server::start().await;
    for program in ["fmp4_a", "fmp4_b"] {
        for name in ["init.mp4", "seg0.m4s"] {
            server.put(
                &format!("{program}/{name}"),
                fixture(&format!("{program}/video/{name}")),
            );
        }
    }
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"init?p=fmp4_a\"\n\
         #EXTINF:1,\nfmp4_a/seg0.m4s\n#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"init?p=fmp4_b\"\n\
         #EXTINF:1,\nfmp4_b/seg0.m4s\n#EXT-X-ENDLIST\n",
    );
    let mut req = request(server.url("index.m3u8"), &dir);
    req.source.hooks = Arc::new(InitByQuery);

    let output = run(req).await.unwrap();

    let program = |name: &str| DiscontinuityGroup {
        tracks: vec![track(
            &format!("{name}/video"),
            Some("init.mp4"),
            &["seg0.m4s"],
        )],
    };
    let want = expected(
        &dir,
        &[Streams::All],
        &[program("fmp4_a"), program("fmp4_b")],
    );
    assert_output(&output, &want);
    assert_eq!(
        (
            server.hits("fmp4_a/init.mp4"),
            server.hits("fmp4_b/init.mp4")
        ),
        (1, 1)
    );
}
