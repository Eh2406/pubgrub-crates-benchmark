use std::{
    cell::{Cell, RefCell},
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    error::Error,
    fs::File,
    hash::{Hash, Hasher},
    io::{BufWriter, Write},
    ops::Bound,
    rc::Rc,
    sync::mpsc,
    thread::spawn,
    time::Instant,
};

use cargo::{core::Summary, util::interning::InternedString};
use crates_index::DependencyKind;
use hasher::StableHasher;
use indicatif::{ParallelProgressIterator as _, ProgressBar, ProgressFinish, ProgressStyle};
use itertools::Itertools as _;
use names::{from_dep, new_bucket, new_links, new_wide, FeatureNamespace, Names};
use pubgrub::{
    error::PubGrubError,
    solver::resolve,
    solver::{Dependencies, DependencyProvider},
    type_aliases::{DependencyConstraints, SelectedDependencies},
    version_set::VersionSet as _,
};
use rayon::iter::{IntoParallelRefIterator as _, ParallelIterator as _};
use ron::ser::PrettyConfig;
use semver_pubgrub::{SemverCompatibility, SemverPubgrub};

mod hasher;
mod index_data;
mod names;

mod read_index;
use read_index::read_index;

mod cargo_resolver;

#[cfg(test)]
use read_index::read_test_file;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const TIME_MAKE_FILE: f32 = 40.0;
const TIME_CUT_OFF: f32 = TIME_MAKE_FILE * 4.0;

#[derive(Clone)]
struct Index<'c> {
    crates: &'c HashMap<InternedString, BTreeMap<semver::Version, index_data::Version>>,
    cargo_crates: &'c HashMap<InternedString, BTreeMap<semver::Version, Summary>>,
    past_result: Option<HashMap<InternedString, HashSet<semver::Version>>>,
    dependencies: RefCell<HashSet<(InternedString, semver::Version)>>,
    pubgrub_dependencies: RefCell<HashSet<(Rc<Names<'c>>, semver::Version)>>,
    start: Cell<Instant>,
    call_count: Cell<u64>,
}

impl<'c> Index<'c> {
    pub fn new(
        crates: &'c HashMap<InternedString, BTreeMap<semver::Version, index_data::Version>>,
        cargo_crates: &'c HashMap<InternedString, BTreeMap<semver::Version, Summary>>,
    ) -> Self {
        Self {
            crates,
            cargo_crates,
            past_result: None,
            pubgrub_dependencies: Default::default(),
            dependencies: Default::default(),
            start: Cell::new(Instant::now()),
            call_count: Cell::new(0),
        }
    }

    fn reset_time(&mut self) {
        *self.start.get_mut() = Instant::now();
    }

    fn reset(&mut self) {
        self.past_result = None;
        self.dependencies.get_mut().clear();
        self.pubgrub_dependencies.get_mut().clear();
        *self.start.get_mut() = Instant::now();
    }

    fn duration(&self) -> f32 {
        self.start.get().elapsed().as_secs_f32()
    }

    fn make_pubgrub_ron_file(&self) {
        let mut dependency_provider: BTreeMap<_, BTreeMap<_, Result<_, _>>> = BTreeMap::new();
        let deps = self
            .pubgrub_dependencies
            .borrow()
            .iter()
            .cloned()
            .collect_vec();

        let Some(name) = deps
            .iter()
            .find(|(name, _)| matches!(&**name, Names::Bucket(_, _, all) if *all))
        else {
            panic!("no root")
        };

        for (package, version) in &deps {
            match self.get_dependencies(package, version).unwrap() {
                Dependencies::Unavailable(s) => {
                    dependency_provider
                        .entry(package.clone())
                        .or_default()
                        .insert(version.clone(), Err(s));
                }
                Dependencies::Available(dependencies) => {
                    dependency_provider
                        .entry(package.clone())
                        .or_default()
                        .insert(version.clone(), Ok(dependencies));
                }
            }
        }

        let file_name = format!("out/pubgrub_ron/{}@{}.ron", name.0.crate_(), name.1);
        let mut file = BufWriter::new(File::create(&file_name).unwrap());
        ron::ser::to_writer_pretty(&mut file, &dependency_provider, PrettyConfig::new()).unwrap();
        file.flush().unwrap();
    }

    fn make_index_ron_data(&self) -> Vec<index_data::Version> {
        let deps = self.dependencies.borrow();

        let name_vers: BTreeSet<_> = deps.iter().map(|(n, v)| (n.as_str(), v)).collect();

        name_vers
            .into_iter()
            .map(|(n, version)| self.crates[n][version].clone())
            .collect()
    }

