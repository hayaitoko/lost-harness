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
//! The verify + install + allowlist logic is pure/local and unit-tested here;
//! the actual network streaming ([`download_to_partial`]) is integration-tested.

use std::path::Path;

use anyhow::{bail, Result};
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

/// Stream `url` into `partial`, RESUMING from an existing partial via a `Range`
/// request. `on_progress(downloaded, total)` fires as bytes land. Refuses an
/// off-allowlist host. Network I/O — integration-tested, not unit-tested.
pub async fn download_to_partial<F: Fn(u64, u64)>(
    url: &str,
    partial: &Path,
    on_progress: F,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    if !host_allowed(url) {
        bail!("refusing to download from a non-allowlisted host: {url}");
    }
    let already = std::fs::metadata(partial).map(|m| m.len()).unwrap_or(0);

    // No TOTAL request timeout here — a multi-GB weights download legitimately
    // runs for a long time (resume covers interruptions). Connect timeout +
    // per-hop redirect re-check only.
    let client = reqwest::Client::builder()
        .user_agent("lost-harness/0.1 (model-downloader)")
        .connect_timeout(std::time::Duration::from_secs(15))
        .redirect(allowlisted_redirect_policy())
        .build()?;
    let mut req = client.get(url);
    if already > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={already}-"));
    }
    let resp = req.send().await?.error_for_status()?;
    // Total = already-have + what the server will send (Content-Length of the
    // ranged remainder), best-effort.
    let remaining = resp.content_length().unwrap_or(0);
    let total = already + remaining;

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true) // resume: append to the existing partial
        .open(partial)
        .await?;
    let mut downloaded = already;
    let mut stream = resp.bytes_stream();
    use tokio_stream::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        on_progress(downloaded, total);
    }
    file.flush().await?;
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
}
