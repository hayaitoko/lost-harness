//! Wave 5.3 / M8 — model download + the **verified-before-runnable** invariant.
//!
//! A downloaded model becomes usable ONLY after its bytes hash to the catalog's
//! pinned `sha256`. Three fail-closed corollaries:
//! 1. **Verify-or-nothing.** A digest mismatch installs NOTHING — the `.partial`
//!    is deleted, no final file, no `model_catalog` row ([`verify_and_install`]).
//! 2. **Allowlisted egress.** The downloader only fetches from Hugging Face
//!    ([`host_allowed`]) — the catalog can't point the app at an arbitrary host.
//! 3. **Re-check at boot.** (S4) a registered model whose file no longer matches
//!    its stored hash is quarantined, never silently served.
//!
//! The verify + install + allowlist logic is pure/local and unit-tested here.
//! [`download_to_partial`] itself needs a socket, but its two decision points —
//! *what to do with the bytes already on disk* ([`classify_partial`],
//! [`plan_resume`]) and *how to consume the body* ([`stream_to_file`]) — are
//! split out as pure/stream-generic units so resume safety (M-02) is unit-tested
//! rather than trusted.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// Hosts the downloader may fetch from — Hugging Face + its LFS CDN only. A
/// catalog URL off this list is refused (corollary 2): the download's network
/// reach is constrained HERE, not by the tool gate (model management isn't a
/// tool). Case-insensitive; subdomains of the allowed hosts (e.g.
/// `cdn-lfs.huggingface.co`, which HF redirects LFS blobs to) are allowed.
pub fn host_allowed(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false; // never plain-http for a trusted download
    }
    let Some(host) = parsed.host_str().map(|h| h.to_ascii_lowercase()) else {
        return false;
    };
    const ROOTS: &[&str] = &["huggingface.co", "hf.co"];
    ROOTS
        .iter()
        .any(|root| host == *root || host.ends_with(&format!(".{root}")))
}

/// A redirect policy that re-checks [`host_allowed`] on EVERY hop. Checking
/// only the caller-constructed URL is not enough: reqwest's default policy
/// silently follows up to 10 redirects with no host re-check, so a redirect
/// chain could carry an "allowlisted" request off the allowlist. An off-list
/// hop errors loudly (never silently dropped); depth is bounded. Shared by the
/// downloader, the HF search layer, and the GGUF metadata reader — every HF
/// touchpoint rides the same gate.
pub(crate) fn allowlisted_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error("too many redirects");
        }
        let next = attempt.url().as_str().to_string();
        if host_allowed(&next) {
            attempt.follow()
        } else {
            attempt.error(format!("redirect to a non-allowlisted host refused: {next}"))
        }
    })
}

/// The SHA-256 of a file as lowercase hex. Streams the file (never loads it whole).
pub fn file_sha256(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Is this a real SHA-256 (64 hex chars, case-insensitive) — not a placeholder?
/// `verify_and_install` lowercases the expected value before comparing, so an
/// upper- or mixed-case digest is accepted here and normalised there.
/// `pub(crate)` so the HF search layer reuses the exact same gate instead of
/// maintaining a parallel copy (they must never drift).
pub(crate) fn is_real_sha256(s: &str) -> bool {
    let s = s.trim();
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Verify a downloaded `partial` against `expected_sha256`; on match, atomically
/// rename it to `final_path` and return `Ok`. On MISMATCH or a placeholder hash,
/// delete the `partial` and return `Err`, installing NOTHING (corollary 1).
pub fn verify_and_install(partial: &Path, final_path: &Path, expected_sha256: &str) -> Result<()> {
    if !is_real_sha256(expected_sha256) {
        let _ = std::fs::remove_file(partial);
        bail!("refusing to install: the catalog entry has no real sha256 (not release-curated)");
    }
    let expected = expected_sha256.trim().to_ascii_lowercase();
    let actual = file_sha256(partial)?;
    if actual != expected {
        let _ = std::fs::remove_file(partial);
        bail!("integrity check failed (expected {expected}, got {actual}) — nothing installed");
    }
    // Verified → atomically publish. A rename within one directory is atomic, so
    // a `final_path` that exists is always a fully-verified file.
    std::fs::rename(partial, final_path)?;
    Ok(())
}

/// Wall-clock ceilings for a single [`download_to_partial`] call.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DownloadLimits {
    /// Longest gap between two chunks before the transfer is declared hung.
    pub idle: Duration,
    /// Longest wall-clock for the WHOLE transfer. An idle timeout alone is not
    /// enough: a "slow-drip" server that emits one byte just inside the idle
    /// window resets that timer forever and holds the download open
    /// indefinitely. A hard ceiling costs progress at worst (the partial stays
    /// on disk and the next attempt resumes), never the download.
    pub total: Duration,
    /// Once the last declared byte has landed, how long to wait for the
    /// stream's end-of-body marker. Reaching EOF is what proves the server did
    /// not send MORE than it declared. If the marker never arrives we still
    /// accept the transfer — every declared byte is on disk and the sha256 gate
    /// runs next — rather than failing an otherwise complete download.
    pub eof_grace: Duration,
}

impl Default for DownloadLimits {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(30),
            // 12 h: a multi-GB weights file on a slow line legitimately runs
            // for hours, and an interrupted transfer resumes.
            total: Duration::from_secs(12 * 60 * 60),
            eof_grace: Duration::from_secs(5),
        }
    }
}