    fn make_index_ron_file(&self) {
        let grub_deps = self.pubgrub_dependencies.borrow();

        let name = grub_deps
            .iter()
            .find(|(name, _)| matches!(&**name, Names::Bucket(_, _, all) if *all))
            .unwrap();

        let out = self.make_index_ron_data();

        let file_name = format!("out/index_ron/{}@{}.ron", name.0.crate_(), name.1);
        let mut file = BufWriter::new(File::create(&file_name).unwrap());
        ron::ser::to_writer_pretty(&mut file, &out, PrettyConfig::new()).unwrap();
        file.flush().unwrap();
    }

    fn get_versions<Q>(&self, name: &Q) -> impl Iterator<Item = &'c semver::Version> + '_
    where
        Q: ?Sized + Hash + Eq,
        InternedString: std::borrow::Borrow<Q>,
    {
        let past = self.past_result.as_ref().map(|p| p.get(name));
        self.crates
            .get(name)
            .into_iter()
            .flat_map(|m| m.keys())
            .rev()
            .filter(move |&v| {
                let Some(past) = past else {
                    return true;
                };
                let Some(past) = past else {
                    return false;
                };
                past.contains(v)
            })
    }

    fn get_version<Q>(&self, name: &Q, ver: &semver::Version) -> Option<&'c index_data::Version>
    where
        Q: ?Sized + Hash + Eq,
        InternedString: std::borrow::Borrow<Q>,
    {
        if let Some(past) = &self.past_result {
            past.get(name)?.get(ver)?;
        }
        self.crates.get(name)?.get(ver)
    }

    #[must_use]
    fn check(&self, root: Rc<Names>, pubmap: &SelectedDependencies<Self>) -> bool {
        // Basic dependency resolution properties
        if !pubmap.contains_key(&root) {
            return false;
        }
        for (name, ver) in pubmap {
            let Dependencies::Available(deps) = self.get_dependencies(name, ver).unwrap() else {
                return false;
            };
            for (dep, req) in deps {
                let Some(dep_ver) = pubmap.get(&dep) else {
                    return false;
                };
                if !req.contains(dep_ver) {
                    return false;
                }
            }
        }

        let mut vertions: HashMap<
            (&str, SemverCompatibility),
            (semver::Version, BTreeSet<_>, BTreeSet<_>, bool),
        > = HashMap::new();
        // Identify the selected packages
        for (names, ver) in pubmap {
            if let Names::Bucket(name, cap, is_root) = &**names {
                if cap != &SemverCompatibility::from(ver) {
                    return false;
                }
                if *is_root {
                    continue;
                }
                let old_val = vertions.insert(
                    (*name, *cap),
                    (ver.clone(), BTreeSet::new(), BTreeSet::new(), false),
                );

                if old_val.is_some() {
                    return false;
                }
            }
        }
        // Identify the selected package features and deps
        for (name, ver) in pubmap {
            if let Names::BucketFeatures(name, cap, feat) = &**name {
                if cap != &SemverCompatibility::from(ver) {
                    return false;
                }
                let old_val = vertions.get_mut(&(name, *cap)).unwrap();
                if &old_val.0 != ver {
                    return false;
                }
                let old_feat = match *feat {
                    FeatureNamespace::Feat(f) => old_val.1.insert(f),
                    FeatureNamespace::Dep(f) => old_val.2.insert(f),
                };
                if !old_feat {
                    return false;
                }
            }
        }
        for (name, ver) in pubmap {
            if let Names::BucketDefaultFeatures(name, cap) = &**name {
                if cap != &SemverCompatibility::from(ver) {
                    return false;
                }
                let old_val = vertions.get_mut(&(name, *cap)).unwrap();
                if &old_val.0 != ver {
                    return false;
                }
                if old_val.3 {
                    return false;
                }
                old_val.3 = true;
            }
        }

        let mut links: BTreeSet<_> = BTreeSet::new();
        for ((name, _), (ver, feats, deps, default_feature)) in vertions.iter() {
            let index_ver = self.get_version(*name, ver).unwrap();
            if index_ver.yanked {
                return false;
            }
            if let Some(link) = &index_ver.links {
                let old_link = links.insert(link.clone());
                if !old_link {
                    return false;
                }
            }

            if *default_feature {
                if index_ver.features.contains_key("default") != feats.contains("default") {
                    return false;
                }
            }

            for dep in index_ver.deps.iter() {
                if dep.optional && !deps.contains(&*dep.name) {
                    continue;
                }
                if index_ver.features.contains_key(&*dep.name) {
                    continue;
                }
                if dep.kind == DependencyKind::Dev {
                    continue;
                }

                // Check for something that meets that dep
                let fulfilled = vertions.iter().find(
                    |(
                        (other_name, _),
                        (other_ver, other_feats, _other_deps, other_default_feature),
                    )| {
                        **other_name == *dep.package_name
                            && dep.req.matches(other_ver)
                            && dep
                                .features
                                .iter()
                                .all(|f| f.is_empty() || other_feats.contains(&**f))
                            && (!dep.default_features || *other_default_feature)
                    },
                );
                if fulfilled.is_none() {
                    return false;
                }
            }

            // todo: check index_ver.features
        }
        true
    }
}

