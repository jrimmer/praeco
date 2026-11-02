//! Signed releases: when a GitHub release carries a packslip bundle
//! (`packslip*.sigstore.json`, see <https://github.com/jdx/packslip>), the
//! download is checked against the signed statement instead of a checksum that
//! sits next to the file.
//!
//! praeco keeps one small record per signed project (`signers.json` next to
//! the app registrations): the repository ID and the workflow that signed the
//! last accepted release. Later releases are held to it, and a project that was
//! signed once does not get to go back to unsigned releases.
//!
//! Releases without a bundle keep the checksum path. Projects on Forgejo, and
//! GitHub Enterprise hosts, have no signer identity packslip can imply from the
//! name, so they stay checksum-only.
//!
//! Built without the `signed` feature, this module does nothing: every
//! release is `Plan::Unsigned`.

use crate::detect::Env;
use crate::error::Result;
use crate::http::Http;
use crate::release::Release;
use crate::source::Source;
use std::path::Path;

/// How a release is to be verified.
pub enum Plan {
    /// No signature is involved: use the published checksums.
    Unsigned,
    /// A packslip bundle was found and verified on its own; the downloaded
    /// file still has to match it.
    Signed(Signed),
}

/// Decide how to verify `release`. Downloads and verifies the bundle, if there
/// is one. Fails when a bundle is there but wrong, or is missing from a project
/// that was signed before.
pub fn plan(http: &Http, source: &Source, release: &Release, token: Option<&str>, env: &Env) -> Result<Plan> {
    imp::plan(http, source, release, token, env)
}

pub use imp::Signed;

#[cfg(feature = "signed")]
pub use imp::{bundles, check_bundle, Record, Store};

impl Signed {
    /// Check the downloaded `file` against the verified bundle, then remember
    /// the signer. Only an asset the bundle lists as an artifact passes.
    pub fn verify_file(&self, file: &Path) -> Result<()> {
        self.check_file(file)
    }
}

#[cfg(feature = "signed")]
mod imp {
    use super::Plan;
    use crate::detect::Env;
    use crate::error::{Error, ErrorCode, Result};
    use crate::http::Http;
    use crate::release::{Asset, Release};
    use crate::source::{Host, Source, GITHUB_API};
    use crate::{verify, version};
    use packslip::forge::{Expected, ForgePin, PinSource};
    use packslip::{Options, Verified};
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;
    use std::io::Write as _;
    use std::path::{Path, PathBuf};

    const MAX_BUNDLES: usize = 8;

