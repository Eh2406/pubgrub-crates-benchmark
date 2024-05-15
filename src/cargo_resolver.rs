use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::task::Poll;

use cargo::core::dependency::DepKind;
use cargo::core::resolver::{self, ResolveOpts, VersionPreferences};
use cargo::core::Resolve;
use cargo::core::ResolveVersion;
use cargo::core::SourceId;
use cargo::core::{Dependency, PackageId, Registry, Summary};
use cargo::sources::source::QueryKind;
use cargo::sources::IndexSummary;
use cargo::util::interning::InternedString;
use cargo::util::{CargoResult, IntoUrl};
use internment::ArcIntern;
use itertools::Itertools;

impl<'a> Registry for crate::Index<'a> {
    fn query(
        &mut self,
        dep: &Dependency,
        kind: QueryKind,
        f: &mut dyn FnMut(IndexSummary),
    ) -> Poll<CargoResult<()>> {
        let name: ArcIntern<str> = dep.package_name().as_str().into();
        if let Some(by_name) = self.crates.get(&name) {
            for ver in by_name.values() {
                let summary = ver.into();
                let matched = match kind {
                    QueryKind::Exact => dep.matches(&summary),
                    QueryKind::Alternatives => true,
                    QueryKind::Normalized => true,
                };
                if matched {
                    // TODO:
                    // self.dependencies
                    //     .borrow_mut()
                    //     .insert((package.clone(), version.clone()));
                    f(IndexSummary::Candidate(summary.clone()));
                }
            }
        }
        Poll::Ready(Ok(()))
    }

    fn describe_source(&self, _src: SourceId) -> String {
        String::new()
    }

    fn is_replaced(&self, _src: SourceId) -> bool {
        false
    }

    fn block_until_ready(&mut self) -> CargoResult<()> {
        Ok(())
    }
}

pub fn resolve<'c>(
    name: &str,
    ver: &semver::Version,
    dp: &mut crate::Index<'c>,
) -> CargoResult<Resolve> {
    let mut dep = Dependency::parse(InternedString::new(name), None, registry_loc()).unwrap();
    dep.set_version_req(semver::VersionReq::parse(&ver.to_string()).unwrap().into());

    let summary = Summary::new(
        PackageId::try_new("root", "1.0.0", registry_loc()).unwrap(),
        vec![dep],
        &BTreeMap::new(),
        None::<&String>,
        None,
    )
    .unwrap();
    let opts = ResolveOpts::everything();
    let version_prefs = VersionPreferences::default();
    resolver::resolve(
        &[(summary, opts)],
        &[],
        dp,
        &version_prefs,
        ResolveVersion::with_rust_version(None),
        None,
    )
}

impl From<&crate::index_data::Dependency> for Dependency {
    fn from(value: &crate::index_data::Dependency) -> Self {
        let mut out =
            Dependency::parse(InternedString::new(&*value.name), None, registry_loc()).unwrap();
        if value.name != value.package_name {
            out.set_explicit_name_in_toml(&*value.package_name);
        }
        out.set_version_req((&*value.req).clone().into());
        out.set_features(value.features.iter().map(|s| &**s));
        out.set_default_features(value.default_features);
        out.set_kind(match &value.kind {
            crates_index::DependencyKind::Normal => DepKind::Normal,
            crates_index::DependencyKind::Dev => DepKind::Development,
            crates_index::DependencyKind::Build => DepKind::Build,
        });
        out.set_optional(value.optional);
        out
    }
}

impl From<&crate::index_data::Version> for Summary {
    fn from(value: &crate::index_data::Version) -> Self {
        let pid = PackageId::try_new(
            &*value.name,
            value.vers.to_string().as_str(),
            registry_loc(),
        )
        .unwrap();
        let dep = value.deps.iter().map(|d| d.into()).collect_vec();
        let features = value
            .features
            .iter()
            .map(|(f, v)| {
                (
                    InternedString::new(&*f),
                    v.iter().map(|f| InternedString::new(&*f)).collect(),
                )
            })
            .collect();
        Summary::new(
            pid,
            dep,
            &features,
            value.links.as_ref().map(|s| InternedString::new(&*s)),
            None,
        )
        .unwrap()
    }
}

fn registry_loc() -> SourceId {
    static EXAMPLE_DOT_COM: OnceLock<SourceId> = OnceLock::new();
    let example_dot = EXAMPLE_DOT_COM.get_or_init(|| {
        SourceId::for_registry(&"https://example.com".into_url().unwrap()).unwrap()
    });
    *example_dot
}