#[derive(Debug)]
pub struct SomeError;

impl std::fmt::Display for SomeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SomeError").finish()
    }
}

impl Error for SomeError {}

fn deps_insert<'c>(
    deps: &mut DependencyConstraints<Rc<Names<'c>>, SemverPubgrub>,
    n: Rc<Names<'c>>,
    r: SemverPubgrub,
) {
    deps.entry(n)
        .and_modify(|old_r| *old_r = old_r.intersection(&r))
        .or_insert(r);
}

impl<'c> DependencyProvider for Index<'c> {
    type P = Rc<Names<'c>>;

    type V = semver::Version;

    type VS = SemverPubgrub;

    type M = String;
    type Err = SomeError;
    fn choose_version(
        &self,
        package: &Rc<Names>,
        range: &SemverPubgrub,
    ) -> Result<Option<semver::Version>, Self::Err> {
        Ok(match &**package {
            Names::Links(_name) => {
                let Some((_, Bound::Included(v))) = range.bounding_range() else {
                    return Err(SomeError);
                };
                Some(v.clone())
            }

            Names::Wide(_, req, _, _)
            | Names::WideFeatures(_, req, _, _, _)
            | Names::WideDefaultFeatures(_, req, _, _) => {
                // one version for each bucket that match req
                self.get_versions(&*package.crate_())
                    .filter(|v| req.matches(v))
                    .map(|v| SemverCompatibility::from(v))
                    .map(|v| v.canonical())
                    .find(|v| range.contains(v))
            }
            Names::Bucket(_, _, _)
            | Names::BucketFeatures(_, _, _)
            | Names::BucketDefaultFeatures(_, _) => self
                .get_versions(&*package.crate_())
                .find(|v| range.contains(v))
                .cloned(),
        })
    }

    type Priority = Reverse<usize>;

    fn prioritize(&self, package: &Rc<Names>, range: &SemverPubgrub) -> Self::Priority {
        Reverse(match &**package {
            Names::Links(_name) => {
                // PubGrub automatically handles when any requirement has no overlap. So this is only deciding a importance of picking the version:
                //
                // - If it only matches one thing, then adding the decision with no additional dependencies makes no difference.
                // - If it can match more than one thing, and it is entirely equivalent to picking the packages directly which would make more sense to the users.
                //
                // So only rubberstamp links attributes when all other decisions are made, by setting the priority as low as it will go.
                usize::MAX
            }

            Names::Wide(_, req, _, _) | Names::WideFeatures(_, req, _, _, _) => {
                // one version for each bucket that match req
                self.get_versions(&*package.crate_())
                    .filter(|v| req.matches(v))
                    .map(|v| SemverCompatibility::from(v))
                    .dedup()
                    .map(|v| v.canonical())
                    .filter(|v| range.contains(v))
                    .count()
            }
            _ => self
                .get_versions(&*package.crate_())
                .filter(|v| range.contains(v))
                .count(),
        })
    }

