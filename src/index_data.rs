use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use itertools::Itertools;

fn is_default<D: Default + PartialEq>(t: &D) -> bool {
    t == &D::default()
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Dependency {
    pub name: Arc<str>,
    pub package_name: Arc<str>,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    pub req: Box<semver::VersionReq>,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    pub features: Vec<Arc<str>>,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    pub default_features: bool,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    pub kind: crates_index::DependencyKind,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    pub optional: bool,
}

impl TryFrom<&crates_index::Dependency> for Dependency {
    type Error = semver::Error;

    fn try_from(dep: &crates_index::Dependency) -> Result<Self, Self::Error> {
        let mut features = dep
            .features()
            .iter()
            .map(|s| s.as_str().into())
            .collect_vec();
        features.sort_unstable();
        Ok(Dependency {
            name: dep.name().into(),
            package_name: dep.crate_name().into(),
            req: dep.requirement().parse::<semver::VersionReq>()?.into(),
            features,
            kind: dep.kind(),
            optional: dep.is_optional(),
            default_features: dep.has_default_features(),
        })
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct Version {
    pub name: Arc<str>,
    pub vers: Box<semver::Version>,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    pub deps: Vec<Dependency>,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    pub features: BTreeMap<Arc<str>, BTreeSet<Arc<str>>>,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    pub links: Option<Arc<str>>,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    pub yanked: bool,
}

impl TryFrom<&crates_index::Version> for Version {
    type Error = semver::Error;

    fn try_from(ver: &crates_index::Version) -> Result<Self, Self::Error> {
        let mut deps: Vec<Dependency> = ver
            .dependencies()
            .iter()
            .map(|d| TryInto::<Dependency>::try_into(d))
            .collect::<Result<_, _>>()?;
        deps.sort_unstable_by_key(|d| d.package_name.clone());
        Ok(Version {
            name: ver.name().into(),
            vers: ver.version().parse::<semver::Version>()?.into(),
            deps,
            features: ver
                .features()
                .iter()
                .map(|(f, ts)| {
                    (
                        f.as_str().into(),
                        ts.iter().map(|f| f.as_str().into()).collect(),
                    )
                })
                .collect(),
            links: ver.links().map(|s| s.into()),
            yanked: ver.is_yanked(),
        })
    }
}
