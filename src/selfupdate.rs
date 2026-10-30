//! `aleph self-update [--check] [--json] [--force]` — update from the
//! GitHub releases of `update_repo=owner/name`.
//!
//! Contract (see .github/workflows/release.yml): each release has
//! `aleph-<target>.tar.gz` (the `aleph` binary at the archive
//! root) for every target, plus `SHA256SUMS`. We pick our target's archive,
//! verify its sha256, unpack it and atomically replace the running binary.
//! Everything goes through curl / shasum / tar with timeouts.

use crate::cli::Args;
use crate::doctor::tilde;
use crate::out::*;
use crate::proc;
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

pub const TARGETS: &[&str] = &[
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
];

/// The release target this binary was built for.
pub fn target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some(TARGETS[0]),
        ("macos", "x86_64") => Some(TARGETS[1]),
        ("linux", "x86_64") => Some(TARGETS[2]),
        ("linux", "aarch64") => Some(TARGETS[3]),
        _ => None,
    }
}

// ------------------------------------------------------------------ versions

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub nums: [u64; 3],
    pub pre: Vec<String>,
}

/// `v1.2.3`, `1.2`, `1.2.3-rc.1+build` → Version.
pub fn parse_version(s: &str) -> Option<Version> {
    let s = s.trim();
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let s = s.split('+').next()?;
    let (core, pre) = match s.split_once('-') {
        Some((c, p)) => (c, p.split('.').map(str::to_string).collect()),
        None => (s, Vec::new()),
    };
    let mut nums = [0u64; 3];
    let parts: Vec<&str> = core.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    for (i, p) in parts.iter().enumerate() {
        nums[i] = p.parse().ok()?;
    }
    Some(Version { nums, pre })
}

impl Ord for Version {
    fn cmp(&self, o: &Self) -> Ordering {
        self.nums
            .cmp(&o.nums)
            .then_with(|| match (self.pre.is_empty(), o.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&o.pre) {
                        let c = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(x), Ok(y)) => x.cmp(&y),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            _ => a.cmp(b),
                        };
                        if c != Ordering::Equal {
                            return c;
                        }
                    }
                    self.pre.len().cmp(&o.pre.len())
                }
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

// ------------------------------------------------------------------ release JSON

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub tag: String,
    pub assets: Vec<(String, String)>,
}

pub fn parse_release(v: &Value) -> Option<Release> {
    let tag = v.get("tag_name")?.as_str()?.to_string();
    let assets = v
        .get("assets")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some((
                        x.get("name")?.as_str()?.to_string(),
                        x.get("browser_download_url")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Release { tag, assets })
}

/// (archive, SHA256SUMS) download URLs for `target`.
pub fn pick_assets<'a>(r: &'a Release, target: &str) -> Option<(&'a (String, String), &'a str)> {
    let want = format!("aleph-{target}.tar.gz");
    let archive = r.assets.iter().find(|a| a.0 == want)?;
    let sums = r.assets.iter().find(|a| a.0 == "SHA256SUMS")?;
    Some((archive, sums.1.as_str()))
}

/// The signature of SHA256SUMS (`SHA256SUMS.minisig`), when the release has one.
pub fn sig_url(r: &Release) -> Option<&str> {
    r.assets
        .iter()
        .find(|a| a.0 == "SHA256SUMS.minisig")
        .map(|a| a.1.as_str())
}

/// Check SHA256SUMS against its minisign signature and the public key we
/// trust (`update_pubkey`: the base64 line, or the whole `.pub` file).
/// Pure — tested.
pub fn verify_sums(sums: &str, sig: &str, pubkey: &str) -> Result<(), String> {
    let line = pubkey
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("untrusted comment:"))
        .ok_or("update_pubkey is empty")?;
    let pk = minisign_verify::PublicKey::from_base64(line)
        .map_err(|e| format!("update_pubkey isn't a minisign public key: {e}"))?;
    let sig = minisign_verify::Signature::decode(sig.trim())
        .map_err(|e| format!("SHA256SUMS.minisig doesn't read as a signature: {e}"))?;
    pk.verify(sums.as_bytes(), &sig, true)
        .map_err(|_| "SHA256SUMS isn't signed by update_pubkey".to_string())
}

/// The hash for `name` in a `sha256sum`-style file (`<hex>  name` or
/// `<hex> *name`, optionally with a leading `./` or directory).
pub fn sum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let hex = it.next()?;
        let file = it.next()?.trim_start_matches('*');
        let base = file.rsplit('/').next().unwrap_or(file);
        (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) && base == name)
            .then(|| hex.to_lowercase())
    })
}