    fn get_dependencies(
        &self,
        package: &Rc<Names<'c>>,
        version: &semver::Version,
    ) -> Result<Dependencies<Self::P, Self::VS, Self::M>, Self::Err> {
        self.pubgrub_dependencies
            .borrow_mut()
            .insert((package.clone(), version.clone()));
        Ok(match &**package {
            &Names::Bucket(name, _major, all_features) => {
                let index_ver = self.get_version(name, version).unwrap();
                self.dependencies
                    .borrow_mut()
                    .insert((index_ver.name, version.clone()));
                if index_ver.yanked {
                    return Ok(Dependencies::Unavailable("yanked: Bucket".into()));
                }
                let mut deps = DependencyConstraints::default();
                if let Some(link) = &index_ver.links {
                    let index_unique_to_each_crate_version = {
                        let mut state = StableHasher::new();
                        package.hash(&mut state);
                        version.hash(&mut state);
                        state.finish()
                    };
                    let ver = semver::Version::new(index_unique_to_each_crate_version, 0, 0);
                    deps.insert(new_links(link), SemverPubgrub::singleton(ver));
                }
                for dep in index_ver.deps.iter() {
                    if dep.kind == DependencyKind::Dev && !all_features {
                        continue;
                    }
                    if dep.optional && !all_features {
                        continue; // handled in Names::Features
                    }

                    let (cray, req_range) = from_dep(&dep, name, version);

                    if &cray == package {
                        return Ok(Dependencies::Unavailable("self dep: Bucket".into()));
                    }
                    deps_insert(&mut deps, cray.clone(), req_range.clone());

                    if dep.default_features {
                        deps_insert(&mut deps, cray.with_default_features(), req_range.clone());
                    }
                    for f in &*dep.features {
                        deps_insert(
                            &mut deps,
                            cray.with_features(FeatureNamespace::new(f)),
                            req_range.clone(),
                        );
                    }
                }
                if all_features {
                    for vals in index_ver.features.values() {
                        for val in &**vals {
                            if let Some((dep, dep_feat)) = val.split_once('/') {
                                let dep_name = dep.strip_suffix('?').unwrap_or(dep);
                                for com in index_ver.deps.get(dep_name) {
                                    let (cray, req_range) = from_dep(com, name, version);
                                    deps_insert(
                                        &mut deps,
                                        cray.with_features(FeatureNamespace::new(dep_feat)),
                                        req_range.clone(),
                                    );
                                }
                            }
                        }
                    }
                }
                Dependencies::Available(deps)
            }
            Names::BucketFeatures(name, _major, FeatureNamespace::Feat(feat)) => {
                let index_ver = self.get_version(*name, version).unwrap();
                self.dependencies
                    .borrow_mut()
                    .insert((index_ver.name, version.clone()));
                if index_ver.yanked {
                    return Ok(Dependencies::Unavailable(
                        "yanked: BucketFeatures Feat".into(),
                    ));
                }
                let mut deps = DependencyConstraints::default();
                deps.insert(
                    new_bucket(name, version.into(), false),
                    SemverPubgrub::singleton(version.clone()),
                );

                if let Some(vals) = index_ver.features.get(*feat) {
                    for val in &**vals {
                        if let Some((dep, dep_feat)) = val.split_once('/') {
                            let dep_name = dep.strip_suffix('?');
                            let week = dep_name.is_some();
                            let dep_name = dep_name.unwrap_or(dep);

                            for dep in index_ver.deps.get(dep_name) {
                                if dep.kind == DependencyKind::Dev {
                                    continue;
                                }
                                let (cray, req_range) = from_dep(dep, name, version);

                                if dep.optional {
                                    deps_insert(
                                        &mut deps,
                                        package.with_features(FeatureNamespace::Dep(dep_name)),
                                        SemverPubgrub::singleton(version.clone()),
                                    );

                                    if !week
                                        && dep_name != *feat
                                        && index_ver.features.contains_key(dep_name)
                                    {
                                        deps_insert(
                                            &mut deps,
                                            package.with_features(FeatureNamespace::Feat(dep_name)),
                                            SemverPubgrub::singleton(version.clone()),
                                        );
                                    }
                                }
                                deps_insert(
                                    &mut deps,
                                    cray.with_features(FeatureNamespace::Feat(dep_feat)),
                                    req_range.clone(),
                                );
                            }
                        } else {
                            deps_insert(
                                &mut deps,
                                package.with_features(FeatureNamespace::new(val)),
                                SemverPubgrub::singleton(version.clone()),
                            );
                        }
                    }
                    return Ok(Dependencies::Available(deps));
                }
                if index_ver.explicitly_named_deps.contains(*feat) {
                    return Ok(Dependencies::Unavailable(
                        "no matching feat (dep not a feat becuse of dep:)".into(),
                    ));
                }
                deps_insert(
                    &mut deps,
                    package.with_features(FeatureNamespace::Dep(feat)),
                    SemverPubgrub::singleton(version.clone()),
                );

                Dependencies::Available(deps)
            }
            Names::BucketDefaultFeatures(name, _major) => {
                let index_ver = self.get_version(*name, version).unwrap();
                self.dependencies
                    .borrow_mut()
                    .insert((index_ver.name, version.clone()));
                if index_ver.yanked {
                    return Ok(Dependencies::Unavailable(
                        "yanked: BucketFeatures DefaultFeatures".into(),
                    ));
                }
                let mut deps = DependencyConstraints::default();
                deps.insert(
                    new_bucket(name, version.into(), false),
                    SemverPubgrub::singleton(version.clone()),
                );

                if index_ver.features.contains_key("default") {
                    deps_insert(
                        &mut deps,
                        package.with_features(FeatureNamespace::Feat("default")),
                        SemverPubgrub::singleton(version.clone()),
                    );
                }

                Dependencies::Available(deps)
            }
            Names::BucketFeatures(name, _major, FeatureNamespace::Dep(feat)) => {
                let index_ver = self.get_version(*name, version).unwrap();
                if index_ver.yanked {
                    return Ok(Dependencies::Unavailable(
                        "yanked: BucketFeatures Dep".into(),
                    ));
                }
                let mut deps = DependencyConstraints::default();
                deps.insert(
                    new_bucket(name, version.into(), false),
                    SemverPubgrub::singleton(version.clone()),
                );

                let mut found_name = false;
                for dep in index_ver.deps.get(*feat) {
                    if !dep.optional {
                        continue;
                    }
                    if dep.kind == DependencyKind::Dev {
                        continue;
                    }
                    found_name = true;
                    let (cray, req_range) = from_dep(&dep, name, version);

                    deps_insert(&mut deps, cray.clone(), req_range.clone());

                    if dep.default_features {
                        deps_insert(&mut deps, cray.with_default_features(), req_range.clone());
                    }
                    for f in &*dep.features {
                        deps_insert(
                            &mut deps,
                            cray.with_features(FeatureNamespace::new(f)),
                            req_range.clone(),
                        );
                    }
                }

                if found_name {
                    Dependencies::Available(deps)
                } else {
                    Dependencies::Unavailable("no matching feat".into())
                }
            }
            Names::Wide(name, req, _, _) => {
                let compatibility = SemverCompatibility::from(version);
                let compat_range = SemverPubgrub::from(&compatibility);
                let req_range = SemverPubgrub::from(*req);
                let range = req_range.intersection(&compat_range);
                Dependencies::Available(DependencyConstraints::from_iter([(
                    new_bucket(name, compatibility, false),
                    range,
                )]))
            }
            Names::WideFeatures(name, req, parent, parent_com, feat) => {
                let compatibility = SemverCompatibility::from(version);
                let compat_range = SemverPubgrub::from(&compatibility);
                let req_range = SemverPubgrub::from(*req);
                let range = req_range.intersection(&compat_range);
                Dependencies::Available(DependencyConstraints::from_iter([
                    (
                        new_wide(name, req, parent, parent_com.clone()),
                        SemverPubgrub::singleton(version.clone()),
                    ),
                    (
                        new_bucket(name, compatibility, false).with_features(*feat),
                        range,
                    ),
                ]))
            }
            Names::WideDefaultFeatures(name, req, parent, parent_com) => {
                let compatibility = SemverCompatibility::from(version);
                let compat_range = SemverPubgrub::from(&compatibility);
                let req_range = SemverPubgrub::from(*req);
                let range = req_range.intersection(&compat_range);
                Dependencies::Available(DependencyConstraints::from_iter([
                    (
                        new_wide(name, req, parent, parent_com.clone()),
                        SemverPubgrub::singleton(version.clone()),
                    ),
                    (
                        new_bucket(name, compatibility, false).with_default_features(),
                        range,
                    ),
                ]))
            }
            Names::Links(_) => Dependencies::Available(DependencyConstraints::default()),
        })
    }

