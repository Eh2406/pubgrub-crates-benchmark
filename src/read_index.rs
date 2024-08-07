use std::collections::{BTreeMap, HashMap};

use cargo::{core::Summary, util::interning::InternedString};
use crates_index::GitIndex;
use rayon::iter::ParallelIterator;

use crate::index_data;

pub fn read_index(
    index: &GitIndex,
    create_filter: impl Fn(&str) -> bool + Sync + 'static,
    version_filter: impl Fn(&index_data::Version) -> bool + Sync + 'static,
) -> (
    HashMap<InternedString, BTreeMap<semver::Version, index_data::Version>>,
    HashMap<InternedString, BTreeMap<semver::Version, Summary>>,
) {
    println!("Start reading index");
    let crates: HashMap<InternedString, BTreeMap<semver::Version, index_data::Version>> = index
        .crates_parallel()
        .map(|c| c.unwrap())
        .filter(|crt| create_filter(crt.name()))
        .map(|crt| {
            let name: InternedString = crt.name().into();
            let ver_lookup = crt
                .versions()
                .iter()
                .filter_map(|v| TryInto::<index_data::Version>::try_into(v).ok())
                .filter(|v| version_filter(v))
                .map(|v| ((*v.vers).clone(), v))
                .collect();
            (name, ver_lookup)
        })
        .collect();
    let cargo_deps = crates
        .iter()
        .map(|(n, vs)| {
            (
                n.clone(),
                vs.iter()
                    .map(|(v, d)| (v.clone(), d.try_into().unwrap()))
                    .collect(),
            )
        })
        .collect();
    println!("Done reading index");
    (crates, cargo_deps)
}

#[cfg(test)]
pub fn read_test_file(
    iter: impl IntoIterator<Item = index_data::Version>,
) -> (
    HashMap<InternedString, BTreeMap<semver::Version, index_data::Version>>,
    HashMap<InternedString, BTreeMap<semver::Version, Summary>>,
) {
    let mut deps: HashMap<InternedString, BTreeMap<semver::Version, index_data::Version>> =
        HashMap::new();

    for v in iter {
        deps.entry(v.name.clone())
            .or_default()
            .insert((*v.vers).clone(), v);
    }

    let cargo_deps = deps
        .iter()
        .map(|(n, vs)| {
            (
                n.clone(),
                vs.iter()
                    .map(|(v, d)| (v.clone(), d.try_into().unwrap()))
                    .collect(),
            )
        })
        .collect();

    (deps, cargo_deps)
}