// ------------------------------------------------------------------ tools

fn curl(args: &[&str], secs: u64) -> Result<proc::Out, String> {
    let mut v = vec!["-fsSL", "--retry", "2", "--connect-timeout", "10"];
    v.extend_from_slice(args);
    let o = proc::cap("curl", &v, Duration::from_secs(secs));
    if o.ok() {
        Ok(o)
    } else {
        Err(o.reason())
    }
}

fn sha256(p: &Path) -> Result<String, String> {
    let ps = p.to_string_lossy().to_string();
    let tries: [(&str, Vec<&str>); 2] = [
        ("shasum", vec!["-a", "256", ps.as_str()]),
        ("sha256sum", vec![ps.as_str()]),
    ];
    for (prog, args) in tries {
        if proc::which(prog).is_none() {
            continue;
        }
        let o = proc::cap(prog, &args, Duration::from_secs(60));
        if o.ok() {
            if let Some(h) = o.text().split_whitespace().next() {
                return Ok(h.to_lowercase());
            }
        }
    }
    Err("neither shasum nor sha256sum worked".into())
}

/// The latest-release API URL (`ALEPH_UPDATE_URL` overrides it, for tests).
fn api_url(repo: &str) -> String {
    crate::brand::env("UPDATE_URL")
        .unwrap_or_else(|| format!("https://api.github.com/repos/{repo}/releases/latest"))
}

fn valid_repo(r: &str) -> bool {
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    matches!(r.split_once('/'), Some((o, n)) if ok(o) && ok(n))
}

// ------------------------------------------------------------------ command

pub fn cmd(a: Args) -> ExitCode {
    let has = |f: &str| a.rest.iter().any(|x| x == f);
    let (check, js, force) = (has("--check"), has("--json"), has("--force"));
    let Some(repo) = a.cfg.get("update_repo") else {
        err("no update_repo set");
        hint("aleph settings set update_repo <owner>/<name>   (the GitHub repo that publishes releases)");
        return ExitCode::from(2);
    };
    if !valid_repo(&repo) {
        return fail(&format!(
            "update_repo should look like owner/name, not '{repo}'"
        ));
    }
    let current = env!("CARGO_PKG_VERSION");
    let sp = Spinner::start(format!("Checking {repo} for a new release"));
    let rel = curl(
        &["-H", "Accept: application/vnd.github+json", &api_url(&repo)],
        30,
    )
    .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).map_err(|e| e.to_string()))
    .and_then(|v| parse_release(&v).ok_or_else(|| "no tag_name in the release JSON".into()));
    sp.stop();
    let rel = match rel {
        Ok(r) => r,
        Err(e) => {
            err(&format!("couldn't read the latest release of {repo}: {e}"));
            hint("check the repo name and your network; GitHub's API also rate-limits anonymous calls");
            return ExitCode::from(1);
        }
    };
    let (Some(cur), Some(latest)) = (parse_version(current), parse_version(&rel.tag)) else {
        return fail(&format!(
            "can't compare versions '{current}' and '{}'",
            rel.tag
        ));
    };
    let newer = latest > cur;
    let tgt = target();
    let assets = tgt.and_then(|t| pick_assets(&rel, t));
    if js {
        json(&json!({
            "current": current,
            "latest": rel.tag.trim_start_matches(['v', 'V']),
            "update_available": newer,
            "target": tgt,
            "asset": assets.map(|a| a.0 .1.clone()),
        }));
        if check || !newer {
            return ExitCode::SUCCESS;
        }
    }
    if !newer {
        ok(&format!(
            "aleph {current} is up to date ({repo} {})",
            rel.tag
        ));
        return ExitCode::SUCCESS;
    }
    if !js {
        say(ratatui::text::Line::from(format!(
            "  {current} → {} available from {repo}",
            rel.tag
        )));
    }
    if check {
        hint("aleph self-update installs it");
        return ExitCode::SUCCESS;
    }
    let Some(t) = tgt else {
        return fail("no release builds for this platform — build from source");
    };
    let Some(((name, url), sums_url)) = assets else {
        err(&format!(
            "release {} has no aleph-{t}.tar.gz + SHA256SUMS",
            rel.tag
        ));
        return ExitCode::from(1);
    };
    let exe = match std::env::current_exe().and_then(|p| p.canonicalize()) {
        Ok(p) => p,
        Err(e) => return fail(&format!("can't find this binary: {e}")),
    };
    if exe.to_string_lossy().contains("/Cellar/") && !force {
        err(&format!("{} is managed by Homebrew", tilde(&exe)));
        hint("brew upgrade aleph   (or --force to replace it anyway)");
        return ExitCode::from(1);
    }
    // with a key we trust, an unsigned release is refused
    let pubkey = a.cfg.get("update_pubkey").filter(|k| !k.trim().is_empty());
    let sig = sig_url(&rel);
    if pubkey.is_some() && sig.is_none() {
        err(&format!(
            "release {} isn't signed (no SHA256SUMS.minisig) — not installed",
            rel.tag
        ));
        hint("update_pubkey is set, so only signed releases install");
        return ExitCode::from(1);
    }
    let check_sig = pubkey.as_deref().zip(sig);
    match install(name, url, sums_url, check_sig, &exe) {
        Ok(()) => {
            ok(&format!("aleph {} → {}", rel.tag, tilde(&exe)));
            ExitCode::SUCCESS
        }
        Err(e) => {
            err(&e);
            ExitCode::from(1)
        }
    }
}

