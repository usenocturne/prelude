//! Choosing releases and bundles for this device from the GitHub releases
//! API response.

use semver::Version;
use serde::Deserialize;

use super::UpdateError;
use crate::settings::UpdateChannel;

/// Image flavour, from `/etc/prelude/flavour`; picks the bundle asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavour {
    Prod,
    Dev,
}

impl Flavour {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "prod" => Some(Self::Prod),
            "dev" => Some(Self::Dev),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Prod => "prod",
            Self::Dev => "dev",
        }
    }

    const fn other(self) -> Self {
        match self {
            Self::Prod => Self::Dev,
            Self::Dev => Self::Prod,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub url: String,
    /// From the API's `digest` field (`sha256:<hex>`), when GitHub sent one.
    pub sha256: Option<[u8; 32]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub prerelease: bool,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    Newer,
    Installed,
    Older,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub version: Version,
    pub tag: String,
    pub asset: Asset,
    pub relation: Relation,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    size: u64,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

/// Parses `GET /repos/{owner}/{repo}/releases`. Drafts and tags that are
/// not `v<semver>` are skipped.
pub fn parse_releases(json: &str) -> Result<Vec<Release>, UpdateError> {
    let releases: Vec<ApiRelease> =
        serde_json::from_str(json).map_err(|_| UpdateError::GithubUnreachable)?;
    Ok(releases
        .into_iter()
        .filter(|release| !release.draft)
        .filter_map(|release| {
            let version = Version::parse(release.tag_name.strip_prefix('v')?).ok()?;
            Some(Release {
                version,
                tag: release.tag_name,
                prerelease: release.prerelease,
                assets: release
                    .assets
                    .into_iter()
                    .map(|asset| Asset {
                        sha256: asset.digest.as_deref().and_then(parse_digest),
                        name: asset.name,
                        size: asset.size,
                        url: asset.browser_download_url,
                    })
                    .collect(),
            })
        })
        .collect())
}

fn parse_digest(digest: &str) -> Option<[u8; 32]> {
    let mut bytes = [0_u8; 32];
    hex::decode_to_slice(digest.strip_prefix("sha256:")?, &mut bytes).ok()?;
    Some(bytes)
}

pub fn asset_name(flavour: Flavour, version: &Version) -> String {
    format!("prelude_update_{}_v{version}.swu", flavour.as_str())
}

/// Every release on `channel` that carries this flavour's bundle, newest
/// first. `installed: None` (an unparsable installed version) makes every
/// release newer.
pub fn candidates(
    releases: &[Release],
    channel: UpdateChannel,
    flavour: Flavour,
    installed: Option<&Version>,
) -> Result<Vec<Candidate>, UpdateError> {
    let on_channel = releases
        .iter()
        .filter(|release| channel == UpdateChannel::Beta || !release.prerelease);
    let mut found: Vec<Candidate> = on_channel
        .clone()
        .filter_map(|release| {
            let name = asset_name(flavour, &release.version);
            let asset = release.assets.iter().find(|asset| asset.name == name)?;
            let relation = match installed.map(|installed| release.version.cmp(installed)) {
                None | Some(std::cmp::Ordering::Greater) => Relation::Newer,
                Some(std::cmp::Ordering::Equal) => Relation::Installed,
                Some(std::cmp::Ordering::Less) => Relation::Older,
            };
            Some(Candidate {
                version: release.version.clone(),
                tag: release.tag.clone(),
                asset: asset.clone(),
                relation,
            })
        })
        .collect();
    if found.is_empty() {
        let other_flavour = on_channel.into_iter().any(|release| {
            let name = asset_name(flavour.other(), &release.version);
            release.assets.iter().any(|asset| asset.name == name)
        });
        if other_flavour {
            return Err(UpdateError::WrongFlavour);
        }
    }
    found.sort_by(|left, right| right.version.cmp(&left.version));
    Ok(found)
}

/// The newest release newer than the installed one.
pub fn newest_update(candidates: &[Candidate]) -> Option<&Candidate> {
    candidates
        .iter()
        .find(|candidate| candidate.relation == Relation::Newer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::UpdateChannel;

    fn release_json(tag: &str, prerelease: bool, draft: bool, assets: &[&str]) -> String {
        let assets: Vec<String> = assets
            .iter()
            .map(|name| {
                format!(
                    r#"{{"name":"{name}","size":1000,"browser_download_url":"https://example.invalid/{name}"}}"#
                )
            })
            .collect();
        format!(
            r#"{{"tag_name":"{tag}","prerelease":{prerelease},"draft":{draft},"assets":[{}]}}"#,
            assets.join(",")
        )
    }

    fn list(releases: &[String]) -> Vec<Release> {
        parse_releases(&format!("[{}]", releases.join(","))).expect("parse")
    }

    fn prod(version: &str) -> String {
        format!("prelude_update_prod_v{version}.swu")
    }

    fn version(text: &str) -> semver::Version {
        semver::Version::parse(text).expect("version")
    }

    fn versions(candidates: &[Candidate]) -> Vec<String> {
        candidates
            .iter()
            .map(|candidate| candidate.version.to_string())
            .collect()
    }

    #[test]
    fn stable_skips_prereleases_beta_includes_them() {
        let releases = list(&[
            release_json("v1.0.6-test", true, false, &[&prod("1.0.6-test")]),
            release_json("v1.0.5", false, false, &[&prod("1.0.5")]),
        ]);
        let installed = version("1.0.4");
        let stable = candidates(
            &releases,
            UpdateChannel::Stable,
            Flavour::Prod,
            Some(&installed),
        )
        .expect("stable");
        assert_eq!(versions(&stable), ["1.0.5"]);
        let beta = candidates(
            &releases,
            UpdateChannel::Beta,
            Flavour::Prod,
            Some(&installed),
        )
        .expect("beta");
        assert_eq!(versions(&beta), ["1.0.6-test", "1.0.5"]);
    }

    #[test]
    fn odd_releases_are_skipped() {
        let releases = list(&[
            release_json("v1.0.9", false, true, &[&prod("1.0.9")]),
            release_json("nightly", false, false, &[&prod("nightly")]),
            release_json("v1.0", false, false, &[&prod("1.0")]),
            release_json("latest", false, false, &[]),
            release_json("v1.0.8", false, false, &[]),
            release_json("v1.0.7", false, false, &["prelude_image_v1.0.7.zip"]),
            release_json("v1.0.5", false, false, &[&prod("1.0.5")]),
        ]);
        assert_eq!(
            releases
                .iter()
                .map(|release| release.tag.as_str())
                .collect::<Vec<_>>(),
            ["v1.0.8", "v1.0.7", "v1.0.5"],
            "drafts and tags that are not v<semver> are dropped"
        );
        let found = candidates(&releases, UpdateChannel::Beta, Flavour::Prod, None).expect("ok");
        assert_eq!(versions(&found), ["1.0.5"]);
    }

    #[test]
    fn exact_asset_name_only() {
        assert_eq!(
            asset_name(Flavour::Prod, &version("1.0.5")),
            "prelude_update_prod_v1.0.5.swu"
        );
        assert_eq!(
            asset_name(Flavour::Dev, &version("1.0.5")),
            "prelude_update_dev_v1.0.5.swu"
        );
        let releases = list(&[release_json(
            "v1.0.5",
            false,
            false,
            &[
                "prelude_update_prod_v1.0.5.swu.sha256",
                "prelude_update_prod_v1.0.5-rc.swu",
                "prelude_update_prod_v1.0.5.swu",
            ],
        )]);
        let found = candidates(&releases, UpdateChannel::Stable, Flavour::Prod, None).expect("ok");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].asset.name, "prelude_update_prod_v1.0.5.swu");
        assert_eq!(
            found[0].asset.url,
            "https://example.invalid/prelude_update_prod_v1.0.5.swu"
        );
        assert_eq!(found[0].asset.size, 1000);
        assert_eq!(found[0].tag, "v1.0.5");
    }

    #[test]
    fn other_flavour_only_is_reported() {
        let releases = list(&[release_json(
            "v1.0.5",
            false,
            false,
            &["prelude_update_dev_v1.0.5.swu"],
        )]);
        assert_eq!(
            candidates(&releases, UpdateChannel::Stable, Flavour::Prod, None),
            Err(UpdateError::WrongFlavour)
        );
        assert_eq!(
            candidates(&[], UpdateChannel::Stable, Flavour::Prod, None),
            Ok(Vec::new()),
            "no releases at all is not a flavour problem"
        );
    }

    #[test]
    fn relation_and_order() {
        let releases = list(&[
            release_json("v1.0.4", false, false, &[&prod("1.0.4")]),
            release_json("v1.0.6", false, false, &[&prod("1.0.6")]),
            release_json("v1.0.6-test", true, false, &[&prod("1.0.6-test")]),
            release_json("v1.0.5", false, false, &[&prod("1.0.5")]),
        ]);
        let installed = version("1.0.5");
        let found = candidates(
            &releases,
            UpdateChannel::Beta,
            Flavour::Prod,
            Some(&installed),
        )
        .expect("ok");
        assert_eq!(versions(&found), ["1.0.6", "1.0.6-test", "1.0.5", "1.0.4"]);
        assert_eq!(
            found
                .iter()
                .map(|candidate| candidate.relation)
                .collect::<Vec<_>>(),
            [
                Relation::Newer,
                Relation::Newer,
                Relation::Installed,
                Relation::Older
            ]
        );
        assert_eq!(
            newest_update(&found).map(|candidate| candidate.version.to_string()),
            Some("1.0.6".to_owned())
        );
        let current = candidates(
            &releases,
            UpdateChannel::Stable,
            Flavour::Prod,
            Some(&version("1.0.6")),
        )
        .expect("ok");
        assert_eq!(newest_update(&current), None);
    }

    #[test]
    fn unparsable_installed_version_treats_all_as_newer() {
        let releases = list(&[release_json("v1.0.4", false, false, &[&prod("1.0.4")])]);
        let found = candidates(&releases, UpdateChannel::Stable, Flavour::Prod, None).expect("ok");
        assert_eq!(found[0].relation, Relation::Newer);
    }

    #[test]
    fn digest_is_read_when_present() {
        let digest = "ab".repeat(32);
        let json = format!(
            r#"[{{"tag_name":"v1.0.5","prerelease":false,"draft":false,"assets":[
                {{"name":"a.swu","size":1,"browser_download_url":"u","digest":"sha256:{digest}"}},
                {{"name":"b.swu","size":1,"browser_download_url":"u","digest":"sha512:00"}},
                {{"name":"c.swu","size":1,"browser_download_url":"u","digest":null}},
                {{"name":"d.swu","size":1,"browser_download_url":"u","digest":"sha256:zz"}}
            ]}}]"#
        );
        let releases = parse_releases(&json).expect("parse");
        let assets = &releases[0].assets;
        assert_eq!(assets[0].sha256, Some([0xab; 32]));
        assert_eq!(assets[1].sha256, None);
        assert_eq!(assets[2].sha256, None);
        assert_eq!(assets[3].sha256, None);
    }

    #[test]
    fn malformed_json_is_unreachable() {
        assert_eq!(
            parse_releases("{not json"),
            Err(UpdateError::GithubUnreachable)
        );
        assert_eq!(
            parse_releases(r#"{"message":"Not Found"}"#),
            Err(UpdateError::GithubUnreachable)
        );
    }

    #[test]
    fn flavour_parses_marker_contents() {
        assert_eq!(Flavour::parse("prod\n"), Some(Flavour::Prod));
        assert_eq!(Flavour::parse("dev"), Some(Flavour::Dev));
        assert_eq!(Flavour::parse("other"), None);
    }
}
