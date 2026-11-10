#![cfg_attr(coverage, feature(coverage_attribute))]
//! tamias-sign — optional content signing for published packages, via
//! `ssh-keygen -Y sign`/`-Y verify` (the same mechanism sturn uses for
//! its WAL ops; SSH signing is stdlib tooling — Ed25519 under the
//! hood, no new crypto deps).
//!
//! **What is signed.** The canonical payload is the package's
//! provenance anchors — name, scope, version, and the `sha256:` digest
//! of the *unpacked* payload tree ([`tamias_core::dir_digest`]):
//!
//! ```text
//! tamias-package-v1
//! name: <bare name>
//! scope: <scope | ->
//! version: <version | ->
//! digest: sha256:<hex>
//! ```
//!
//! The digest is the same value the registry index records and the
//! store record carries, so a signature binds "this exact payload,
//! under this name@version" — tampering with either side fails
//! verification. See [`signing_bytes`].
//!
//! **Transport.** Signatures ride the registry index: `publish --sign`
//! stores the armored PEM in the entry's `signature` field, which
//! `index.json` carries to every `cf:`/registry install. Installed
//! entries keep it in the store record so `tamias verify` can re-check.
//!
//! **Boundary (v1).** Only registry-published packages (`cf:` refs)
//! can carry a signature — a signature can't live inside a `git:` or
//! bare `https:` ref without a sidecar convention, so those verify by
//! digest-pin alone. `signing.required` therefore effectively mandates
//! registry-published packages. cosign/OCI signing is future work —
//! it needs its own binary + identity model.
//!
//! **Secrets.** The private key is only ever a *path* — resolved from
//! `--key`, `$TAMIAS_SIGNING_KEY` (whose value is a path),
//! `signing.key_env`'s named env var, or `signing.key`. Key material
//! never appears in config, args, or logs.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use tamias_core::{dir_digest, Error, Manifest, StoreRecord};

/// The `ssh-keygen -Y` namespace signatures are made under — a
/// signature minted for `tamias-package` can't be replayed as another
/// namespace's (sturn uses `sturn-event` the same way).
pub const DEFAULT_NAMESPACE: &str = "tamias-package";

/// Env var whose *value* is the signing-key path — checked before
/// `signing.key_env`/`signing.key` on `publish --sign`.
pub const SIGNING_KEY_ENV: &str = "TAMIAS_SIGNING_KEY";

/// Env var naming an `allowed_signers` file — wins over
/// `signing.allowed_signers` (the env-wins rule the config's
/// store/cache/registry resolution follows).
pub const ALLOWED_SIGNERS_ENV: &str = "TAMIAS_ALLOWED_SIGNERS";

/// Env var whose value is an `official_signers` file — the smaller
/// trust set backing the official-badge check (P-005). Wins over
/// `signing.official_signers`, same env-wins rule as
/// [`ALLOWED_SIGNERS_ENV`].
pub const OFFICIAL_SIGNERS_ENV: &str = "TAMIAS_OFFICIAL_SIGNERS";

/// Canonical bytes for signing — the provenance anchors, one
/// `key: value` per line, `-` for absent optional fields. Fixed
/// order, no trailing ambiguity; both sides build it from the same
/// inputs so it can't drift.
#[must_use]
pub fn signing_bytes(
    name: &str,
    scope: Option<&str>,
    version: Option<&str>,
    digest: &str,
) -> Vec<u8> {
    format!(
        "tamias-package-v1\nname: {name}\nscope: {}\nversion: {}\ndigest: {digest}\n",
        scope.unwrap_or("-"),
        version.unwrap_or("-")
    )
    .into_bytes()
}

/// The canonical bytes for a store record — what `tamias verify`
/// re-checks a recorded signature against. `record.digest` is the
/// payload digest the signature bound at install time.
#[must_use]
pub fn record_signing_bytes(record: &StoreRecord) -> Vec<u8> {
    signing_bytes(
        &record.name,
        record.scope.as_deref(),
        record.version.as_deref(),
        &record.digest,
    )
}