/// A parsed `Content-Range: bytes <start>-<end>/<total>` (total `*` → `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContentRange {
    pub start: u64,
    /// INCLUSIVE last byte offset, per RFC 9110 §14.4.
    pub end: u64,
    pub total: Option<u64>,
}

/// Parse a `Content-Range` response header. Anything malformed is an error —
/// an unparseable range must never be treated as "close enough" and appended.
pub(crate) fn parse_content_range(raw: &str) -> Result<ContentRange> {
    let s = raw.trim();
    let (unit, rest) = s
        .split_once(char::is_whitespace)
        .with_context(|| format!("Content-Range has no range unit: {raw:?}"))?;
    if !unit.eq_ignore_ascii_case("bytes") {
        bail!("Content-Range unit is not `bytes`: {raw:?}");
    }
    let (range, total) = rest
        .trim()
        .split_once('/')
        .with_context(|| format!("Content-Range has no /total: {raw:?}"))?;
    let total = match total.trim() {
        "*" => None,
        t => Some(
            t.parse::<u64>()
                .with_context(|| format!("Content-Range total is not an integer: {raw:?}"))?,
        ),
    };
    let (start, end) = range
        .trim()
        .split_once('-')
        .with_context(|| format!("Content-Range has no start-end: {raw:?}"))?;
    let start = start
        .trim()
        .parse::<u64>()
        .with_context(|| format!("Content-Range start is not an integer: {raw:?}"))?;
    let end = end
        .trim()
        .parse::<u64>()
        .with_context(|| format!("Content-Range end is not an integer: {raw:?}"))?;
    if end < start {
        bail!("Content-Range end precedes its start: {raw:?}");
    }
    if let Some(total) = total {
        // `end` is inclusive, so the last legal offset is total-1.
        if end >= total {
            bail!("Content-Range end {end} lies outside its own total {total}: {raw:?}");
        }
    }
    Ok(ContentRange { start, end, total })
}

/// What to do with the bytes already sitting in the `.partial` file, decided
/// BEFORE any request is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PartialState {
    /// Every declared byte is already on disk — nothing left to fetch.
    Complete,
    /// Ask the server for `bytes=<offset>-` (an offset of 0 = fresh download).
    Resume(u64),
    /// The partial is LARGER than the declared total, so it cannot be a prefix
    /// of the right file — discard it and download from scratch.
    Discard,
}

/// Classify a pre-existing partial against the catalog's declared size.
///
/// The oversized case (M-02 item 6) is the interesting one and its handling is
/// deliberately destructive: a partial longer than the declared total is not a
/// truncated prefix of the wanted file, it is *different bytes*, so no amount of
/// resuming can ever make it hash correctly. Truncating it back to
/// `expected_size` would be worse than useless (it would hand a plausible-looking
/// file to the hasher), and refusing outright would wedge the download forever.
/// So: throw it away and start over.
///
/// `expected_size == 0` means "size unknown" (no catalog figure); the ceilings
/// are inapplicable and the old resume-from-whatever-is-there behaviour stands.
pub(crate) fn classify_partial(on_disk: u64, expected_size: u64) -> PartialState {
    if expected_size == 0 {
        return PartialState::Resume(on_disk);
    }
    if on_disk > expected_size {
        return PartialState::Discard;
    }
    if on_disk == expected_size {
        return PartialState::Complete;
    }
    PartialState::Resume(on_disk)
}