    /// What praeco remembers about a signed project.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Record {
        /// The repository the signature came from, by the forge's ID.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub pin: Option<ForgePin>,
        /// The certificate identity (the workflow) that signed the last accepted release.
        pub signer: String,
        /// Whether the release asked for later releases to be held to that workflow.
        pub pins_workflow: bool,
        /// The last version accepted.
        pub version: String,
    }

    #[derive(Debug, Default, Serialize, Deserialize)]
    struct File {
        version: u32,
        #[serde(default)]
        projects: BTreeMap<String, Record>,
    }

    /// The record of signed projects. Without a config folder (no `HOME`) it
    /// is empty and nothing is saved, so nothing is held to a signer.
    #[derive(Debug, Default)]
    pub struct Store {
        path: Option<PathBuf>,
        file: File,
    }

    impl Store {
        pub fn path_for(env: &Env) -> Option<PathBuf> {
            let base = env.config_home.clone().or_else(|| env.home.as_ref().map(|h| h.join(".config")))?;
            Some(base.join("praeco").join("signers.json"))
        }

        /// Read the record. A missing file is an empty record; a damaged one
        /// is an error, because ignoring it would switch the downgrade check off.
        pub fn load(env: &Env) -> Result<Store> {
            let Some(path) = Store::path_for(env) else { return Ok(Store::default()) };
            let file = match std::fs::read(&path) {
                Ok(bytes) => serde_json::from_slice::<File>(&bytes).map_err(|e| {
                    Error::new(
                        ErrorCode::Io,
                        format!("praeco's record of signed apps ({}) is damaged ({e}). Fix or delete it to continue.", path.display()),
                    )
                })?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => File::default(),
                Err(e) => return Err(e.into()),
            };
            Ok(Store { path: Some(path), file })
        }

        pub fn get(&self, project: &str) -> Option<&Record> {
            self.file.projects.get(&project.to_ascii_lowercase())
        }

        pub fn put(&mut self, project: &str, record: Record) -> Result<()> {
            self.file.projects.insert(project.to_ascii_lowercase(), record);
            self.file.version = 1;
            let Some(path) = &self.path else { return Ok(()) };
            let dir = path.parent().expect("a file path has a parent");
            std::fs::create_dir_all(dir)?;
            let mut json = serde_json::to_vec_pretty(&self.file).expect("serialises");
            json.push(b'\n');
            let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
            tmp.write_all(&json)?;
            tmp.persist(path).map_err(|e| Error::from(e.error))?;
            Ok(())
        }
    }

    /// A signed release found, verified on its own, waiting for the file.
    pub struct Signed {
        project: String,
        tag: String,
        bundle: String,
        resolved_id: Option<String>,
        store: std::sync::Mutex<Store>,
        pub signer: String,
    }

    impl Signed {
        pub fn project(&self) -> &str {
            &self.project
        }

        pub(super) fn check_file(&self, file: &Path) -> Result<()> {
            let name = file.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
            let mut store = self.store.lock().expect("not poisoned");
            let accepted = check_bundle(&self.project, &self.bundle, store.get(&self.project), self.resolved_id.as_deref(), &[file])
                .map_err(|e| self.refuse(&e.to_string()))?;
            if !accepted.verified.checked_artifacts.contains(&name) {
                return Err(self.refuse(&format!("{name} is not one of the artifacts the signature covers")));
            }
            let pin = accepted
                .check
                .pin
                .clone()
                .or_else(|| self.resolved_id.as_ref().map(|id| ForgePin::of(self.project.clone(), id.clone())));
            store.put(
                &self.project,
                Record {
                    pin,
                    signer: accepted.verified.key_id.clone(),
                    pins_workflow: accepted.check.pins_workflow(),
                    version: accepted.verified.version.clone(),
                },
            )
        }

        fn refuse(&self, why: &str) -> Error {
            Error::new(
                ErrorCode::SignatureInvalid,
                format!("The signature on {} release {} did not check out: {why}. It was not installed.", self.project, self.tag),
            )
        }
    }

    /// `github.com/owner/repo` for a repository on public GitHub; `None` for
    /// everything packslip cannot name a signer for.
    pub fn project_name(source: &Source) -> Option<String> {
        match &source.host {
            Host::GitHub { api_base } if api_base == GITHUB_API => Some(format!("github.com/{}/{}", source.owner, source.repo)),
            _ => None,
        }
    }

    /// The release's packslip bundle assets.
    pub fn bundles(release: &Release) -> Vec<&Asset> {
        release
            .assets
            .iter()
            .filter(|a| {
                let n = a.name.to_ascii_lowercase();
                n.starts_with("packslip") && n.ends_with(".sigstore.json")
            })
            .collect()
    }

    /// Verify `bundle` as the signature of `project`'s release under the
    /// policy GitHub implies, hold it to what was recorded, and check `files`
    /// against it.
    pub fn check_bundle(
        project: &str,
        bundle: &str,
        recorded: Option<&Record>,
        resolved_id: Option<&str>,
        files: &[&Path],
    ) -> std::result::Result<packslip::verify::ForgeVerified<Verified>, Box<dyn std::error::Error + Send + Sync>> {
        let root = packslip::sigstore::trusted_root(None)?;
        let options = Options { require_log: true, trusted_root: &root };
        let pins: Vec<(PinSource, ForgePin)> =
            recorded.and_then(|r| r.pin.clone()).map(|p| vec![(PinSource::Local, p)]).unwrap_or_default();
        let mut expected = Expected::new(project).pinned_by(&pins);
        if recorded.and_then(|r| r.pin.as_ref()).is_none() {
            expected = expected.resolved(resolved_id);
        }
        let accepted = packslip::verify_forge(bundle, &expected, options, files)?;
        if let Some(r) = recorded
            && (!accepted.check.continues_signer(&r.signer) || (r.pins_workflow && !accepted.check.pins_workflow()))
        {
            return Err(format!(
                "the signer changed: it was {}, now {}. If that is expected, remove {project} from praeco's signers.json",
                r.signer, accepted.verified.key_id
            )
            .into());
        }
        Ok(accepted)
    }

    fn repo_id(http: &Http, source: &Source, token: Option<&str>) -> Result<String> {
        let Host::GitHub { api_base } = &source.host else { unreachable!("only GitHub projects are signed") };
        let url = url::Url::parse(&format!("{api_base}/repos/{}/{}", source.owner, source.repo))
            .map_err(|e| Error::new(ErrorCode::Usage, format!("Bad repository URL: {e}")))?;
        let auth = token.map(|t| source.auth_header(t));
        let resp = http.get(&url, "application/vnd.github+json", auth.as_deref(), &[("X-GitHub-Api-Version", "2022-11-28")])?;
        if resp.status() != 200 {
            return Err(Error::new(
                ErrorCode::Api,
                format!("Could not look up {}/{} to check who signed its release (HTTP {})", source.owner, source.repo, resp.status()),
            ));
        }
        let bytes = crate::http::read_body(resp, 1024 * 1024)?;
        let v: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| Error::new(ErrorCode::Api, format!("GitHub sent something praeco could not read: {e}")))?;
        v["id"]
            .as_u64()
            .map(|id| id.to_string())
            .ok_or_else(|| Error::new(ErrorCode::Api, "GitHub did not give a repository ID"))
    }

    pub fn plan(http: &Http, source: &Source, release: &Release, token: Option<&str>, env: &Env) -> Result<Plan> {
        let Some(project) = project_name(source) else { return Ok(Plan::Unsigned) };
        let store = Store::load(env)?;
        let candidates = bundles(release);
        if candidates.is_empty() {
            return match store.get(&project) {
                Some(r) => Err(Error::new(
                    ErrorCode::SignatureMissing,
                    format!(
                        "{project} signed its releases before (last: {}), but release {} has no signature. \
                         It was not installed. If the release was only just published, its signature may still be \
                         on the way: try again in a few minutes. If the project has stopped signing on purpose, remove it from {}.",
                        r.version,
                        release.tag,
                        Store::path_for(env).map(|p| p.display().to_string()).unwrap_or_else(|| "praeco's signers.json".into()),
                    ),
                )),
                None => Ok(Plan::Unsigned),
            };
        }
        let bad = |why: String| {
            Error::new(ErrorCode::SignatureInvalid, format!("The signature on {project} release {} did not check out: {why}. It was not installed.", release.tag))
        };
        if candidates.len() > MAX_BUNDLES {
            return Err(bad(format!("the release has {} signature files", candidates.len())));
        }
        // Pick the bundle that claims this project; the claim is only used to choose.
        let mut texts = Vec::new();
        for a in &candidates {
            texts.push(verify::fetch_text(http, source, release, a)?);
        }
        let chosen = texts
            .iter()
            .find(|t| {
                packslip::verify::peek_unverified(t)
                    .is_ok_and(|c| c.predicate_type == packslip::model::PREDICATE_TYPE && c.project.eq_ignore_ascii_case(&project))
            })
            .or_else(|| (texts.len() == 1).then(|| &texts[0]))
            .ok_or_else(|| bad("none of its signature files is for this project".into()))?
            .clone();

        let recorded = store.get(&project).cloned();
        let resolved_id = match recorded.as_ref().and_then(|r| r.pin.as_ref()) {
            Some(_) => None,
            None => Some(repo_id(http, source, token)?),
        };
        let accepted = check_bundle(&project, &chosen, recorded.as_ref(), resolved_id.as_deref(), &[]).map_err(|e| bad(e.to_string()))?;
        let signed_version = version::parse(&accepted.verified.version);
        if !signed_version.is_some_and(|v| version::same(&v, &release.version)) {
            return Err(bad(format!("it is for version {}, not {}", accepted.verified.version, release.version)));
        }
        Ok(Plan::Signed(Signed {
            project,
            tag: release.tag.clone(),
            bundle: chosen,
            resolved_id,
            store: std::sync::Mutex::new(store),
            signer: accepted.verified.key_id.clone(),
        }))
    }
}