/// Sign `bytes` with the SSH private key at `key` —
/// `ssh-keygen -Y sign -n <namespace>`. Returns the armored signature
/// (`-----BEGIN SSH SIGNATURE-----`).
///
/// # Errors
/// Fails when `ssh-keygen` is absent or refuses the key/payload.
pub fn sign(bytes: &[u8], key: &Path, namespace: &str) -> Result<String, Error> {
    let data = scratch_file("tamias-sign")?;
    std::fs::write(&data, bytes).map_err(Error::io(format!("write {}", data.display())))?;
    let result = Command::new("ssh-keygen")
        .args(["-Y", "sign", "-f"])
        .arg(key)
        .args(["-n", namespace])
        .arg(&data)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let sig_path = PathBuf::from(format!("{}.sig", data.display()));
    let sig = std::fs::read_to_string(&sig_path).ok();
    let _ = std::fs::remove_file(&data);
    let _ = std::fs::remove_file(&sig_path);
    match result {
        Ok(status) if status.success() => {
            sig.ok_or_else(|| Error::Failed("ssh-keygen -Y sign produced no signature".to_string()))
        }
        Ok(_) => Err(Error::Failed(format!(
            "ssh-keygen -Y sign failed for key {}",
            key.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::Failed(
            "ssh-keygen not found — install openssh to use package signing".to_string(),
        )),
        Err(e) => Err(Error::io("run ssh-keygen -Y sign")(e)),
    }
}

/// Verify `signature` (armored PEM) over `bytes` against the
/// `allowed_signers` file — `ssh-keygen -Y verify -I <principal>`.
/// `principal` selects the allowed_signers line; tamias uses the bare
/// package name, so a key authorized for `ci-gate` can't vouch for
/// `other-pkg`.
///
/// # Errors
/// Fails when `ssh-keygen` is absent or can't run; `Ok(false)` means
/// the signature didn't verify (wrong key, tampered payload, unknown
/// principal).
pub fn verify(
    bytes: &[u8],
    signature: &str,
    allowed_signers: &Path,
    principal: &str,
    namespace: &str,
) -> Result<bool, Error> {
    let data = scratch_file("tamias-verify")?;
    let sig = scratch_file("tamias-verify-sig")?;
    std::fs::write(&data, bytes).map_err(Error::io(format!("write {}", data.display())))?;
    std::fs::write(&sig, signature).map_err(Error::io(format!("write {}", sig.display())))?;
    let input =
        std::fs::File::open(&data).map_err(Error::io(format!("open {}", data.display())))?;
    let status = Command::new("ssh-keygen")
        .args(["-Y", "verify", "-f"])
        .arg(allowed_signers)
        .args(["-I", principal, "-n", namespace, "-s"])
        .arg(&sig)
        .stdin(Stdio::from(input))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::fs::remove_file(&data);
    let _ = std::fs::remove_file(&sig);
    match status {
        Ok(s) => Ok(s.success()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::Failed(
            "ssh-keygen not found — install openssh to verify package signatures".to_string(),
        )),
        Err(e) => Err(Error::io("run ssh-keygen -Y verify")(e)),
    }
}

/// A unique scratch path in the temp dir — concurrent sign/verify
/// calls in one process (tests, `tamias serve`) must not share files.
fn scratch_file(tag: &str) -> Result<PathBuf, Error> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    Ok(std::env::temp_dir().join(format!("{tag}-{}-{nanos}-{seq}", std::process::id())))
}

/// `~` / `~/…` expansion for config-supplied paths (`$HOME`, else
/// `/` — mirrors `tamias_source::expand_path` without the dep).
#[must_use]
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(path)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// The resolved verification policy — what installs and `tamias
/// verify` enforce. Built from config + env by [`SigningPolicy::resolve`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigningPolicy {
    /// `allowed_signers` path (`~` expanded) — `None` when nothing
    /// configured and the default file doesn't exist.
    pub allowed_signers: Option<PathBuf>,
    /// `official_signers` path (`~` expanded) — the smaller trust set
    /// for the official-badge check; `None` when nothing configured
    /// and the default file doesn't exist.
    pub official_signers: Option<PathBuf>,
    /// Every install must carry a valid signature (only `cf:`/registry
    /// refs can supply one in v1 — see the crate docs).
    pub required: bool,
    /// The `ssh-keygen -Y` namespace signatures verify under.
    pub namespace: String,
}

impl SigningPolicy {
    /// Resolve from `config` + env:
    ///
    /// - `allowed_signers`: `$TAMIAS_ALLOWED_SIGNERS` →
    ///   `signing.allowed_signers` → `~/.config/tamias/allowed_signers`
    ///   (kept only when the file exists — a configured-but-missing
    ///   path still wins so the error names it).
    /// - `official_signers`: `$TAMIAS_OFFICIAL_SIGNERS` →
    ///   `signing.official_signers` → `~/.config/tamias/official_signers`
    ///   — same precedence and keep-only-when-it-exists rule.
    /// - `required`/`namespace`: `signing.required`,
    ///   `signing.namespace` or [`DEFAULT_NAMESPACE`].
    #[must_use]
    pub fn resolve(config: Option<&tamias_config::Config>) -> Self {
        let signing = config.map(|c| &c.signing);
        let allowed_signers = std::env::var(ALLOWED_SIGNERS_ENV)
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| expand_tilde(&v))
            .or_else(|| {
                signing
                    .and_then(|s| s.allowed_signers.as_deref())
                    .map(expand_tilde)
            })
            .or_else(|| {
                let p = home_dir().join(".config/tamias/allowed_signers");
                p.is_file().then_some(p)
            });
        let official_signers = std::env::var(OFFICIAL_SIGNERS_ENV)
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| expand_tilde(&v))
            .or_else(|| {
                signing
                    .and_then(|s| s.official_signers.as_deref())
                    .map(expand_tilde)
            })
            .or_else(|| {
                let p = home_dir().join(".config/tamias/official_signers");
                p.is_file().then_some(p)
            });
        Self {
            allowed_signers,
            official_signers,
            required: signing.is_some_and(|s| s.required),
            namespace: signing
                .and_then(|s| s.namespace.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| DEFAULT_NAMESPACE.to_string()),
        }
    }
}