/// (the trusted public key, the signature's URL)
type SigCheck<'a> = Option<(&'a str, &'a str)>;

fn install(name: &str, url: &str, sums_url: &str, sig: SigCheck, exe: &Path) -> Result<(), String> {
    let dir = std::env::temp_dir().join(format!("aleph-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let r = install_in(&dir, name, url, sums_url, sig, exe);
    let _ = std::fs::remove_dir_all(&dir);
    r
}

/// The repository Aleph's releases come from, unless `update_repo` says another.
pub const HOME_REPO: &str = "sagewhocodes/aleph";

/// Fetch one file of the release this build came from, into `dest`, checked against
/// that release's SHA256SUMS (and its signature, with `update_pubkey`).
/// `ALEPH_RELEASE_BASE` stands in for the release's address, for tests.
pub fn fetch_own_release_file(
    cfg: &crate::config::Config,
    name: &str,
    dest: &Path,
) -> Result<(), String> {
    let base = crate::brand::env("RELEASE_BASE").unwrap_or_else(|| {
        let repo = cfg
            .get("update_repo")
            .filter(|r| valid_repo(r))
            .unwrap_or_else(|| HOME_REPO.to_string());
        format!(
            "https://github.com/{repo}/releases/download/v{}",
            env!("CARGO_PKG_VERSION")
        )
    });
    let pubkey = cfg.get("update_pubkey").filter(|k| !k.trim().is_empty());
    let sig_url = format!("{base}/SHA256SUMS.minisig");
    download_checked(
        dest,
        name,
        &format!("{base}/{name}"),
        &format!("{base}/SHA256SUMS"),
        pubkey.as_deref().map(|k| (k, sig_url.as_str())),
    )
}

/// Download `url` to `archive`, and check it against the checksums (and their
/// signature, when there's a key to check it with).
fn download_checked(
    archive: &Path,
    name: &str,
    url: &str,
    sums_url: &str,
    sig: SigCheck,
) -> Result<(), String> {
    let a = archive.to_string_lossy().to_string();
    let dl = curl(&["-o", &a, url], 300).map_err(|e| format!("download failed: {e}"));
    let sums = dl.and_then(|_| {
        curl(&[sums_url], 60)
            .map(|o| o.text())
            .map_err(|e| format!("couldn't fetch SHA256SUMS: {e}"))
    })?;
    // the checksums are what the signature covers; the archive is checked against them
    if let Some((pubkey, sig_url)) = sig {
        let s = curl(&[sig_url], 60)
            .map(|o| o.text())
            .map_err(|e| format!("couldn't fetch SHA256SUMS.minisig: {e}"))?;
        verify_sums(&sums, &s, pubkey).map_err(|e| format!("{e} — not installed"))?;
    }
    let want = sum_for(&sums, name).ok_or_else(|| format!("SHA256SUMS has no entry for {name}"))?;
    let got = sha256(archive)?;
    if got != want {
        return Err(format!(
            "checksum mismatch for {name} (expected {want}, got {got}) — not installed"
        ));
    }
    Ok(())
}

fn install_in(
    dir: &Path,
    name: &str,
    url: &str,
    sums_url: &str,
    sig: SigCheck,
    exe: &Path,
) -> Result<(), String> {
    let sp = Spinner::start(format!("Downloading {name}"));
    let archive = dir.join(name);
    let a = archive.to_string_lossy().to_string();
    let checked = download_checked(&archive, name, url, sums_url, sig);
    sp.stop();
    checked?;
    let out = dir.join("x");
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let o = proc::cap(
        "tar",
        &["-xzf", a.as_str(), "-C", &out.to_string_lossy()],
        Duration::from_secs(60),
    );
    if !o.ok() {
        return Err(format!("couldn't unpack {name}: {}", o.reason()));
    }
    let new = out.join("aleph");
    if !new.is_file() {
        return Err(format!("{name} has no aleph binary at its root"));
    }
    replace(&new, exe)
}

/// Copy `new` next to `exe` and rename it over (atomic on one filesystem).
pub fn replace(new: &Path, exe: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let staged: PathBuf = exe.with_file_name(format!(".aleph.new.{}", std::process::id()));
    let res = (|| {
        std::fs::copy(new, &staged)?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&staged, exe)
    })();
    res.map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            format!(
                "no permission to replace {} — rerun with sudo, or reinstall where you can write",
                tilde(exe)
            )
        } else {
            format!("couldn't replace {}: {e}", tilde(exe))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        parse_version(s).unwrap()
    }

    #[test]
    fn version_compare() {
        assert!(v("1.0.1") > v("1.0.0"));
        assert!(v("v1.10.0") > v("1.9.9"));
        assert!(v("2.0") > v("1.99.99"));
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert_eq!(v("1.2.3+build.5").cmp(&v("1.2.3")), Ordering::Equal);
        // pre-releases sort before the release, numerically, then by length
        assert!(v("1.0.0-rc.1") < v("1.0.0"));
        assert!(v("1.0.0-rc.2") < v("1.0.0-rc.10"));
        assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"));
        assert!(v("1.0.0-alpha.1") < v("1.0.0-beta"));
        assert!(v("1.0.0-1") < v("1.0.0-alpha"));
        assert!(v("1.0.1-rc.1") > v("1.0.0"));
        assert!(parse_version("latest").is_none());
        assert!(parse_version("1.2.3.4").is_none());
        assert!(parse_version("").is_none());
        assert!(parse_version(env!("CARGO_PKG_VERSION")).is_some());
    }

    #[test]
    fn asset_choice() {
        let j = serde_json::json!({
            "tag_name": "v1.2.0",
            "assets": [
                {"name": "aleph-x86_64-apple-darwin.tar.gz", "browser_download_url": "https://x/mac-intel"},
                {"name": "aleph-aarch64-apple-darwin.tar.gz", "browser_download_url": "https://x/mac-arm"},
                {"name": "aleph-x86_64-unknown-linux-musl.tar.gz", "browser_download_url": "https://x/linux"},
                {"name": "SHA256SUMS", "browser_download_url": "https://x/sums"},
                {"name": "broken"}
            ]
        });
        let r = parse_release(&j).unwrap();
        assert_eq!(r.tag, "v1.2.0");
        assert_eq!(r.assets.len(), 4);
        let (a, s) = pick_assets(&r, "aarch64-apple-darwin").unwrap();
        assert_eq!(
            (a.0.as_str(), a.1.as_str(), s),
            (
                "aleph-aarch64-apple-darwin.tar.gz",
                "https://x/mac-arm",
                "https://x/sums"
            )
        );
        assert!(pick_assets(&r, "aarch64-unknown-linux-musl").is_none());
        let no_sums = Release {
            tag: "v1".into(),
            assets: vec![("aleph-x86_64-unknown-linux-musl.tar.gz".into(), "u".into())],
        };
        assert!(pick_assets(&no_sums, "x86_64-unknown-linux-musl").is_none());
        assert!(parse_release(&serde_json::json!({"message": "Not Found"})).is_none());
        if let Some(t) = target() {
            assert!(TARGETS.contains(&t));
        }
    }

    #[test]
    fn signed_checksums() {
        // minisign-verify's own vectors: "test" signed by this key
        let pk = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
        let sig = "untrusted comment: signature from minisign secret key
RWQf6LRCGA9i59SLOFxz6NxvASXDJeRtuZykwQepbDEGt87ig1BNpWaVWuNrm73YiIiJbq71Wi+dP9eKL8OC351vwIasSSbXxwA=
trusted comment: timestamp:1555779966\tfile:test
QtKMXWyYcwdpZAlPF7tE2ENJkRd1ujvKjlj1m9RtHTBnZPa5WKU5uWRs5GoP5M/VqE81QFuMKI5k/SfNQUaOAA==";
        assert_eq!(verify_sums("test", sig, pk), Ok(()));
        // the whole .pub file works too
        let file = format!("untrusted comment: minisign public key\n{pk}\n");
        assert_eq!(verify_sums("test", sig, &file), Ok(()));
        // changed checksums, another key, junk: refused
        assert!(verify_sums("Test", sig, pk).is_err());
        assert!(verify_sums(
            "test",
            sig,
            "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO4"
        )
        .is_err());
        assert!(verify_sums("test", "nonsense", pk).is_err());
    }

    #[test]
    fn sha256sums_parsing() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = format!(
            "{a}  aleph-aarch64-apple-darwin.tar.gz\n{b} *dist/aleph-x86_64-apple-darwin.tar.gz\nnot a line\n{}  short\n",
            "c".repeat(10)
        );
        assert_eq!(sum_for(&sums, "aleph-aarch64-apple-darwin.tar.gz"), Some(a));
        assert_eq!(
            sum_for(&sums, "aleph-x86_64-apple-darwin.tar.gz"),
            Some("b".repeat(64))
        );
        assert_eq!(sum_for(&sums, "short"), None);
        assert_eq!(sum_for(&sums, "missing.tar.gz"), None);
    }

    #[test]
    fn install_from_file_urls() {
        if ["curl", "tar"].iter().any(|p| proc::which(p).is_none()) {
            return;
        }
        let d = std::env::temp_dir().join(format!("chi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let src = d.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("aleph"), b"#!/bin/sh\necho new\n").unwrap();
        let name = "aleph-test.tar.gz";
        let tgz = d.join(name);
        let o = proc::cap(
            "tar",
            &[
                "-czf",
                &tgz.to_string_lossy(),
                "-C",
                &src.to_string_lossy(),
                "aleph",
            ],
            Duration::from_secs(20),
        );
        assert!(o.ok(), "{}", o.stderr);
        let hex = sha256(&tgz).unwrap();
        let exe = d.join("bin-aleph");
        std::fs::write(&exe, b"old").unwrap();
        let url = |p: &Path| format!("file://{}", p.display());
        // wrong checksum → refused, binary untouched
        std::fs::write(d.join("BAD"), format!("{}  {name}\n", "0".repeat(64))).unwrap();
        let e = install(name, &url(&tgz), &url(&d.join("BAD")), None, &exe).unwrap_err();
        assert!(e.contains("checksum mismatch"), "{e}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        std::fs::write(d.join("SUMS"), format!("{hex}  {name}\n")).unwrap();
        // a signature that doesn't cover these checksums → refused, untouched
        std::fs::write(d.join("SUMS.minisig"), "untrusted comment: x\nRWQf6LRCGA9i59SLOFxz6NxvASXDJeRtuZykwQepbDEGt87ig1BNpWaVWuNrm73YiIiJbq71Wi+dP9eKL8OC351vwIasSSbXxwA=\ntrusted comment: timestamp:1555779966\tfile:test\nQtKMXWyYcwdpZAlPF7tE2ENJkRd1ujvKjlj1m9RtHTBnZPa5WKU5uWRs5GoP5M/VqE81QFuMKI5k/SfNQUaOAA==\n").unwrap();
        let sig = url(&d.join("SUMS.minisig"));
        let pk = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
        let e = install(
            name,
            &url(&tgz),
            &url(&d.join("SUMS")),
            Some((pk, &sig)),
            &exe,
        )
        .unwrap_err();
        assert!(e.contains("isn't signed by update_pubkey"), "{e}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        install(name, &url(&tgz), &url(&d.join("SUMS")), None, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"#!/bin/sh\necho new\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn repo_names_and_replace() {
        assert!(valid_repo("someone/aleph"));
        assert!(!valid_repo("aleph") && !valid_repo("a/b/c") && !valid_repo("a/$(x)"));
        let d = std::env::temp_dir().join(format!("chu-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let (new, exe) = (d.join("new"), d.join("aleph"));
        std::fs::write(&new, b"NEW").unwrap();
        std::fs::write(&exe, b"OLD").unwrap();
        replace(&new, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"NEW");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::read_dir(&d).unwrap().count(),
            2,
            "no staged leftovers"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