#[cfg(not(feature = "signed"))]
mod imp {
    use super::Plan;
    use crate::detect::Env;
    use crate::error::Result;
    use crate::http::Http;
    use crate::release::Release;
    use crate::source::Source;
    use std::path::Path;

    /// Never built: without the `signed` feature every release is unsigned.
    pub struct Signed {
        never: std::convert::Infallible,
        pub signer: String,
    }

    impl Signed {
        pub fn project(&self) -> &str {
            match self.never {}
        }
        pub(super) fn check_file(&self, _file: &Path) -> Result<()> {
            match self.never {}
        }
    }

    pub fn plan(_: &Http, _: &Source, _: &Release, _: Option<&str>, _: &Env) -> Result<Plan> {
        Ok(Plan::Unsigned)
    }
}

#[cfg(all(test, feature = "signed"))]
mod tests {
    use super::imp::{bundles, check_bundle, project_name, Record, Store};
    use super::*;
    use crate::error::ErrorCode;
    use crate::release::{Asset, Release};

    // A real release bundle: jdx/hk 2.3.0, from the packslip test fixtures (MIT).
    const HK: &str = include_str!("../tests/fixtures/hk-v2.3.0.sigstore.json");

    fn release(tag: &str, assets: &[&str]) -> Release {
        Release {
            tag: tag.into(),
            version: crate::version::parse(tag).unwrap(),
            name: None,
            body: String::new(),
            prerelease: false,
            html_url: None,
            published_at: None,
            assets: assets.iter().map(|n| Asset { name: n.to_string(), url: format!("https://x/{n}"), size: 1 }).collect(),
        }
    }