impl Default for SigningPolicy {
    /// The no-config policy — optional signing, default namespace.
    fn default() -> Self {
        Self::resolve(None)
    }
}

/// The private-key path for `publish --sign`: explicit `--key` →
/// `$TAMIAS_SIGNING_KEY` (value is a path) → the env var named by
/// `signing.key_env` → `signing.key` → `~/.ssh/id_ed25519`.
///
/// # Errors
/// Fails when no candidate exists — the message lists what was tried.
pub fn resolve_signing_key(
    config: Option<&tamias_config::Config>,
    explicit: Option<&Path>,
) -> Result<PathBuf, Error> {
    if let Some(key) = explicit {
        return Ok(key.to_path_buf());
    }
    let signing = config.map(|c| &c.signing);
    let candidates: Vec<(&'static str, PathBuf)> = [
        (SIGNING_KEY_ENV, std::env::var(SIGNING_KEY_ENV).ok()),
        (
            "signing.key_env",
            signing
                .and_then(|s| s.key_env.as_deref())
                .and_then(|var| std::env::var(var).ok()),
        ),
        ("signing.key", signing.and_then(|s| s.key.clone())),
    ]
    .into_iter()
    .filter_map(|(label, value)| {
        value
            .filter(|v| !v.is_empty())
            .map(|v| (label, expand_tilde(&v)))
    })
    .collect();
    for (_, path) in &candidates {
        if path.is_file() {
            return Ok(path.clone());
        }
    }
    let fallback = home_dir().join(".ssh/id_ed25519");
    if fallback.is_file() {
        return Ok(fallback);
    }
    let tried = if candidates.is_empty() {
        "$TAMIAS_SIGNING_KEY, signing.key_env, signing.key, ~/.ssh/id_ed25519".to_string()
    } else {
        candidates
            .iter()
            .map(|(label, path)| format!("{label} ({})", path.display()))
            .collect::<Vec<_>>()
            .join(", ")
            + ", ~/.ssh/id_ed25519"
    };
    Err(Error::Failed(format!(
        "no signing key found — tried {tried}; set {SIGNING_KEY_ENV} to the key path"
    )))
}

/// What a signature check concluded — the status `tamias verify`
/// reports and the install pipeline enforces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureStatus {
    /// Signature present and `ssh-keygen -Y verify` passed.
    Verified,
    /// Signature present but verification failed — wrong signer,
    /// tampered payload, or unknown principal.
    Failed,
    /// No signature on the package.
    Unsigned,
    /// Signature present but no `allowed_signers` is configured —
    /// can't be checked either way.
    Unchecked,
}

impl SignatureStatus {
    /// Stable wire/report spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Failed => "bad",
            Self::Unsigned => "unsigned",
            Self::Unchecked => "unchecked",
        }
    }
}

/// Check a package's signature status against `allowed_signers`
/// (`None` → [`SignatureStatus::Unchecked`] when a sig is present).
/// The payload signed is the canonical [`signing_bytes`] of the
/// package's provenance anchors.
///
/// **Principal resolution.** Scoped packages (`@scope/name`) prefer
/// the scope-qualified principal `@scope/name` — authorization in
/// `allowed_signers` is per-principal, so `@acme/w` and `@other/w`
/// don't share a signer line. For backward compatibility with
/// allowed_signers files that only list the bare name, a scoped
/// package falls back to the bare `name` principal when the scoped
/// principal doesn't verify — a package verifies if *either* line
/// authorizes the key. Unscoped packages use the bare name only.
/// (The signed bytes already bind scope, so this is authorization
/// granularity, not replay protection.)
///
/// # Errors
/// Propagates `ssh-keygen` spawn/IO failures from [`verify`].
pub fn signature_status(
    name: &str,
    scope: Option<&str>,
    version: Option<&str>,
    digest: &str,
    signature: Option<&str>,
    allowed_signers: Option<&Path>,
    namespace: &str,
) -> Result<SignatureStatus, Error> {
    signature_status_detail(
        name,
        scope,
        version,
        digest,
        signature,
        allowed_signers,
        namespace,
    )
    .map(|(status, _)| status)
}