    fn should_cancel(&self) -> Result<(), Self::Err> {
        let calls = self.call_count.get();
        self.call_count.set(calls + 1);
        if calls % 64 == 0 && TIME_CUT_OFF < self.start.get().elapsed().as_secs_f32() {
            return Err(SomeError);
        }
        Ok(())
    }
}

fn process_carte_version<'c>(
    dp: &mut Index<'c>,
    crt: InternedString,
    ver: semver::Version,
) -> OutPutSummery {
    let root = new_bucket(crt.as_str(), (&ver).into(), true);
    dp.reset();
    let res = resolve(dp, root.clone(), (&ver).clone());
    let duration = dp.duration();
    match res.as_ref() {
        Ok(map) => {
            if !dp.check(root.clone(), &map) {
                dp.make_index_ron_file();
                panic!("failed check");
            }
        }
        Err(PubGrubError::NoSolution(_derivation)) => {}
        Err(e) => {
            dp.make_index_ron_file();
            dbg!(e);
        }
    }
    if duration > TIME_MAKE_FILE {
        dp.make_index_ron_file();
    }

    dp.reset_time();
    let cargo_out = cargo_resolver::resolve(crt, &ver, dp);
    let cargo_duration = dp.duration();

    // TODO: check for cyclic package dependency!
    let cyclic_package_dependency = &cargo_out
        .as_ref()
        .map_err(|e| e.to_string().starts_with("cyclic package dependency"))
        == &Err(true);

    if !cyclic_package_dependency && res.is_ok() != cargo_out.is_ok() {
        dp.make_index_ron_file();
        println!("failed to match cargo {root:?}");
    }

    if duration > 20.0 * (cargo_duration + 0.015) {
        dp.make_index_ron_file();
        println!("pub 20x slower than cargo {root:?}");
    }

    let mut cargo_check_pub_lock_res = false;
    let mut cargo_check_pub_lock_time = 0.0;
    if res.is_ok() {
        dp.past_result = res
            .as_ref()
            .map(|map| {
                let mut results: HashMap<InternedString, HashSet<semver::Version>> = HashMap::new();
                for (k, v) in map.iter() {
                    if k.is_real() {
                        results
                            .entry(k.crate_().into())
                            .or_default()
                            .insert(v.clone());
                    }
                }
                results
            })
            .ok();
        dp.reset_time();
        let cargo_check_pub_lock_out = cargo_resolver::resolve(crt, &ver, dp);
        cargo_check_pub_lock_time = dp.duration();
        cargo_check_pub_lock_res = cargo_check_pub_lock_out.is_ok();

        let cyclic_package_dependency_pub_lock = &cargo_check_pub_lock_out
            .as_ref()
            .map_err(|e| e.to_string().starts_with("cyclic package dependency"))
            == &Err(true);

        if !cyclic_package_dependency_pub_lock && !cargo_check_pub_lock_out.is_ok() {
            dp.make_index_ron_file();
            println!("failed to match pub lock cargo {root:?}");
        }
    }

    let mut pub_check_cargo_lock_res = false;
    let mut pub_check_cargo_lock_time = 0.0;
    if cargo_out.is_ok() {
        dp.past_result = cargo_out
            .as_ref()
            .map(|map| {
                let mut results: HashMap<InternedString, HashSet<semver::Version>> = HashMap::new();
                for v in map.iter() {
                    results
                        .entry(v.name())
                        .or_default()
                        .insert(v.version().clone());
                }
                results
            })
            .ok();
        dp.reset_time();
        let pub_check_cargo_lock_out = resolve(dp, root.clone(), ver.clone());
        pub_check_cargo_lock_time = dp.duration();
        pub_check_cargo_lock_res = pub_check_cargo_lock_out.is_ok();

        if !pub_check_cargo_lock_out.is_ok() {
            dp.make_index_ron_file();
            println!("failed to match cargo lock pub {root:?}");
        }
    }

    OutPutSummery {
        name: crt,
        ver,
        time: duration,
        succeeded: res.is_ok(),
        pubgrub_deps: res.as_ref().map(|r| r.len()).unwrap_or(0),
        deps: res
            .as_ref()
            .map(|r| r.iter().filter(|(v, _)| v.is_real()).count())
            .unwrap_or(0),
        cargo_time: cargo_duration,
        cargo_res: cargo_out.is_ok(),
        cyclic_package_dependency,
        cargo_deps: cargo_out.as_ref().map(|r| r.iter().count()).unwrap_or(0),
        cargo_check_pub_lock_time,
        cargo_check_pub_lock_res,
        pub_check_cargo_lock_time,
        pub_check_cargo_lock_res,
    }
}