    fn env_in(dir: &std::path::Path) -> Env {
        let mut env = Env::from_process();
        env.home = Some(dir.to_path_buf());
        env.config_home = Some(dir.join("config"));
        env
    }

    #[test]
    fn a_real_bundle_verifies_offline() {
        let ok = check_bundle("github.com/jdx/hk", HK, None, None, &[]).unwrap();
        assert_eq!(ok.verified.version, "2.3.0");
        assert!(ok.verified.key_id.contains("jdx/hk"), "{}", ok.verified.key_id);
    }

    #[test]
    fn another_project_is_refused() {
        assert!(check_bundle("github.com/evil/hk", HK, None, None, &[]).is_err());
    }

    #[test]
    fn a_tampered_bundle_is_refused() {
        let mut v: serde_json::Value = serde_json::from_str(HK).unwrap();
        let p = v["dsseEnvelope"]["payload"].as_str().unwrap().to_string();
        let flipped = format!("{}{}", if p.starts_with('e') { 'f' } else { 'e' }, &p[1..]);
        v["dsseEnvelope"]["payload"] = flipped.into();
        assert!(check_bundle("github.com/jdx/hk", &v.to_string(), None, None, &[]).is_err());
    }

    #[test]
    fn a_file_the_bundle_does_not_list_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("hk-not-listed.tar.gz");
        std::fs::write(&f, "x").unwrap();
        let e = check_bundle("github.com/jdx/hk", HK, None, None, &[&f]).err().unwrap();
        assert!(e.to_string().contains("not listed"), "{e}");
    }

    #[test]
    fn a_changed_signer_is_refused() {
        let rec = Record {
            pin: None,
            signer: "https://github.com/someone/else/.github/workflows/release.yml@refs/tags/v1".into(),
            pins_workflow: false,
            version: "1.0.0".into(),
        };
        let e = check_bundle("github.com/jdx/hk", HK, Some(&rec), None, &[]).err().unwrap();
        assert!(e.to_string().contains("signer changed"), "{e}");
    }

    #[test]
    fn finds_bundle_assets() {
        let r = release("v1.0.0", &["app.tar.gz", "packslip.sigstore.json", "PACKSLIP-tool.sigstore.json", "other.sigstore.json", "packslip.txt"]);
        let names: Vec<_> = bundles(&r).iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["packslip.sigstore.json", "PACKSLIP-tool.sigstore.json"]);
    }

    #[test]
    fn only_public_github_is_signed() {
        assert_eq!(project_name(&Source::github("jdx/hk").unwrap()).as_deref(), Some("github.com/jdx/hk"));
        assert_eq!(project_name(&Source::github_with_api_base("a/b", "https://ghe.example.com/api/v3").unwrap()), None);
        assert_eq!(project_name(&Source::forgejo("https://git.example.com", "a/b").unwrap()), None);
    }

    #[test]
    fn store_round_trips_and_ignores_case() {
        let d = tempfile::tempdir().unwrap();
        let env = env_in(d.path());
        let mut s = Store::load(&env).unwrap();
        assert!(s.get("github.com/Jdx/HK").is_none());
        let rec = Record { pin: None, signer: "s".into(), pins_workflow: true, version: "2.3.0".into() };
        s.put("github.com/jdx/hk", rec).unwrap();
        let back = Store::load(&env).unwrap();
        assert_eq!(back.get("github.com/JDX/hk").unwrap().version, "2.3.0");
    }

    #[test]
    fn a_damaged_record_is_an_error_not_a_free_pass() {
        let d = tempfile::tempdir().unwrap();
        let env = env_in(d.path());
        let path = Store::path_for(&env).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(Store::load(&env).err().unwrap().code, ErrorCode::Io);
    }

    #[test]
    fn unsigned_projects_stay_unsigned() {
        let d = tempfile::tempdir().unwrap();
        let env = env_in(d.path());
        let r = release("v1.0.0", &["app.AppImage", "app.AppImage.sha256"]);
        let first = plan(&Http::new(), &Source::github("a/b").unwrap(), &r, None, &env).unwrap();
        assert!(matches!(first, Plan::Unsigned));
        // Forgejo never takes the signed path, even with a bundle attached.
        let r = release("v1.0.0", &["packslip.sigstore.json"]);
        let second = plan(&Http::new(), &Source::forgejo("https://git.example.com", "a/b").unwrap(), &r, None, &env).unwrap();
        assert!(matches!(second, Plan::Unsigned));
    }

    #[test]
    fn a_signed_project_cannot_go_back_to_unsigned() {
        let d = tempfile::tempdir().unwrap();
        let env = env_in(d.path());
        let mut s = Store::load(&env).unwrap();
        s.put("github.com/a/b", Record { pin: None, signer: "s".into(), pins_workflow: false, version: "1.0.0".into() }).unwrap();
        let r = release("v1.1.0", &["app.AppImage", "app.AppImage.sha256"]);
        let e = plan(&Http::new(), &Source::github("a/b").unwrap(), &r, None, &env).err().unwrap();
        assert_eq!(e.code, ErrorCode::SignatureMissing);
        assert!(e.message.contains("1.0.0"), "{}", e.message);
    }
}
