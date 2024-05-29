use std::{
    cell::{Cell, RefCell},
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    error::Error,
    fs::File,
    hash::{Hash, Hasher},
    ops::{Bound, Deref},
    sync::{mpsc, Arc},
    thread::spawn,
    time::Instant,
};

use cargo::util::interning::InternedString;
use crates_index::DependencyKind;
use hasher::StableHasher;
use indicatif::{ParallelProgressIterator, ProgressBar, ProgressFinish, ProgressStyle};
use itertools::Itertools as _;
use names::{new_bucket, new_links, new_wide, Names};
use pubgrub::{
    error::PubGrubError,
    solver::resolve,
    solver::{Dependencies, DependencyProvider},
    type_aliases::{DependencyConstraints, SelectedDependencies},
    version_set::VersionSet as _,
};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use ron::ser::PrettyConfig;
use semver_pubgrub::{SemverCompatibility, SemverPubgrub};

mod hasher;
mod index_data;
mod names;

mod read_index;
use read_index::read_index;

#[cfg(test)]
use pubgrub::report::{DefaultStringReporter, Reporter};
#[cfg(test)]
use read_index::read_test_file;

const TIME_MAKE_FILE: f32 = 40.0;
const TIME_CUT_OFF: f32 = TIME_MAKE_FILE * 4.0;

#[derive(Clone)]
struct Index<'c> {
    crates: &'c HashMap<InternedString, BTreeMap<Arc<semver::Version>, index_data::Version>>,
    dependencies: RefCell<HashSet<(Arc<Names<'c>>, semver::Version)>>,
    start: Cell<Instant>,
    call_count: Cell<u64>,
}

impl<'c> Index<'c> {
    pub fn new(
        crates: &'c HashMap<InternedString, BTreeMap<Arc<semver::Version>, index_data::Version>>,
    ) -> Self {
        Self {
            crates,
            dependencies: Default::default(),
            start: Cell::new(Instant::now()),
            call_count: Cell::new(0),
        }
    }

    fn reset(&mut self) {
        self.dependencies.get_mut().clear();
        *self.start.get_mut() = Instant::now();
    }

    fn duration(&self) -> f32 {
        self.start.get().elapsed().as_secs_f32()
    }

    fn make_pubgrub_ron_file(&self) {
        let mut dependency_provider: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();
        let deps = self.dependencies.borrow().iter().cloned().collect_vec();

        let Some(name) = deps
            .iter()
            .find(|(name, _)| matches!(&**name, Names::Bucket(_, _, all) if *all))
        else {
            panic!("no root")
        };

        for (package, version) in &deps {
            if let Dependencies::Available(dependencies) =
                self.get_dependencies(package, version).unwrap()
            {
                *dependency_provider
                    .entry(package.clone())
                    .or_default()
                    .entry(version.clone())
                    .or_default() = dependencies.clone();
            }
        }

        let file_name = format!("out/pubgrub_ron/{}@{}.ron", name.0.crate_(), name.1);
        let file = File::create(&file_name).unwrap();
        ron::ser::to_writer_pretty(file, &dependency_provider, PrettyConfig::new()).unwrap();
    }

    fn make_index_ron_file(&self) {
        let mut deps = self.dependencies.borrow().iter().cloned().collect_vec();
        deps.sort_unstable();

        let name = deps
            .iter()
            .find(|(name, _)| matches!(&**name, Names::Bucket(_, _, all) if *all))
            .unwrap();

        let name_vers: BTreeSet<_> = deps
            .iter()
            .filter_map(|(package, version)| match &**package {
                Names::Bucket(n, _, _) | Names::BucketFeatures(n, _, _) => Some((n, version)),
                _ => None,
            })
            .collect();

        let out = name_vers
            .into_iter()
            .map(|(n, version)| self.crates[*n][version].clone())
            .collect_vec();

        let file_name = format!("out/index_ron/{}@{}.ron", name.0.crate_(), name.1);
        let file = File::create(&file_name).unwrap();
        ron::ser::to_writer_pretty(file, &out, PrettyConfig::new()).unwrap();
    }

    fn get_crate<Q>(&self, name: &Q) -> &'c BTreeMap<Arc<semver::Version>, index_data::Version>
    where
        Q: ?Sized,
        InternedString: std::borrow::Borrow<Q>,
        Q: Hash + Eq,
    {
        static EMPTY: BTreeMap<Arc<semver::Version>, index_data::Version> = BTreeMap::new();
        self.crates.get(name).unwrap_or(&EMPTY)
    }

    fn get_versions<Q>(&self, name: &Q) -> impl Iterator<Item = &'c semver::Version>
    where
        Q: ?Sized,
        InternedString: std::borrow::Borrow<Q>,
        Q: Hash + Eq,
    {
        self.get_crate(name).keys().map(|v| &**v).rev()
    }