/// [`signature_status`] plus *which* principal verified — the
/// `@scope/name` or bare `name` allowed_signers line whose key
/// checked out (`None` unless the status is
/// [`SignatureStatus::Verified`]). Reporting surfaces (`tamias
/// verify`, the [`TrustTier`] computation, install-time `signer`
/// recording) use this to name who vouched for a package.
///
/// # Errors
/// Propagates `ssh-keygen` spawn/IO failures from [`verify`].
pub fn signature_status_detail(
    name: &str,
    scope: Option<&str>,
    version: Option<&str>,
    digest: &str,
    signature: Option<&str>,
    allowed_signers: Option<&Path>,
    namespace: &str,
) -> Result<(SignatureStatus, Option<String>), Error> {
    let Some(sig) = signature else {
        return Ok((SignatureStatus::Unsigned, None));
    };
    let Some(signers) = allowed_signers else {
        return Ok((SignatureStatus::Unchecked, None));
    };
    if !signers.is_file() {
        return Ok((SignatureStatus::Unchecked, None));
    }
    let bytes = signing_bytes(name, scope, version, digest);
    // Scoped packages try `@scope/name` first, then the bare-name
    // fallback — the first principal that verifies is reported.
    let mut principals = Vec::with_capacity(2);
    if let Some(scope) = scope {
        principals.push(format!("@{scope}/{name}"));
    }
    principals.push(name.to_string());
    for principal in &principals {
        if verify(&bytes, sig, signers, principal, namespace)? {
            return Ok((SignatureStatus::Verified, Some(principal.clone())));
        }
    }
    Ok((SignatureStatus::Failed, None))
}

/// The store-record convenience form of [`signature_status`].
pub fn record_signature_status(
    record: &StoreRecord,
    allowed_signers: Option<&Path>,
    namespace: &str,
) -> Result<SignatureStatus, Error> {
    signature_status(
        &record.name,
        record.scope.as_deref(),
        record.version.as_deref(),
        &record.digest,
        record.signature.as_deref(),
        allowed_signers,
        namespace,
    )
}

/// Signature status against the policy's `official_signers` trust
/// set — the official-badge check (P-005). Same principal rules as
/// [`signature_status`]. Returns [`SignatureStatus::Unchecked`] when
/// no `official_signers` is configured (or the file is missing) —
/// `signature_status` already arms that way for `None`.
pub fn official_signature_status(
    name: &str,
    scope: Option<&str>,
    version: Option<&str>,
    digest: &str,
    signature: Option<&str>,
    policy: &SigningPolicy,
) -> Result<SignatureStatus, Error> {
    signature_status(
        name,
        scope,
        version,
        digest,
        signature,
        policy.official_signers.as_deref(),
        &policy.namespace,
    )
}

/// The trust tier a package's provenance earns — the badge
/// `tamias verify` and install records report (P-005 §A). Highest
/// wins:
///
/// - [`TrustTier::Official`] — signature present and verified
///   against `policy.official_signers`.
/// - [`TrustTier::Signed`] — signature present and verified against
///   `policy.allowed_signers`, but not the official set.
/// - [`TrustTier::Bad`] — signature present and verification
///   *failed* against every configured signers file. A failed
///   signature must never display as merely unsigned.
/// - [`TrustTier::Pinned`] — no verifiable signature, but
///   `source_ref` carries an integrity pin (see [`ref_is_pinned`]).
/// - [`TrustTier::Unpinned`] — everything else: a moving git
///   branch/tag, bare http(s), fs paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustTier {
    /// Verified against the `official_signers` trust set.
    Official,
    /// Verified against `allowed_signers` (not official).
    Signed,
    /// No signature, but the source ref pins content integrity.
    Pinned,
    /// No signature and no integrity pin.
    Unpinned,
    /// Signature present but verification failed.
    Bad,
}

impl TrustTier {
    /// Stable wire/report spelling — what `StoreRecord.trust`
    /// records and `tamias verify --json` emits.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Official => "official",
            Self::Signed => "signed",
            Self::Pinned => "pinned",
            Self::Unpinned => "unpinned",
            Self::Bad => "bad",
        }
    }
}