/// How the response says the body relates to what is already on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeAction {
    /// Append the body to the existing partial, starting at this offset.
    Append { from: u64 },
    /// The server ignored our `Range` — truncate the partial and start over.
    Restart,
}

/// Decide what the response permits, from the status line and headers ALONE —
/// before a single byte is written (M-02 items 1-3).
///
/// * `206` — the `Content-Range` must describe exactly the bytes we asked for:
///   `start` must equal what we already hold, `end` must not reach past the
///   declared size, and the stated `total` must agree with the declared size.
///   (`total: *` is tolerated: HF always states it, and `end` plus the final
///   exact-length check already pin the size even without it.)
/// * `200` — the server ignored `Range`; the body is the WHOLE file, so the
///   partial must be truncated, never appended to.
/// * anything else — fail closed.
///
/// A 206 range SHORTER than requested is accepted: the bytes are still a valid
/// prefix continuation, `stream_to_file` then reports the transfer as incomplete,
/// and the next attempt resumes from the new offset. That loses no progress,
/// where accepting-and-verifying would delete the partial on the hash mismatch.
pub(crate) fn plan_resume(
    status: u16,
    content_range: Option<&str>,
    content_length: Option<u64>,
    already: u64,
    expected_size: u64,
) -> Result<ResumeAction> {
    match status {
        206 => {
            let raw = content_range
                .context("a 206 Partial Content response carried no Content-Range header")?;
            let range = parse_content_range(raw)?;
            if range.start != already {
                bail!(
                    "Content-Range starts at {} but the partial holds {already} byte(s): {raw:?}",
                    range.start
                );
            }
            if expected_size > 0 {
                if range.end >= expected_size {
                    bail!(
                        "Content-Range end {} reaches past the declared size {expected_size} \
                         (last legal offset is {}): {raw:?}",
                        range.end,
                        expected_size - 1
                    );
                }
                if let Some(total) = range.total {
                    if total != expected_size {
                        bail!(
                            "Content-Range total {total} contradicts the declared size \
                             {expected_size}: {raw:?}"
                        );
                    }
                }
            }
            if let Some(len) = content_length {
                let declared = range.end - range.start + 1;
                if len != declared {
                    bail!(
                        "Content-Length {len} contradicts the {declared}-byte range it \
                         accompanies: {raw:?}"
                    );
                }
            }
            Ok(ResumeAction::Append { from: already })
        }
        200 => {
            // Over-declared body → refuse now, before opening the file.
            if expected_size > 0 {
                if let Some(len) = content_length {
                    if len > expected_size {
                        bail!(
                            "response declares {len} bytes, more than the expected size \
                             {expected_size}"
                        );
                    }
                }
            }
            Ok(ResumeAction::Restart)
        }
        416 => bail!(
            "server refused the resume range for the {already} byte(s) already on disk \
             (416 Range Not Satisfiable)"
        ),
        other => bail!("download failed with HTTP status {other}"),
    }
}