#[derive(serde::Serialize)]
struct OutPutSummery {
    name: InternedString,
    ver: semver::Version,
    time: f32,
    succeeded: bool,
    pubgrub_deps: usize,
    deps: usize,
    cargo_time: f32,
    cargo_res: bool,
    cyclic_package_dependency: bool,
    cargo_deps: usize,
    cargo_check_pub_lock_time: f32,
    cargo_check_pub_lock_res: bool,
    pub_check_cargo_lock_time: f32,
    pub_check_cargo_lock_res: bool,
}

#[cfg(test)]
mod tests;

fn main() {
    // let create_filter = |name: &str| !name.contains("solana");
    // println!("!!!!!!!!!! Excluding Solana Crates !!!!!!!!!!");
    let version_filter =
        |version: &index_data::Version| !version.yanked && Summary::try_from(version).is_ok();
    println!("!!!!!!!!!! Excluding Yanked and Non Cargo Summary Versions !!!!!!!!!!");

    let index =
        crates_index::GitIndex::with_path("index", "https://github.com/rust-lang/crates.io-index")
            .unwrap();
    let (data, cargo_crates) = read_index(&index, |_| true, version_filter);

    let (tx, rx) = mpsc::channel::<OutPutSummery>();

    let file_handle = spawn(|| {
        let mut out_file = csv::Writer::from_path("out.csv").unwrap();
        let start = Instant::now();
        let mut pub_cpu_time = 0.0;
        let mut cargo_cpu_time = 0.0;
        let mut cargo_pub_lock_cpu_time = 0.0;
        let mut pub_cargo_lock_cpu_time = 0.0;
        for row in rx {
            pub_cpu_time += row.time;
            cargo_cpu_time += row.cargo_time;
            cargo_pub_lock_cpu_time += row.cargo_check_pub_lock_time;
            pub_cargo_lock_cpu_time += row.pub_check_cargo_lock_time;
            out_file.serialize(row).unwrap();
        }
        out_file.flush().unwrap();
        (
            pub_cpu_time,
            cargo_cpu_time,
            cargo_pub_lock_cpu_time,
            pub_cargo_lock_cpu_time,
            start.elapsed().as_secs_f32(),
        )
    });

    let names = [
        // "addr2line",
        // "adler",
        // "ahash",
        // "aho-corasick",
        // "android_system_properties",
        // "android-tzdata",
        // "ansi_term",
        // "arrayref",
        // "arrayvec",
        // "ascii",
        // "assert_matches",
        // "atty",
        // "autocfg",
        // "backtrace",
        // "base64",
        // "bincode",
        // "bitflags",
        // "blake3",
        // "block-buffer",
        // "block-padding",
        // "borsh",
        // "borsh-derive",
        // "borsh-derive-internal",
        // "borsh-schema-derive-internal",
        // "bs58",
        // "bumpalo",
        // "bv",
        // "byteorder",
        // "bytes",
        // "byte-tools",
        // "cc",
        // "cfg-if",
        // "chrono",
        // "clap",
        // "combine",
        // "console",
        // "console_error_panic_hook",
        // "console_log",
        // "constant_time_eq",
        // "core-foundation",
        // "core-foundation-sys",
        // "cpufeatures",
        // "crossbeam-channel",
        // "crossbeam-deque",
        // "crossbeam-epoch",
        // "crossbeam-utils",
        // "crunchy",
        // "crypto-mac",
        // "curve25519-dalek",
        // "derivation-path",
        // "dialoguer",
        // "digest",
        // "dlopen",
        // "dlopen_derive",
        // "ed25519-dalek",
        // "ed25519-dalek-bip32",
        // "either",
        // "encode_unicode",
        // "encoding_rs",
        // "env_logger",
        // "errno",
        // "failure",
        // "fake-simd",
        // "fastrand",
        // "feature-probe",
        // "fnv",
        // "form_urlencoded",
        // "fuchsia-zircon",
        // "fuchsia-zircon-sys",
        // "futures",
        // "futures-channel",
        // "futures-core",
        // "futures-io",
        // "futures-macro",
        // "futures-sink",
        // "futures-task",
        // "futures-util",
        // "generic-array",
        // "gethostname",
        // "getrandom",
        // "gimli",
        // "h2",
        // "hash32",
        // "hashbrown",
        // "hermit-abi",
        // "hmac",
        // "hmac-drbg",
        // "http",
        // "httparse",
        // "http-body",
        // "httpdate",
        // "humantime",
        // "hyper",
        // "hyper-rustls",
        // "iana-time-zone",
        // "iana-time-zone-haiku",
        // "idna",
        // "im",
        // "indexmap",
        // "indicatif",
        // "iovec",
        // "ipnet",
        // "itertools",
        // "itoa",
        // "jobserver",
        // "jsonrpc-core",
        // "js-sys",
        // "keccak",
        // "kernel32-sys",
        // "lazy_static",
        // "libc",
        // "libsecp256k1",
        // "libsecp256k1-core",
        // "libsecp256k1-gen-ecmult",
        // "libsecp256k1-gen-genmult",
        // "light-poseidon",
        // "linux-raw-sys",
        // "lock_api",
        // "log",
        // "memchr",
        // "memmap2",
        // "memoffset",
        // "mime",
        // "mime_guess",
        // "miniz_oxide",
        // "mio",
        // "miow",
        // "native-tls",
        // "net2",
        // "nix",
        // "num_cpus",
        // "num_enum",
        // "num_enum_derive",
        // "number_prefix",
        // "num-derive",
        // "num-traits",
        // "object",
        // "once_cell",
        // "opaque-debug",
        // "openssl-probe",
        // "parking_lot",
        // "parking_lot_core",
        // "paste",
        // "pbkdf2",
        // "percent-encoding",
        // "pin-project",
        // "pin-project-internal",
        // "pin-project-lite",
        // "pin-utils",
        // "pkg-config",
        // "ppv-lite86",
        // "proc-macro2",
        // "proc-macro-crate",
        // "qstring",
        // "quote",
        // "rand",
        // "rand_chacha",
        // "rand_core",
        // "rand_hc",
        // "rand_xoshiro",
        // "rayon",
        // "rayon-core",
        // "redox_syscall",
        // "regex",
        // "regex-automata",
        // "regex-syntax",
        // "reqwest",
        // "ring",
        // "rpassword",
        // "rustc_version",
        // "rustc-demangle",
        // "rustc-hash",
        // "rustix",
        // "rustls",
        // "rustversion",
        // "ryu",
        // "schannel",
        // "scopeguard",
        // "sct",
        // "security-framework",
        // "security-framework-sys",
        // "semver",
        // "semver-parser",
        // "serde",
        // "serde_bytes",
        // "serde_derive",
        // "serde_json",
        // "serde_urlencoded",
        // "serde_with",
        // "serde_with_macros",
        // "sha2",
        // "sha3",
        // "slab",
        // "smallvec",
        // "socket2",
        // "solana_rbpf",
        // "solana-clap-utils",
        // "solana-client",
        // "solana-config-program",
        // "solana-crate-features",
        // "solana-faucet",
        // "solana-frozen-abi",
        // "solana-frozen-abi-macro",
        // "solana-logger",
        // "solana-measure",
        // "solana-metrics",
        // "solana-net-utils",
        // "solana-perf",
        // "solana-program",
        // "solana-rayon-threadlimit",
        // "solana-remote-wallet",
        // "solana-sdk",
        // "solana-sdk-macro",
        "solana-transaction-status",
        // "solana-version",
        // "solana-vote-program",
        // "spin",
        // "strsim",
        // "subtle",
        // "syn",
        // "synstructure",
        // "tempfile",
        // "termcolor",
        // "textwrap",
        // "thiserror",
        // "thiserror-impl",
        // "time",
        // "tiny-bip39",
        // "tinyvec",
        // "tinyvec_macros",
        // "tokio",
        // "tokio-rustls",
        // "tokio-util",
        // "toml",
        // "tower-service",
        // "tracing",
        // "tracing-core",
        // "tracing-futures",
        // "try-lock",
        // "tungstenite",
        // "typenum",
        // "unicase",
        // "unicode-bidi",
        // "unicode-ident",
        // "unicode-normalization",
        // "unicode-width",
        // "unicode-xid",
        // "untrusted",
        // "uriparse",
        // "url",
        // "utf-8",
        // "vcpkg",
        // "vec_map",
        // "version_check",
        // "void",
        // "want",
        // "wasi",
        // "wasm-bindgen",
        // "wasm-bindgen-backend",
        // "wasm-bindgen-futures",
        // "wasm-bindgen-macro",
        // "wasm-bindgen-macro-support",
        // "wasm-bindgen-shared",
        // "webpki",
        // "webpki-roots",
        // "web-sys",
        // "winapi",
        // "winapi-build",
        // "winapi-i686-pc-windows-gnu",
        // "winapi-util",
        // "winapi-x86_64-pc-windows-gnu",
        // "windows_aarch64_gnullvm",
        // "windows_aarch64_msvc",
        // "windows_i686_gnu",
        // "windows_i686_gnullvm",
        // "windows_i686_msvc",
        // "windows_x86_64_gnu",
        // "windows_x86_64_gnullvm",
        // "windows_x86_64_msvc",
        // "windows-core",
        // "windows-sys",
        // "windows-targets",
        // "winreg",
        // "ws2_32-sys",
        // "zeroize",
    ];

    let template = "PubGrub: [Time: {elapsed}, Rate: {per_sec}, Remaining: {eta}] {wide_bar} {pos:>6}/{len:6}: {percent:>3}%";
    let style = ProgressBar::new(
        data.iter()
            .filter(|(name, _)| names.contains(&name.as_str()))
            .map(|(_, v)| v.len())
            .sum::<usize>() as u64,
    )
    .with_style(ProgressStyle::with_template(template).unwrap())
    .with_finish(ProgressFinish::AndLeave);

    data.par_iter()
        .filter(|(name, _)| names.contains(&name.as_str()))
        .flat_map(|(c, v)| v.par_iter().map(|(v, _)| (c.clone(), v)))
        .progress_with(style)
        .map(|(crt, ver)| {
            process_carte_version(&mut Index::new(&data, &cargo_crates), crt, ver.clone())
        })
        .for_each(move |csv_line| {
            let _ = tx.send(csv_line);
        });

    let (pub_cpu_time, cargo_cpu_time, cargo_pub_lock_cpu_time, pub_cargo_lock_cpu_time, wall_time) =
        file_handle.join().unwrap();
    let p = |t: f32| {
        println!(
            " time: {:.2}s == {:.2}min == {:.2}hr",
            t,
            t / 60.0,
            t / 3600.0
        )
    };
    print!("Pub CPU");
    p(pub_cpu_time);

    print!("Cargo CPU");
    p(cargo_cpu_time);

    print!("Cargo check lock CPU");
    p(cargo_pub_lock_cpu_time);

    print!("Pub check lock CPU");
    p(pub_cargo_lock_cpu_time);

    print!("Wall");
    p(wall_time);
}