/// Whether `source_ref` carries an integrity pin — the
/// [`TrustTier::Pinned`] heuristic:
///
/// - `cf:` refs — the registry index digest pins the payload.
/// - `#sha256:<hex>` / `#sha256=<hex>` fragments — pinned http
///   tarballs.
/// - `#` + a full 40-hex sha — a pinned git commit (mirrors
///   `tamias-source-git`'s `is_full_sha`: lowercase hex only).
fn ref_is_pinned(source_ref: &str) -> bool {
    if source_ref.starts_with("cf:") {
        return true;
    }
    let Some((_, fragment)) = source_ref.split_once('#') else {
        return false;
    };
    if fragment.starts_with("sha256:") || fragment.starts_with("sha256=") {
        return true;
    }
    fragment.len() == 40
        && fragment
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// The trust-tier computation shared by [`trust_tier`] and
/// [`trust_and_signer`]. A present signature is checked against
/// `official_signers` first (short-circuit on Verified), then
/// `allowed_signers`; a failure against every configured file is
/// [`TrustTier::Bad`]. With no signers file at all the signature is
/// *unchecked* — not failed — so the pin heuristic decides (an
/// `Unchecked` signature is already a hard install error via
/// [`enforce_signature`]; reaching here unchecked means a
/// hand-edited record under re-verification). No signature skips
/// straight to the pin heuristic.
fn assess_trust(
    source_ref: &str,
    signature: Option<&str>,
    name: &str,
    scope: Option<&str>,
    version: Option<&str>,
    digest: &str,
    policy: &SigningPolicy,
) -> Result<(TrustTier, Option<String>), Error> {
    if let Some(sig) = signature {
        let mut checked = false;
        let mut failed = false;
        for (official, signers) in [
            (true, policy.official_signers.as_deref()),
            (false, policy.allowed_signers.as_deref()),
        ] {
            let Some(signers) = signers.filter(|p| p.is_file()) else {
                continue;
            };
            checked = true;
            let (status, principal) = signature_status_detail(
                name,
                scope,
                version,
                digest,
                Some(sig),
                Some(signers),
                &policy.namespace,
            )?;
            match status {
                SignatureStatus::Verified => {
                    let tier = if official {
                        TrustTier::Official
                    } else {
                        TrustTier::Signed
                    };
                    return Ok((tier, principal));
                }
                SignatureStatus::Failed => failed = true,
                _ => {}
            }
        }
        if checked && failed {
            return Ok((TrustTier::Bad, None));
        }
    }
    let tier = if ref_is_pinned(source_ref) {
        TrustTier::Pinned
    } else {
        TrustTier::Unpinned
    };
    Ok((tier, None))
}

/// The package's [`TrustTier`] — **reporting only**; the install
/// gate stays [`enforce_signature`]. `source_ref` is the ref the
/// install was invoked with (pin detection reads it verbatim);
/// `digest` is the payload digest the signature bound.
///
/// # Errors
/// Propagates `ssh-keygen` spawn/IO failures from the signature
/// checks.
pub fn trust_tier(
    source_ref: &str,
    signature: Option<&str>,
    name: &str,
    scope: Option<&str>,
    version: Option<&str>,
    digest: &str,
    policy: &SigningPolicy,
) -> Result<TrustTier, Error> {
    Ok(assess_trust(source_ref, signature, name, scope, version, digest, policy)?.0)
}

/// [`trust_tier`] plus the verified signing principal in one pass —
/// the `@scope/name` or bare `name` that checked out (official set
/// preferred), `None` for unsigned/failed tiers. The install
/// pipeline records both on the [`StoreRecord`].
///
/// # Errors
/// Propagates `ssh-keygen` spawn/IO failures from the signature
/// checks.
pub fn trust_and_signer(
    source_ref: &str,
    signature: Option<&str>,
    name: &str,
    scope: Option<&str>,
    version: Option<&str>,
    digest: &str,
    policy: &SigningPolicy,
) -> Result<(TrustTier, Option<String>), Error> {
    assess_trust(source_ref, signature, name, scope, version, digest, policy)
}

/// Sign a payload dir's canonical provenance — `dir_digest(root)` is
/// the same digest the registry index records and the store record
/// carries, so the signature binds exactly what install/verify check.
///
/// # Errors
/// Propagates digest/sign failures.
pub fn sign_package(
    root: &Path,
    manifest: &Manifest,
    key: &Path,
    namespace: &str,
) -> Result<String, Error> {
    let digest = dir_digest(root)?;
    sign(
        &signing_bytes(
            &manifest.name,
            manifest.scope.as_deref(),
            manifest.version.as_deref(),
            &digest,
        ),
        key,
        namespace,
    )
}

/// The install-pipeline gate — call after the payload manifest loads,
/// before driver dispatch and the store commit. `signature` is what
/// the source carried (registry index entries today); `payload_digest`
/// is `dir_digest` of the payload root.
///
/// # Errors
/// Fails on a bad signature, a signed package with no usable
/// `allowed_signers`, or — under `policy.required` — any unsigned
/// install.
pub fn enforce_signature(
    name: &str,
    scope: Option<&str>,
    version: Option<&str>,
    payload_digest: &str,
    signature: Option<&str>,
    policy: &SigningPolicy,
) -> Result<(), Error> {
    match signature_status(
        name,
        scope,
        version,
        payload_digest,
        signature,
        policy.allowed_signers.as_deref(),
        &policy.namespace,
    )? {
        SignatureStatus::Verified => Ok(()),
        SignatureStatus::Unsigned if !policy.required => Ok(()),
        SignatureStatus::Unsigned => Err(Error::Failed(format!(
            "package '{name}' is unsigned and signing.required is set — \
             only signed registry (`cf:`) packages can be installed"
        ))),
        SignatureStatus::Failed => Err(Error::Failed(format!(
            "package '{name}': signature verification failed \
             (allowed_signers: {})",
            policy
                .allowed_signers
                .as_ref()
                .map_or_else(|| "?".to_string(), |p| p.display().to_string())
        ))),
        SignatureStatus::Unchecked => Err(Error::Failed(format!(
            "package '{name}' is signed but no allowed_signers is configured — \
             set signing.allowed_signers, {ALLOWED_SIGNERS_ENV}, or \
             ~/.config/tamias/allowed_signers"
        ))),
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// `ssh-keygen -Y` needs a real key — generate one per test run
    /// (same approach as sturn's wal tests; ssh-keygen is already a
    /// de-facto dep for git-over-ssh).
    fn gen_key(dir: &Path, name: &str) -> PathBuf {
        let key = dir.join(name);
        let out = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "ssh-keygen: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        key
    }

    /// The allowed_signers line for `principal` + `key` —
    /// `<principal> <keytype> <base64>` via `ssh-keygen -y`.
    fn allow(dir: &Path, principal: &str, key: &Path) -> PathBuf {
        // principals may be `@scope/name` — `/` can't appear in a filename
        let name = format!("allowed-{}", principal.replace('/', "_"));
        allow_file(dir, &name, principal, key)
    }

    /// [`allow`] with an explicit filename — for tests that need two
    /// signers files authorizing the *same* principal (e.g. an
    /// `allowed_signers` and an `official_signers` holding different
    /// keys for `pkg`).
    fn allow_file(dir: &Path, file: &str, principal: &str, key: &Path) -> PathBuf {
        let pubout = Command::new("ssh-keygen")
            .args(["-y", "-f"])
            .arg(key)
            .output()
            .unwrap();
        assert!(pubout.status.success());
        let pubkey = String::from_utf8_lossy(&pubout.stdout).trim().to_string();
        let path = dir.join(file);
        std::fs::write(&path, format!("{principal} {pubkey}\n")).unwrap();
        path
    }

    #[test]
    fn signing_bytes_is_canonical() {
        let b = signing_bytes("w", Some("acme"), Some("1.0.0"), "sha256:aa");
        assert_eq!(
            String::from_utf8(b).unwrap(),
            "tamias-package-v1\nname: w\nscope: acme\nversion: 1.0.0\ndigest: sha256:aa\n"
        );
        let b2 = signing_bytes("w", None, None, "sha256:aa");
        assert!(String::from_utf8(b2)
            .unwrap()
            .contains("scope: -\nversion: -\n"));
    }

    #[test]
    fn sign_verify_roundtrip_and_failures() {
        let dir = tempfile::tempdir().unwrap();
        let key = gen_key(dir.path(), "k1");
        let other = gen_key(dir.path(), "k2");
        let signers = allow(dir.path(), "pkg", &key);
        let bytes = signing_bytes("pkg", None, Some("1.0.0"), "sha256:abc");

        let sig = sign(&bytes, &key, DEFAULT_NAMESPACE).unwrap();
        assert!(sig.contains("BEGIN SSH SIGNATURE"));
        assert!(verify(&bytes, &sig, &signers, "pkg", DEFAULT_NAMESPACE).unwrap());

        // wrong signer key → the allowed_signers line doesn't match
        let signers_other = allow(dir.path(), "pkg", &other);
        assert!(!verify(&bytes, &sig, &signers_other, "pkg", DEFAULT_NAMESPACE).unwrap());
        // tampered payload → bad
        let tampered = signing_bytes("pkg", None, Some("1.0.1"), "sha256:abc");
        assert!(!verify(&tampered, &sig, &signers, "pkg", DEFAULT_NAMESPACE).unwrap());
        // wrong principal → no allowed_signers line for it
        assert!(!verify(&bytes, &sig, &signers, "other", DEFAULT_NAMESPACE).unwrap());
        // wrong namespace → rejected
        assert!(!verify(&bytes, &sig, &signers, "pkg", "other-ns").unwrap());
    }

    #[test]
    fn signature_status_arms() {
        let dir = tempfile::tempdir().unwrap();
        let key = gen_key(dir.path(), "k");
        let signers = allow(dir.path(), "pkg", &key);
        let bytes = signing_bytes("pkg", None, None, "sha256:d");
        let sig = sign(&bytes, &key, DEFAULT_NAMESPACE).unwrap();

        assert_eq!(
            signature_status(
                "pkg",
                None,
                None,
                "sha256:d",
                None,
                Some(&signers),
                DEFAULT_NAMESPACE
            )
            .unwrap(),
            SignatureStatus::Unsigned
        );
        assert_eq!(
            signature_status(
                "pkg",
                None,
                None,
                "sha256:d",
                Some(&sig),
                Some(&signers),
                DEFAULT_NAMESPACE
            )
            .unwrap(),
            SignatureStatus::Verified
        );
        // present but uncheckable
        assert_eq!(
            signature_status(
                "pkg",
                None,
                None,
                "sha256:d",
                Some(&sig),
                None,
                DEFAULT_NAMESPACE
            )
            .unwrap(),
            SignatureStatus::Unchecked
        );
        // bad digest → Failed
        assert_eq!(
            signature_status(
                "pkg",
                None,
                None,
                "sha256:evil",
                Some(&sig),
                Some(&signers),
                DEFAULT_NAMESPACE
            )
            .unwrap(),
            SignatureStatus::Failed
        );
    }

    #[test]
    fn enforce_signature_policies() {
        let dir = tempfile::tempdir().unwrap();
        let key = gen_key(dir.path(), "k");
        let signers = allow(dir.path(), "pkg", &key);
        let bytes = signing_bytes("pkg", None, None, "sha256:d");
        let sig = sign(&bytes, &key, DEFAULT_NAMESPACE).unwrap();

        let mut policy = SigningPolicy {
            allowed_signers: Some(signers.clone()),
            ..Default::default()
        };
        // unsigned + optional → ok
        assert!(enforce_signature("pkg", None, None, "sha256:d", None, &policy).is_ok());
        // unsigned + required → err
        policy.required = true;
        assert!(enforce_signature("pkg", None, None, "sha256:d", None, &policy).is_err());
        // signed + verified → ok even under required
        assert!(enforce_signature("pkg", None, None, "sha256:d", Some(&sig), &policy).is_ok());
        // signed + no allowed_signers → err
        policy.allowed_signers = None;
        assert!(enforce_signature("pkg", None, None, "sha256:d", Some(&sig), &policy).is_err());
        // bad sig → err
        policy.allowed_signers = Some(signers);
        let bad = signing_bytes("pkg", None, None, "sha256:other");
        let badsig = sign(&bad, &key, DEFAULT_NAMESPACE).unwrap();
        assert!(enforce_signature("pkg", None, None, "sha256:d", Some(&badsig), &policy).is_err());
    }

    #[test]
    fn resolve_signing_key_order() {
        // explicit wins outright
        let dir = tempfile::tempdir().unwrap();
        let key = gen_key(dir.path(), "k");
        assert_eq!(resolve_signing_key(None, Some(&key)).unwrap(), key);
        // nothing configured + no ~/.ssh/id_ed25519 guarantee → may err;
        // just assert it doesn't panic and returns a path or a clean error
        let _ = resolve_signing_key(None, None);
    }

    #[test]
    fn sign_package_uses_payload_digest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tamias.yaml"), "name: p\nversion: 1.0.0\n").unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        let key = gen_key(dir.path(), "k");
        let signers = allow(dir.path(), "p", &key);
        let manifest = Manifest::load(dir.path()).unwrap();
        let sig = sign_package(dir.path(), &manifest, &key, DEFAULT_NAMESPACE).unwrap();
        let digest = dir_digest(dir.path()).unwrap();
        let status = signature_status(
            "p",
            None,
            Some("1.0.0"),
            &digest,
            Some(&sig),
            Some(&signers),
            DEFAULT_NAMESPACE,
        )
        .unwrap();
        assert_eq!(status, SignatureStatus::Verified);
    }

    #[test]
    fn scoped_principal_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let key = gen_key(dir.path(), "k");
        let other = gen_key(dir.path(), "k2");
        let bytes = signing_bytes("w", Some("acme"), Some("1.0.0"), "sha256:d");
        let sig = sign(&bytes, &key, DEFAULT_NAMESPACE).unwrap();
        let status = |signers: &Path| {
            signature_status(
                "w",
                Some("acme"),
                Some("1.0.0"),
                "sha256:d",
                Some(&sig),
                Some(signers),
                DEFAULT_NAMESPACE,
            )
            .unwrap()
        };

        // (a) a `@acme/w` principal line verifies the scoped package
        let scoped = allow(dir.path(), "@acme/w", &key);
        assert_eq!(status(&scoped), SignatureStatus::Verified);

        // (b) a bare `w` line still verifies it — the fallback keeps
        // pre-scope allowed_signers files working
        let bare = allow(dir.path(), "w", &key);
        assert_eq!(status(&bare), SignatureStatus::Verified);

        // (c) a key authorized only for `@other/w` does NOT verify
        // `@acme/w` — scope-qualified authorization
        let wrong_scope = allow(dir.path(), "@other/w", &key);
        assert_eq!(status(&wrong_scope), SignatureStatus::Failed);
        // same for the wrong *key* under the right principal
        let wrong_key = allow(dir.path(), "@acme/w", &other);
        assert_eq!(status(&wrong_key), SignatureStatus::Failed);
    }

    #[test]
    fn signature_status_detail_reports_principal() {
        let dir = tempfile::tempdir().unwrap();
        let key = gen_key(dir.path(), "k");
        let bytes = signing_bytes("w", Some("acme"), None, "sha256:d");
        let sig = sign(&bytes, &key, DEFAULT_NAMESPACE).unwrap();
        let detail = |signers: &Path| {
            signature_status_detail(
                "w",
                Some("acme"),
                None,
                "sha256:d",
                Some(&sig),
                Some(signers),
                DEFAULT_NAMESPACE,
            )
            .unwrap()
        };
        // scoped principal line → reported verbatim
        let scoped = allow(dir.path(), "@acme/w", &key);
        assert_eq!(
            detail(&scoped),
            (SignatureStatus::Verified, Some("@acme/w".to_string()))
        );
        // bare-name fallback → the bare principal is reported
        let bare = allow(dir.path(), "w", &key);
        assert_eq!(
            detail(&bare),
            (SignatureStatus::Verified, Some("w".to_string()))
        );
        // unsigned / failed arms carry no principal
        assert_eq!(
            signature_status_detail(
                "w",
                Some("acme"),
                None,
                "sha256:d",
                None,
                Some(&bare),
                DEFAULT_NAMESPACE,
            )
            .unwrap(),
            (SignatureStatus::Unsigned, None)
        );
        let wrong = allow(dir.path(), "@acme/w", &gen_key(dir.path(), "k2"));
        assert_eq!(detail(&wrong), (SignatureStatus::Failed, None));
    }

    #[test]
    fn trust_tier_signed_official_and_bad() {
        let dir = tempfile::tempdir().unwrap();
        let key = gen_key(dir.path(), "k");
        let official_key = gen_key(dir.path(), "ok");
        let rogue = gen_key(dir.path(), "rk");
        // `key` is allowed-but-not-official; `official_key` is in the
        // official set; `rogue` is in neither.
        let allowed = allow(dir.path(), "pkg", &key);
        let official = allow_file(dir.path(), "official-signers", "pkg", &official_key);
        let policy = SigningPolicy {
            allowed_signers: Some(allowed),
            official_signers: Some(official),
            required: false,
            namespace: DEFAULT_NAMESPACE.to_string(),
        };
        let bytes = signing_bytes("pkg", None, None, "sha256:d");
        let sig = sign(&bytes, &key, DEFAULT_NAMESPACE).unwrap();
        let osig = sign(&bytes, &official_key, DEFAULT_NAMESPACE).unwrap();
        let rsig = sign(&bytes, &rogue, DEFAULT_NAMESPACE).unwrap();
        // a sig over different anchors fails everywhere
        let wrong = sign(
            &signing_bytes("pkg", None, None, "sha256:other"),
            &key,
            DEFAULT_NAMESPACE,
        )
        .unwrap();
        let tier = |source_ref: &str, s: Option<&str>| {
            trust_tier(source_ref, s, "pkg", None, None, "sha256:d", &policy).unwrap()
        };

        // official signature → Official (checked first, short-circuits)
        assert_eq!(tier("cf:reg/pkg@1.0.0", Some(&osig)), TrustTier::Official);
        // allowed-only signature → Signed
        assert_eq!(tier("cf:reg/pkg@1.0.0", Some(&sig)), TrustTier::Signed);
        // signature fails both configured files → Bad, even on a
        // pinned ref — a bad sig never displays as merely unsigned
        assert_eq!(tier("cf:reg/pkg@1.0.0", Some(&rsig)), TrustTier::Bad);
        assert_eq!(tier("cf:reg/pkg@1.0.0", Some(&wrong)), TrustTier::Bad);
        // trust_and_signer reports the verifying principal
        let (t, who) = trust_and_signer(
            "cf:reg/pkg@1.0.0",
            Some(&sig),
            "pkg",
            None,
            None,
            "sha256:d",
            &policy,
        )
        .unwrap();
        assert_eq!((t, who.as_deref()), (TrustTier::Signed, Some("pkg")));
        let (t, who) = trust_and_signer(
            "cf:reg/pkg@1.0.0",
            Some(&osig),
            "pkg",
            None,
            None,
            "sha256:d",
            &policy,
        )
        .unwrap();
        assert_eq!((t, who.as_deref()), (TrustTier::Official, Some("pkg")));
    }

    #[test]
    fn trust_tier_pinned_and_unpinned() {
        // no signers configured at all — signature presence can't be
        // checked, so the pin heuristic decides
        let policy = SigningPolicy {
            allowed_signers: None,
            official_signers: None,
            required: false,
            namespace: DEFAULT_NAMESPACE.to_string(),
        };
        let tier = |source_ref: &str| {
            trust_tier(source_ref, None, "pkg", None, None, "sha256:d", &policy).unwrap()
        };
        // pinned arms
        assert_eq!(tier("cf:reg/pkg@1.0.0"), TrustTier::Pinned);
        assert_eq!(
            tier("https://h/x.tgz#sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            TrustTier::Pinned
        );
        assert_eq!(
            tier("https://h/x.tgz#sha256=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            TrustTier::Pinned
        );
        assert_eq!(
            tier("git:https://h/org/repo#0123456789abcdef0123456789abcdef01234567"),
            TrustTier::Pinned
        );
        // unpinned arms
        assert_eq!(tier("git:https://h/org/repo#main"), TrustTier::Unpinned);
        assert_eq!(tier("git:https://h/org/repo"), TrustTier::Unpinned);
        assert_eq!(tier("https://h/x.tgz"), TrustTier::Unpinned);
        assert_eq!(tier("./local/pkg"), TrustTier::Unpinned);
        // a short hex rev is NOT a full-sha pin
        assert_eq!(tier("git:https://h/org/repo#0123456"), TrustTier::Unpinned);
        // an unverifiable signature falls through to the pin heuristic
        let dir = tempfile::tempdir().unwrap();
        let key = gen_key(dir.path(), "k");
        let sig = sign(
            &signing_bytes("pkg", None, None, "sha256:d"),
            &key,
            DEFAULT_NAMESPACE,
        )
        .unwrap();
        assert_eq!(
            trust_tier(
                "cf:reg/pkg@1.0.0",
                Some(&sig),
                "pkg",
                None,
                None,
                "sha256:d",
                &policy
            )
            .unwrap(),
            TrustTier::Pinned
        );
        assert_eq!(
            trust_tier(
                "./local/pkg",
                Some(&sig),
                "pkg",
                None,
                None,
                "sha256:d",
                &policy
            )
            .unwrap(),
            TrustTier::Unpinned
        );
    }
}
