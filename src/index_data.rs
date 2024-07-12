use std::collections::{BTreeMap, BTreeSet};

use cargo::util::interning::InternedString;
use internment::Intern;
use itertools::Itertools;

fn is_default<D: Default + PartialEq>(t: &D) -> bool {
    t == &D::default()
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq, Hash)]

struct RawIndexDependency<'da> {
    name: &'da str,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    package_name: &'da str,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    req: semver::VersionReq,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    features: Vec<&'da str>,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    default_features: bool,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    kind: crates_index::DependencyKind,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    optional: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Dependency {
    pub name: InternedString,
    pub package_name: InternedString,
    pub req: Intern<semver::VersionReq>,
    pub features: Intern<Vec<InternedString>>,
    pub default_features: bool,
    pub kind: crates_index::DependencyKind,
    pub optional: bool,
}

impl<'da> From<RawIndexDependency<'da>> for Dependency {
    fn from(value: RawIndexDependency<'da>) -> Self {
        Self {
            name: value.name.into(),
            package_name: if !is_default(&value.package_name) {
                value.package_name.into()
            } else {
                value.name.into()
            },
            req: value.req.into(),
            features: value
                .features
                .iter()
                .map(|s| InternedString::new(s))
                .collect::<Vec<_>>()
                .into(),
            default_features: value.default_features,
            kind: value.kind,
            optional: value.optional,
        }
    }
}

impl<'a> Into<RawIndexDependency<'static>> for &'a Dependency {
    fn into(self) -> RawIndexDependency<'static> {
        RawIndexDependency {
            name: self.name.as_str(),
            package_name: if self.name != self.package_name {
                self.package_name.as_str()
            } else {
                Default::default()
            },
            req: (&*self.req).clone(),
            features: self
                .features
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .into(),
            default_features: self.default_features,
            kind: self.kind,
            optional: self.optional,
        }
    }
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
            features: features.into(),
            kind: dep.kind(),
            optional: dep.is_optional(),
            default_features: dep.has_default_features(),
        })
    }
}

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct DependencyList {
    deps: BTreeMap<InternedString, Intern<Vec<Dependency>>>,
}

impl DependencyList {
    pub fn iter(&self) -> impl Iterator<Item = &Dependency> {
        self.deps.iter().flat_map(|(_, v)| v.iter())
    }

    pub fn get<Q: ?Sized>(&self, name: &Q) -> &[Dependency]
    where
        InternedString: std::borrow::Borrow<Q>,
        Q: std::cmp::Ord,
    {
        self.deps.get(name).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

impl<'da> From<Vec<RawIndexDependency<'da>>> for DependencyList {
    fn from(value: Vec<RawIndexDependency<'da>>) -> Self {
        let mut deps: BTreeMap<InternedString, Vec<Dependency>> = BTreeMap::new();
        for dep in value {
            deps.entry(dep.name.into()).or_default().push(dep.into());
        }
        DependencyList {
            deps: deps.into_iter().map(|(k, v)| (k, Intern::new(v))).collect(),
        }
    }
}

impl Into<Vec<RawIndexDependency<'static>>> for DependencyList {
    fn into(self) -> Vec<RawIndexDependency<'static>> {
        self.deps
            .iter()
            .flat_map(|(_, v)| v.iter())
            .map(|d| d.into())
            .collect()
    }
}

fn default_semver_version_for_serde() -> semver::Version {
    semver::Version::new(0, 0, 1)
}

fn is_default_semver_version_for_serde(t: &semver::Version) -> bool {
    t == &default_semver_version_for_serde()
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct RawIndexVersion<'da> {
    name: &'da str,
    #[serde(skip_serializing_if = "is_default_semver_version_for_serde")]
    #[serde(default = "default_semver_version_for_serde")]
    vers: semver::Version,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    deps: Vec<RawIndexDependency<'da>>,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    features: BTreeMap<&'da str, BTreeSet<&'da str>>,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    links: Option<&'da str>,
    #[serde(skip_serializing_if = "is_default")]
    #[serde(default)]
    yanked: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(from = "RawIndexVersion")]
#[serde(into = "RawIndexVersion")]
pub struct Version {
    pub name: InternedString,
    pub vers: Intern<semver::Version>,
    pub deps: DependencyList,
    pub explicitly_named_deps: BTreeSet<InternedString>,
    pub features: Intern<BTreeMap<InternedString, Intern<BTreeSet<InternedString>>>>,
    pub links: Option<InternedString>,
    pub yanked: bool,
}

#[cfg(test)]
impl Version {
    pub(crate) fn without_features(self) -> Option<Self> {
        if !self.features.is_empty() {
            Some(Self {
                features: Intern::new(Default::default()),
                ..self
            })
        } else {
            None
        }
    }
    pub(crate) fn without_deps(self) -> Option<Self> {
        if !self.deps.deps.is_empty() {
            Some(Self {
                deps: DependencyList::default(),
                ..self
            })
        } else {
            None
        }
    }
}

impl<'da> From<RawIndexVersion<'da>> for Version {
    fn from(value: RawIndexVersion<'da>) -> Self {
        Self {
            name: value.name.into(),
            vers: value.vers.into(),
            deps: value.deps.into(),
            explicitly_named_deps: value
                .features
                .values()
                .flat_map(|f| f.iter())
                .filter_map(|f| f.strip_prefix("dep:"))
                .map(|s| s.into())
                .collect(),
            features: value
                .features
                .iter()
                .map(|(&k, v)| (k.into(), Intern::new(v.iter().map(|&s| s.into()).collect())))
                .collect::<BTreeMap<_, _>>()
                .into(),
            links: value.links.map(|s| s.into()),
            yanked: value.yanked,
        }
    }
}

impl Into<RawIndexVersion<'static>> for Version {
    fn into(self) -> RawIndexVersion<'static> {
        RawIndexVersion {
            name: self.name.as_str(),
            vers: (&*self.vers).clone(),
            deps: self.deps.into(),
            features: self
                .features
                .iter()
                .map(|(&k, v)| (k.as_str(), v.iter().map(|s| s.as_str()).collect()))
                .collect(),
            links: self.links.map(|s| s.as_str()),
            yanked: self.yanked,
        }
    }
}

impl TryFrom<&crates_index::Version> for Version {
    type Error = semver::Error;

    fn try_from(ver: &crates_index::Version) -> Result<Self, Self::Error> {
        let mut deps: BTreeMap<InternedString, Vec<Dependency>> = BTreeMap::new();
        for dep in ver.dependencies() {
            deps.entry(dep.name().into())
                .or_default()
                .push(dep.try_into()?);
        }

        Ok(Version {
            name: ver.name().into(),
            vers: ver.version().parse::<semver::Version>()?.into(),
            deps: DependencyList {
                deps: deps.into_iter().map(|(k, v)| (k, Intern::new(v))).collect(),
            },
            explicitly_named_deps: ver
                .features()
                .values()
                .flat_map(|f| f.iter())
                .filter_map(|f| f.strip_prefix("dep:"))
                .map(|s| s.into())
                .collect(),
            features: Intern::new(
                ver.features()
                    .iter()
                    .map(|(f, ts)| {
                        (
                            f.as_str().into(),
                            Intern::new(ts.iter().map(|f| f.as_str().into()).collect()),
                        )
                    })
                    .collect(),
            ),
            links: ver.links().map(|s| s.into()),
            yanked: ver.is_yanked(),
        })
    }
}