    #[must_use]
    fn check(&self, root: Arc<Names>, pubmap: &SelectedDependencies<Self>) -> bool {
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

        let mut vertions: HashMap<(&str, SemverCompatibility), (semver::Version, BTreeSet<&str>)> =
            HashMap::new();
        // Identify the selected packages
        for (names, ver) in pubmap {
            if let Names::Bucket(name, cap, is_root) = &**names {
                if cap != &SemverCompatibility::from(ver) {
                    return false;
                }
                if *is_root {
                    continue;
                }
                let old_val = vertions.insert((*name, *cap), (ver.clone(), BTreeSet::new()));

                if old_val.is_some() {
                    return false;
                }
            }
        }
        // Identify the selected package features
        for (name, ver) in pubmap {
            if let Names::BucketFeatures(name, cap, feat) = &**name {
                if cap != &SemverCompatibility::from(ver) {
                    return false;
                }
                let old_val = vertions.get_mut(&(name, *cap)).unwrap();
                if &old_val.0 != ver {
                    return false;
                }
                let old_feat = old_val.1.insert(feat);
                if !old_feat {
                    return false;
                }
            }
        }

        let mut links: BTreeSet<_> = BTreeSet::new();
        for ((name, _), (ver, feats)) in vertions.iter() {
            let index_ver = &self.get_crate(*name)[ver];
            if index_ver.yanked {
                return false;
            }
            if let Some(link) = &index_ver.links {
                let old_link = links.insert(link.clone());
                if !old_link {
                    return false;
                }
            }

            for dep in &index_ver.deps {
                if dep.optional && !feats.contains(&*dep.name) {
                    continue;
                }
                if dep.kind == DependencyKind::Dev {
                    continue;
                }

                // Check for something that meets that dep
                let fulfilled =
                    vertions
                        .iter()
                        .find(|((other_name, _), (other_ver, other_feats))| {
                            **other_name == *dep.package_name
                                && dep.req.matches(other_ver)
                                && dep
                                    .features
                                    .iter()
                                    .all(|f| f.is_empty() || other_feats.contains(&**f))
                                && (!dep.default_features || other_feats.contains("default"))
                        });
                if fulfilled.is_none() {
                    return false;
                }
            }
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
    deps: &mut DependencyConstraints<Arc<Names<'c>>, SemverPubgrub>,
    n: Arc<Names<'c>>,
    r: SemverPubgrub,
) {
    deps.entry(n)
        .and_modify(|old_r| *old_r = old_r.intersection(&r))
        .or_insert(r);
}

impl<'c> DependencyProvider for Index<'c> {
    type P = Arc<Names<'c>>;

    type V = semver::Version;

    type VS = SemverPubgrub;

    type M = String;
    type Err = SomeError;
    fn choose_version(
        &self,
        package: &Arc<Names>,
        range: &SemverPubgrub,
    ) -> Result<Option<semver::Version>, Self::Err> {
        Ok(match &**package {
            Names::Links(_name) => {
                let Some((_, Bound::Included(v))) = range.bounding_range() else {
                    return Err(SomeError);
                };
                Some(v.clone())
            }

            Names::Wide(_, req, _, _) | Names::WideFeatures(_, req, _, _, _) => {
                // one version for each bucket that match req
                self.get_versions(&*package.crate_())
                    .filter(|v| req.matches(v))
                    .map(|v| SemverCompatibility::from(v))
                    .map(|v| v.canonical())
                    .find(|v| range.contains(v))
            }
            _ => self
                .get_versions(&*package.crate_())
                .find(|v| range.contains(v))
                .cloned(),
        })
    }

    type Priority = Reverse<usize>;

    fn prioritize(&self, package: &Arc<Names>, range: &SemverPubgrub) -> Self::Priority {
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
        package: &Arc<Names<'c>>,
        version: &semver::Version,
    ) -> Result<Dependencies<Self::P, Self::VS, Self::M>, Self::Err> {
        self.dependencies
            .borrow_mut()
            .insert((package.clone(), version.clone()));
        Ok(match &**package {
            Names::Bucket(name, _major, all_features) => {
                let index_ver = &self.get_crate(*name)[version];
                if index_ver.yanked {
                    return Ok(Dependencies::Unavailable("yanked".into()));
                }
                let mut deps = DependencyConstraints::default();
                if let Some(link) = &index_ver.links {
                    let index_unique_to_each_crate_version = {
                        let mut state = StableHasher::new();
                        name.hash(&mut state);
                        version.hash(&mut state);
                        state.finish()
                    };
                    let ver = semver::Version::new(index_unique_to_each_crate_version, 0, 0);
                    deps.insert(new_links(link), SemverPubgrub::singleton(ver));
                }
                for dep in &index_ver.deps {
                    if dep.kind == DependencyKind::Dev && !all_features {
                        continue;
                    }
                    if dep.optional && !all_features {
                        continue; // handled in Names::Features
                    }

                    let req_range = SemverPubgrub::from(&*dep.req);

                    let (cray, req_range) =
                        if let Some(compat) = req_range.only_one_compatibility_range() {
                            (
                                new_bucket(dep.package_name.as_str(), compat, false),
                                req_range,
                            )
                        } else {
                            (
                                new_wide(
                                    dep.package_name.as_str(),
                                    dep.req.deref(),
                                    package.crate_(),
                                    version.into(),
                                ),
                                SemverPubgrub::full(),
                            )
                        };

                    if &cray == package {
                        return Ok(Dependencies::Unavailable("self dep".into()));
                    }
                    deps_insert(&mut deps, cray.clone(), req_range.clone());

                    if dep.default_features {
                        deps_insert(&mut deps, cray.with_features("default"), req_range.clone());
                    }
                    for f in &dep.features {
                        deps_insert(&mut deps, cray.with_features(f), req_range.clone());
                    }
                }
                Dependencies::Available(deps)
            }
            Names::BucketFeatures(name, _major, feat) => {
                let index_ver = &self.get_crate(*name)[version];
                if index_ver.yanked {
                    return Ok(Dependencies::Unavailable("yanked".into()));
                }
                let mut compatibilitys: HashMap<_, Vec<(_, _)>> = HashMap::new();
                let mut deps = DependencyConstraints::default();
                deps.insert(
                    new_bucket(name, version.into(), false),
                    SemverPubgrub::singleton(version.clone()),
                );

                for dep in &index_ver.deps {
                    if dep.kind == DependencyKind::Dev {
                        continue;
                    }

                    if dep.optional && dep.name.as_str() == *feat {
                        let req_range = SemverPubgrub::from(&*dep.req);

                        let (cray, req_range) =
                            if let Some(compat) = req_range.only_one_compatibility_range() {
                                (
                                    new_bucket(dep.package_name.as_str(), compat, false),
                                    req_range,
                                )
                            } else {
                                (
                                    new_wide(
                                        dep.package_name.as_str(),
                                        dep.req.deref(),
                                        package.crate_(),
                                        version.into(),
                                    ),
                                    SemverPubgrub::full(),
                                )
                            };

                        if &cray == package {
                            return Ok(Dependencies::Unavailable("self dep".into()));
                        }
                        deps_insert(&mut deps, cray.clone(), req_range.clone());

                        if dep.default_features {
                            deps_insert(
                                &mut deps,
                                cray.with_features("default"),
                                req_range.clone(),
                            );
                        }
                        for f in &dep.features {
                            deps_insert(&mut deps, cray.with_features(f), req_range.clone());
                        }
                    }

                    compatibilitys
                        .entry(dep.name)
                        .or_default()
                        .push((dep.package_name, dep.req.deref()));
                }
                if deps.len() > 1 {
                    return Ok(Dependencies::Available(deps));
                }

                if let Some(vals) = index_ver.features.get(*feat) {
                    for val in vals {
                        if val.contains('/') {
                            let val: Vec<&str> = val
                                .trim_start_matches("dep:")
                                .split(['/', '?'])
                                .filter(|s| !s.is_empty())
                                .collect();
                            assert!(val.len() == 2);
                            for com in compatibilitys.get(val[0]).into_iter().flatten() {
                                let req_range = SemverPubgrub::from(&*com.1);

                                let (cray, req_range) = if let Some(compat) =
                                    req_range.only_one_compatibility_range()
                                {
                                    (new_bucket(com.0.as_str(), compat, false), req_range)
                                } else {
                                    (
                                        new_wide(
                                            com.0.as_str(),
                                            com.1,
                                            package.crate_(),
                                            version.into(),
                                        ),
                                        SemverPubgrub::full(),
                                    )
                                };
                                if &cray == package {
                                    return Ok(Dependencies::Unavailable("self dep".into()));
                                }
                                deps_insert(
                                    &mut deps,
                                    cray.with_features(val[1]),
                                    req_range.clone(),
                                );
                            }
                        } else {
                            deps_insert(
                                &mut deps,
                                package.with_features(val.trim_start_matches("dep:")),
                                SemverPubgrub::singleton(version.clone()),
                            );
                        }
                    }
                    return Ok(Dependencies::Available(deps));
                }
                if **feat == *"default" {
                    // if "default" was specified it would be in features
                    return Ok(Dependencies::Available(deps));
                }
                Dependencies::Unavailable("no matching feat".into())
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
                        new_bucket(name, compatibility, false).with_features(feat),
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
    ver: Arc<semver::Version>,
) -> OutPutSummery {
    let root = new_bucket(crt.as_str(), ver.deref().into(), true);
    dp.reset();
    let res = resolve(dp, root.clone(), ver.deref().clone());
    let duration = dp.duration();
    match res.as_ref() {
        Ok(map) => {
            if !dp.check(root, &map) {
                dp.make_index_ron_file();
                dp.make_pubgrub_ron_file();
                panic!("failed check");
            }
        }
        Err(PubGrubError::NoSolution(_derivation)) => {}
        Err(e) => {
            dp.make_index_ron_file();
            dp.make_pubgrub_ron_file();
            dbg!(e);
        }
    }
    if duration > TIME_MAKE_FILE {
        dp.make_index_ron_file();
        dp.make_pubgrub_ron_file();
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
    }
}

#[derive(serde::Serialize)]
struct OutPutSummery {
    name: InternedString,
    ver: Arc<semver::Version>,
    time: f32,
    succeeded: bool,
    pubgrub_deps: usize,
    deps: usize,
}

#[test]
fn files_pass_tests() {
    // Switch to https://docs.rs/snapbox/latest/snapbox/harness/index.html
    let mut faild: Vec<String> = vec![];
    for case in std::fs::read_dir("out/index_ron").unwrap() {
        let case = case.unwrap().path();
        let file_name = case.file_name().unwrap().to_string_lossy();
        let (name, rest) = file_name.split_once("@").unwrap();
        let ver = rest.strip_suffix(".ron").unwrap();
        dbg!((name, ver));
        let ver: semver::Version = ver.parse().unwrap();
        let data = std::fs::read_to_string(&case).unwrap();
        let start_time = std::time::Instant::now();
        let data: Vec<index_data::Version> = ron::de::from_str(&data).unwrap();
        let dp = Index::new(read_test_file(data));
        let root = new_bucket(name.into(), (&ver).into(), true);
        match resolve(&dp, root.clone(), ver.clone()) {
            Ok(map) => {
                if !dp.check(root.clone(), &map) {
                    dp.make_index_ron_file();
                    faild.push(root.to_string());
                }
                // dbg!(map);
            }

            Err(PubGrubError::NoSolution(derivation)) => {
                eprintln!("{}", DefaultStringReporter::report(&derivation));
            }
            Err(e) => {
                dp.make_index_ron_file();
                faild.push(root.to_string());
                dbg!(e);
            }
        }
        dp.make_pubgrub_ron_file();

        eprintln!(" in {}s", start_time.elapsed().as_secs());
    }
    assert_eq!(faild.as_slice(), &Vec::<String>::new());
}

fn main() {
    let create_filter = |name: &str| !name.contains("solana");
    println!("!!!!!!!!!! Excluding Solana Crates !!!!!!!!!!");

    let index =
        crates_index::GitIndex::with_path("index", "https://github.com/rust-lang/crates.io-index")
            .unwrap();
    let data = read_index(&index, create_filter);

    let (tx, rx) = mpsc::channel::<OutPutSummery>();

    let file_handle = spawn(|| {
        let mut out_file = csv::Writer::from_path("out.csv").unwrap();
        let start = Instant::now();
        let mut cpu_time = 0.0;
        for row in rx {
            cpu_time += row.time;
            out_file.serialize(row).unwrap();
        }
        out_file.flush().unwrap();
        (cpu_time, start.elapsed().as_secs_f32())
    });

    let template = "PubGrub: [Time: {elapsed}, Rate: {per_sec}, Remaining: {eta}] {wide_bar} {pos:>6}/{len:6}: {percent:>3}%";
    let style = ProgressBar::new(data.values().map(|v| v.len()).sum::<usize>() as u64)
        .with_style(ProgressStyle::with_template(template).unwrap())
        .with_finish(ProgressFinish::AndLeave);

    data.par_iter()
        .flat_map(|(c, v)| v.par_iter().map(|(v, _)| (c.clone(), v)))
        .progress_with(style)
        .map_with(Index::new(data), |dp, (crt, ver)| {
            process_carte_version(dp, crt, ver.clone())
        })
        .for_each_with(tx, |tx, csv_line| {
            let _ = tx.send(csv_line);
        });

    let (cpu_time, wall_time) = file_handle.join().unwrap();
    println!(
        "CPU time: {:.2}s == {:.2}min == {:.2}hr",
        cpu_time,
        cpu_time / 60.0,
        cpu_time / 3600.0
    );
    println!(
        "Wall time: {:.2}s == {:.2}min == {:.2}hr",
        wall_time,
        wall_time / 60.0,
        wall_time / 3600.0
    );
}