/// Open the partial for the body we are about to receive. `start_at == 0` means
/// TRUNCATE: appending a whole-file body onto an existing partial is exactly the
/// silent corruption M-02 item 2 is about.
async fn open_partial(partial: &Path, start_at: u64) -> Result<tokio::fs::File> {
    let mut opts = tokio::fs::OpenOptions::new();
    opts.create(true);
    if start_at == 0 {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    let file = opts
        .open(partial)
        .await
        .with_context(|| format!("opening {}", partial.display()))?;
    let len = file.metadata().await?.len();
    if len != start_at {
        bail!(
            "{} holds {len} byte(s) but the resume plan assumed {start_at}",
            partial.display()
        );
    }
    Ok(file)
}

/// Which clock we are currently waiting against — decides what a timeout means.
enum WaitKind {
    Idle,
    Total,
    Eof,
}

/// Drain `stream` into `file`, enforcing the size ceiling and both clocks.
/// Returns the total byte count on disk. Generic over the stream so the ceiling,
/// the EOF proof and both timeouts are unit-tested without a socket.
async fn stream_to_file<S, B, E, F>(
    mut stream: S,
    file: &mut tokio::fs::File,
    start_at: u64,
    expected_size: u64,
    total_for_progress: u64,
    limits: DownloadLimits,
    on_progress: &F,
) -> Result<u64>
where
    S: tokio_stream::Stream<Item = std::result::Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
    F: Fn(u64, u64),
{
    use tokio::io::AsyncWriteExt;
    use tokio_stream::StreamExt;

    let started = tokio::time::Instant::now();
    let mut downloaded = start_at;

    loop {
        let complete = expected_size > 0 && downloaded >= expected_size;
        let budget = limits.total.checked_sub(started.elapsed()).unwrap_or_default();
        let wait;
        let kind;
        if complete {
            // Keep reading past the declared size: only EOF proves the server
            // stopped there. Bounded by `eof_grace`, so a server that never
            // closes the body cannot hold us.
            let grace = limits.eof_grace.min(budget);
            if grace.is_zero() {
                break;
            }
            wait = grace;
            kind = WaitKind::Eof;
        } else if budget.is_zero() {
            bail!("download exceeded its total budget of {:?}", limits.total);
        } else if budget <= limits.idle {
            wait = budget;
            kind = WaitKind::Total;
        } else {
            wait = limits.idle;
            kind = WaitKind::Idle;
        }

        let next = match tokio::time::timeout(wait, stream.next()).await {
            Ok(next) => next,
            Err(_) => match kind {
                WaitKind::Eof => break, // complete, no end-of-body marker → accept
                WaitKind::Idle => bail!(
                    "download stalled: no data for {:?} ({downloaded} byte(s) on disk)",
                    limits.idle
                ),
                WaitKind::Total => {
                    bail!("download exceeded its total budget of {:?}", limits.total)
                }
            },
        };
        let Some(chunk) = next else {
            break; // end of body — the stream really did end
        };
        let chunk = chunk.map_err(|e| anyhow::anyhow!("download stream error: {e}"))?;
        let bytes = chunk.as_ref();
        if bytes.is_empty() {
            continue;
        }
        if expected_size > 0 && downloaded + bytes.len() as u64 > expected_size {
            // Refuse BEFORE writing. Truncating the chunk to fit and calling it
            // done would hand a silently-different file to the hasher.
            bail!(
                "server sent more than the declared {expected_size} bytes: {} extra byte(s) \
                 after {downloaded}",
                downloaded + bytes.len() as u64 - expected_size
            );
        }
        file.write_all(bytes).await?;
        downloaded += bytes.len() as u64;
        on_progress(downloaded, total_for_progress);
    }

    file.flush().await?;
    if expected_size > 0 && downloaded != expected_size {
        bail!("incomplete download: {downloaded} of {expected_size} byte(s)");
    }
    Ok(downloaded)
}

/// Stream `url` into `partial`, RESUMING from an existing partial via a `Range`
/// request. `expected_size` is the catalog's declared byte count for the file
/// (0 = unknown, which disables the size ceiling). `on_progress(downloaded,
/// total)` fires as bytes land. Refuses an off-allowlist host.
///
/// Resume safety (M-02): the response must *prove* it continues the partial
/// ([`plan_resume`]), the partial itself must be a possible prefix
/// ([`classify_partial`]), and the body may deliver neither more nor fewer bytes
/// than declared ([`stream_to_file`]).
pub async fn download_to_partial<F: Fn(u64, u64)>(
    url: &str,
    partial: &Path,
    expected_size: u64,
    on_progress: F,
) -> Result<()> {
    download_to_partial_with_limits(
        url,
        partial,
        expected_size,
        on_progress,
        DownloadLimits::default(),
    )
    .await
}

async fn download_to_partial_with_limits<F: Fn(u64, u64)>(
    url: &str,
    partial: &Path,
    expected_size: u64,
    on_progress: F,
    limits: DownloadLimits,
) -> Result<()> {
    if !host_allowed(url) {
        bail!("refusing to download from a non-allowlisted host: {url}");
    }
    let on_disk = std::fs::metadata(partial).map(|m| m.len()).unwrap_or(0);
    let already = match classify_partial(on_disk, expected_size) {
        PartialState::Complete => {
            // Nothing to fetch — and asking for `bytes={expected_size}-` would
            // only earn a 416. Whether these are the RIGHT bytes is
            // `verify_and_install`'s call, not ours.
            on_progress(on_disk, expected_size);
            return Ok(());
        }
        PartialState::Discard => {
            tracing::warn!(
                partial = %partial.display(),
                on_disk,
                expected_size,
                "discarding an oversized partial — it cannot be a prefix of the declared file"
            );
            0
        }
        PartialState::Resume(from) => from,
    };

    // No TOTAL reqwest timeout here — `limits.total` bounds the transfer
    // instead, measured over the body rather than the request future.
    let client = reqwest::Client::builder()
        .user_agent("lost-harness/0.1 (model-downloader)")
        .connect_timeout(Duration::from_secs(15))
        .redirect(allowlisted_redirect_policy())
        .build()?;
    let mut req = client.get(url);
    if already > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={already}-"));
    }
    let resp = req.send().await?;
    let status = resp.status().as_u16();
    let content_range = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let content_length = resp.content_length();

    let start_at = match plan_resume(
        status,
        content_range.as_deref(),
        content_length,
        already,
        expected_size,
    )? {
        ResumeAction::Append { from } => from,
        ResumeAction::Restart => 0,
    };
    let total = if expected_size > 0 {
        expected_size
    } else {
        start_at + content_length.unwrap_or(0)
    };

    let mut file = open_partial(partial, start_at).await?;
    let stream = Box::pin(resp.bytes_stream());
    stream_to_file(
        stream,
        &mut file,
        start_at,
        expected_size,
        total,
        limits,
        &on_progress,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("lhp-dl-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn host_allowlist_permits_only_huggingface_https() {
        assert!(host_allowed("https://huggingface.co/x/y.gguf"));
        assert!(host_allowed("https://cdn-lfs.huggingface.co/blob"));
        assert!(host_allowed("https://hf.co/x"));
        assert!(!host_allowed("http://huggingface.co/x"), "plain http refused");
        assert!(!host_allowed("https://evil.com/x.gguf"));
        assert!(!host_allowed("https://huggingface.co.evil.com/x"), "suffix-spoof refused");
        assert!(!host_allowed("not a url"));
    }

    #[test]
    fn verify_installs_on_match_and_installs_nothing_on_mismatch() {
        let dir = tmp();
        let partial = dir.join("model.gguf.partial");
        let final_path = dir.join("model.gguf");
        std::fs::File::create(&partial).unwrap().write_all(b"hello").unwrap();
        // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        let good = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

        // Mismatch → nothing installed, partial gone.
        assert!(verify_and_install(&partial, &final_path, &"a".repeat(64)).is_err());
        assert!(!partial.exists(), "a failed verify removes the partial");
        assert!(!final_path.exists(), "a failed verify installs no final file");

        // Recreate + verify with the correct hash → atomic install.
        std::fs::File::create(&partial).unwrap().write_all(b"hello").unwrap();
        verify_and_install(&partial, &final_path, good).unwrap();
        assert!(final_path.exists() && !partial.exists(), "verified → renamed to final");
        assert_eq!(std::fs::read(&final_path).unwrap(), b"hello");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn verify_refuses_a_placeholder_hash() {
        let dir = tmp();
        let partial = dir.join("m.partial");
        std::fs::File::create(&partial).unwrap().write_all(b"data").unwrap();
        let err = verify_and_install(&partial, &dir.join("m"), "TODO-CURATE").unwrap_err();
        assert!(err.to_string().contains("no real sha256"));
        assert!(!partial.exists(), "a placeholder verify removes the partial too");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn file_sha256_matches_known_vector() {
        let dir = tmp();
        let p = dir.join("f");
        std::fs::File::create(&p).unwrap().write_all(b"hello").unwrap();
        assert_eq!(
            file_sha256(&p).unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    // ── M-02: resume safety ────────────────────────────────────────────────

    #[test]
    fn content_range_parses_start_end_and_total() {
        assert_eq!(
            parse_content_range("bytes 100-199/1000").unwrap(),
            ContentRange { start: 100, end: 199, total: Some(1000) }
        );
        // An unknown total is `*`, not absent.
        assert_eq!(
            parse_content_range("bytes 0-9/*").unwrap(),
            ContentRange { start: 0, end: 9, total: None }
        );
        // Case-insensitive unit + tolerant whitespace.
        assert_eq!(
            parse_content_range("  Bytes  5-6 / 7  ").unwrap(),
            ContentRange { start: 5, end: 6, total: Some(7) }
        );
    }

    #[test]
    fn malformed_content_range_is_refused_not_guessed_at() {
        for raw in [
            "100-199/1000",       // no unit
            "items 1-2/3",        // wrong unit
            "bytes 100-199",      // no /total
            "bytes 100/1000",     // no start-end
            "bytes a-199/1000",   // non-integer start
            "bytes 100-b/1000",   // non-integer end
            "bytes 100-199/x",    // non-integer total
            "bytes 200-199/1000", // end precedes start
            "bytes 0-1000/1000",  // end outside its own total (end is inclusive)
            "bytes */1000",       // 416-style, not a body description
        ] {
            assert!(
                parse_content_range(raw).is_err(),
                "malformed Content-Range must be refused: {raw:?}"
            );
        }
    }

    #[test]
    fn resume_accepts_only_a_206_that_continues_exactly_where_the_partial_ends() {
        // Partial holds 400 of 1000 bytes; the server must offer 400..=999.
        assert_eq!(
            plan_resume(206, Some("bytes 400-999/1000"), Some(600), 400, 1000).unwrap(),
            ResumeAction::Append { from: 400 }
        );
        // Starting anywhere else would leave a hole or duplicate bytes.
        for cr in ["bytes 399-999/1000", "bytes 401-999/1000", "bytes 0-999/1000"] {
            let err = plan_resume(206, Some(cr), None, 400, 1000).unwrap_err().to_string();
            assert!(err.contains("Content-Range starts at"), "unexpected error for {cr}: {err}");
        }
    }

    #[test]
    fn resume_rejects_a_206_whose_total_contradicts_the_catalog_size() {
        // The catalog says 1000 bytes; the server claims the resource is 1001.
        // The offered range is otherwise self-consistent, so ONLY the total
        // check can catch this.
        let err = plan_resume(206, Some("bytes 400-999/1001"), Some(600), 400, 1000)
            .unwrap_err()
            .to_string();
        assert!(err.contains("total 1001 contradicts"), "got: {err}");
        // A total that agrees is fine, and `*` is tolerated.
        assert!(plan_resume(206, Some("bytes 400-999/1000"), Some(600), 400, 1000).is_ok());
        assert!(plan_resume(206, Some("bytes 400-999/*"), Some(600), 400, 1000).is_ok());
    }

    #[test]
    fn resume_rejects_a_range_reaching_one_byte_past_the_declared_size() {
        // `end` is INCLUSIVE, so for a 1000-byte file the last legal offset is
        // 999. `end == 1000` is one byte of excess and must be refused BEFORE
        // anything is written.
        let err = plan_resume(206, Some("bytes 400-1000/*"), None, 400, 1000)
            .unwrap_err()
            .to_string();
        assert!(err.contains("reaches past the declared size 1000"), "got: {err}");
        // The boundary itself is legal.
        assert_eq!(
            plan_resume(206, Some("bytes 400-999/*"), None, 400, 1000).unwrap(),
            ResumeAction::Append { from: 400 }
        );
    }

    #[test]
    fn resume_rejects_a_content_length_that_contradicts_its_own_range() {
        // 400-999 inclusive is 600 bytes; a 601-byte body is a contradiction.
        let err = plan_resume(206, Some("bytes 400-999/1000"), Some(601), 400, 1000)
            .unwrap_err()
            .to_string();
        assert!(err.contains("contradicts the 600-byte range"), "got: {err}");
    }

    #[test]
    fn a_206_without_a_content_range_header_is_refused() {
        let err = plan_resume(206, None, Some(600), 400, 1000).unwrap_err().to_string();
        assert!(err.contains("carried no Content-Range"), "got: {err}");
    }

    #[test]
    fn a_200_on_a_resume_attempt_restarts_from_zero_instead_of_appending() {
        // The server ignored Range and is sending the WHOLE file. Appending it
        // to the 400 bytes we hold would silently corrupt the partial.
        assert_eq!(
            plan_resume(200, None, Some(1000), 400, 1000).unwrap(),
            ResumeAction::Restart
        );
    }

    #[test]
    fn a_200_declaring_more_than_the_expected_size_is_refused_before_any_bytes() {
        let err = plan_resume(200, None, Some(1001), 0, 1000).unwrap_err().to_string();
        assert!(err.contains("more than the expected size"), "got: {err}");
    }

    #[test]
    fn other_statuses_fail_closed_on_a_resume() {
        assert!(plan_resume(416, None, None, 400, 1000).unwrap_err().to_string().contains("416"));
        assert!(plan_resume(404, None, None, 0, 1000).is_err());
        assert!(plan_resume(204, None, None, 0, 1000).is_err());
        assert!(plan_resume(302, None, None, 0, 1000).is_err());
    }

    #[test]
    fn an_oversized_partial_is_discarded_rather_than_resumed_or_truncated() {
        assert_eq!(classify_partial(1001, 1000), PartialState::Discard);
        assert_eq!(classify_partial(u64::MAX, 1000), PartialState::Discard);
        assert_eq!(classify_partial(1000, 1000), PartialState::Complete);
        assert_eq!(classify_partial(400, 1000), PartialState::Resume(400));
        assert_eq!(classify_partial(0, 1000), PartialState::Resume(0));
        // Unknown declared size → no ceiling, resume from whatever is there.
        assert_eq!(classify_partial(400, 0), PartialState::Resume(400));
    }

    #[tokio::test]
    async fn restart_truncates_the_partial_while_resume_preserves_it() {
        use tokio::io::AsyncWriteExt;
        let dir = tmp();
        let p = dir.join("m.partial");
        std::fs::File::create(&p).unwrap().write_all(&[7u8; 500]).unwrap();

        // Resume at 500 → append mode, existing bytes intact.
        let mut f = open_partial(&p, 500).await.unwrap();
        f.write_all(b"XY").await.unwrap();
        f.flush().await.unwrap();
        drop(f);
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 502, "resume appends");

        // Restart at 0 → truncate.
        let f = open_partial(&p, 0).await.unwrap();
        drop(f);
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 0, "restart truncates");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn open_partial_refuses_a_file_that_is_not_the_length_the_plan_assumed() {
        let dir = tmp();
        let p = dir.join("m.partial");
        std::fs::File::create(&p).unwrap().write_all(&[0u8; 10]).unwrap();
        let err = open_partial(&p, 400).await.unwrap_err().to_string();
        assert!(err.contains("holds 10 byte(s) but the resume plan assumed 400"), "got: {err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A one-off sink for the stream tests.
    async fn sink(dir: &Path) -> (std::path::PathBuf, tokio::fs::File) {
        let p = dir.join("s.partial");
        let f = open_partial(&p, 0).await.unwrap();
        (p, f)
    }

    /// One synthetic body chunk. `String` stands in for `reqwest::Error`.
    type Chunk = std::result::Result<Vec<u8>, String>;

    fn chunks(parts: &[&[u8]]) -> tokio_stream::Iter<std::vec::IntoIter<Chunk>> {
        tokio_stream::iter(parts.iter().map(|p| Ok(p.to_vec())).collect::<Vec<_>>())
    }

    #[tokio::test]
    async fn an_exact_stream_is_accepted_and_lands_every_byte() {
        let dir = tmp();
        let (p, mut f) = sink(&dir).await;
        let seen = std::cell::Cell::new(0u64);
        let n = stream_to_file(
            chunks(&[b"abcd", b"efghij"]),
            &mut f,
            0,
            10,
            10,
            DownloadLimits::default(),
            &|d, t| {
                assert_eq!(t, 10);
                seen.set(d);
            },
        )
        .await
        .unwrap();
        drop(f);
        assert_eq!(n, 10);
        assert_eq!(seen.get(), 10, "progress reported the final byte count");
        assert_eq!(std::fs::read(&p).unwrap(), b"abcdefghij");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn an_overlong_stream_is_refused_and_the_excess_is_never_written() {
        let dir = tmp();
        let (p, mut f) = sink(&dir).await;
        // First chunk completes the declared 10 bytes; a second chunk follows.
        // Only reading PAST the declared size can catch this — a loop that
        // breaks at `expected_size` would report success.
        let err = stream_to_file(
            chunks(&[b"abcdefghij", b"EXTRA"]),
            &mut f,
            0,
            10,
            10,
            DownloadLimits::default(),
            &|_, _| {},
        )
        .await
        .unwrap_err()
        .to_string();
        drop(f);
        assert!(err.contains("more than the declared 10 bytes"), "got: {err}");
        assert!(err.contains("5 extra byte(s)"), "got: {err}");
        assert_eq!(
            std::fs::read(&p).unwrap(),
            b"abcdefghij",
            "the excess chunk must not reach the file"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_short_stream_is_refused_so_the_partial_stays_resumable() {
        let dir = tmp();
        let (p, mut f) = sink(&dir).await;
        let err = stream_to_file(
            chunks(&[b"abc"]),
            &mut f,
            0,
            10,
            10,
            DownloadLimits::default(),
            &|_, _| {},
        )
        .await
        .unwrap_err()
        .to_string();
        drop(f);
        assert!(err.contains("incomplete download: 3 of 10"), "got: {err}");
        assert_eq!(std::fs::read(&p).unwrap(), b"abc", "the good prefix is kept for the resume");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_complete_body_with_no_end_of_body_marker_is_still_accepted() {
        let dir = tmp();
        let (_p, mut f) = sink(&dir).await;
        // Every declared byte lands, then the server just holds the socket
        // open. Waiting the full idle timeout and then FAILING would turn a
        // complete download into an error, so the EOF probe is grace-bounded.
        let stream = Box::pin(
            tokio_stream::StreamExt::chain(chunks(&[b"abcdefghij"]), tokio_stream::pending()),
        );
        let started = std::time::Instant::now();
        let n = stream_to_file(
            stream,
            &mut f,
            0,
            10,
            10,
            DownloadLimits {
                idle: Duration::from_secs(30),
                total: Duration::from_secs(600),
                eof_grace: Duration::from_millis(50),
            },
            &|_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(n, 10);
        assert!(started.elapsed() < Duration::from_secs(5), "bounded by eof_grace, not by idle");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_stream_trips_the_idle_timeout() {
        let dir = tmp();
        let (_p, mut f) = sink(&dir).await;
        let stream = Box::pin(tokio_stream::StreamExt::chain(
            chunks(&[b"abc"]),
            tokio_stream::pending::<Chunk>(),
        ));
        let err = stream_to_file(
            stream,
            &mut f,
            0,
            10_000,
            10_000,
            DownloadLimits {
                idle: Duration::from_secs(30),
                total: Duration::from_secs(3600),
                eof_grace: Duration::from_secs(5),
            },
            &|_, _| {},
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("download stalled"), "got: {err}");
        assert!(err.contains("3 byte(s) on disk"), "got: {err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_drip_that_never_trips_the_idle_timer_still_hits_the_total_budget() {
        use tokio_stream::StreamExt as _;
        let dir = tmp();
        let (_p, mut f) = sink(&dir).await;
        // One byte every 10 s: the 30 s idle timer NEVER fires, so without a
        // total budget this server holds the download open indefinitely.
        let drip = Box::pin(
            tokio_stream::iter(
                (0..1000).map(|_| Ok::<Vec<u8>, String>(vec![b'x'])).collect::<Vec<_>>(),
            )
            .throttle(Duration::from_secs(10)),
        );
        let err = stream_to_file(
            drip,
            &mut f,
            0,
            1000,
            1000,
            DownloadLimits {
                idle: Duration::from_secs(30),
                total: Duration::from_secs(45),
                eof_grace: Duration::from_secs(5),
            },
            &|_, _| {},
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("exceeded its total budget"), "got: {err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn an_already_complete_partial_short_circuits_without_touching_the_network() {
        let dir = tmp();
        let p = dir.join("done.partial");
        std::fs::File::create(&p).unwrap().write_all(b"hello").unwrap();
        // An allowlisted host that does not resolve to this file: if the
        // short-circuit is removed, this either 404s or fails to connect.
        let url = "https://huggingface.co/lhp-test-not-a-real-repo/does-not-exist.gguf";
        let seen = std::cell::Cell::new((0u64, 0u64));
        download_to_partial(url, &p, 5, |d, t| seen.set((d, t))).await.unwrap();
        assert_eq!(seen.get(), (5, 5), "progress is reported as already-finished");
        assert_eq!(std::fs::read(&p).unwrap(), b"hello", "the bytes are left for the hasher");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn download_still_refuses_an_off_allowlist_host_before_any_io() {
        let dir = tmp();
        let p = dir.join("x.partial");
        let err = download_to_partial("https://evil.com/m.gguf", &p, 10, |_, _| {})
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-allowlisted host"), "got: {err}");
        assert!(!p.exists(), "no file is created for a refused host");
        let _ = std::fs::remove_dir_all(dir);
    }
}
