use std::path::Path;

use rayon::iter::IndexedParallelIterator as _;
use rayon::iter::IntoParallelIterator as _;
use rayon::iter::ParallelIterator as _;

use super::*;

fn case_from_file_name(file_name: &str) -> (&str, semver::Version) {
    let (name, rest) = file_name.split_once("@").unwrap();
    let ver = rest.strip_suffix(".ron").unwrap();
    (name, ver.parse().unwrap())
}

fn crates_data_from_file<P: AsRef<Path>>(path: P) -> IndexMapLookup {
    let data = std::fs::read_to_string(path).unwrap();
    let data: Vec<index_data::Version> = ron::de::from_str(&data).unwrap();
    read_test_file(data)
}

#[must_use]
fn check<'c>(dp: &mut Index<'c>, root: Rc<Names<'c>>, ver: &semver::Version) -> bool {
    dp.reset();
    let res = resolve(dp, root.clone(), ver.clone());
    match res.as_ref() {
        Ok(map) => {
            if !dp.check(root.clone(), &map) {
                return false;
            }
        }

        Err(PubGrubError::NoSolution(_derivation)) => {
            // eprintln!("{}", DefaultStringReporter::report(&derivation));
        }
        Err(_e) => {
            return false;
        }
    }
    true
}

#[test]
fn serde_round_trip() {
    // Switch to https://docs.rs/snapbox/latest/snapbox/harness/index.html
    let mut faild: Vec<_> = vec![];
    for case in std::fs::read_dir("out/index_ron").unwrap() {
        let case = case.unwrap().path();
        let file_name = case.file_name().unwrap().to_string_lossy().to_string();
        eprintln!("Running: {file_name}");
        let raw_data = std::fs::read_to_string(&case).unwrap();
        let data: Vec<index_data::Version> = ron::de::from_str(&raw_data).unwrap();
        let raw_data_2 = ron::ser::to_string_pretty(&data, PrettyConfig::new()).unwrap();
        let crates_1 = read_test_file(data);
        let data_2: Vec<index_data::Version> = ron::de::from_str(&raw_data_2).unwrap();
        let crates_2 = read_test_file(data_2);
        if crates_1 != crates_2 {
            faild.push(file_name);
        } else if raw_data != raw_data_2 {
            let mut file = File::create(&case).unwrap();
            file.write_all(raw_data_2.as_bytes()).unwrap();
            file.flush().unwrap();
        }
    }
    assert_eq!(faild.as_slice(), &Vec::<String>::new());
}

#[test]
fn minimize_named_from_files() {
    rayon::ThreadPoolBuilder::new()
        .num_threads(100)
        .build_global()
        .unwrap();

    let name = "solana-archiver-lib";
    let name_old = "solana-archiver-lib-old";
    let ver: semver::Version = "1.1.13".parse().unwrap();

    let mut data: Vec<index_data::Version> = ron::de::from_str(
        &std::fs::read_to_string("out/index_ron/solana-archiver-lib@1.1.13.ron").unwrap(),
    )
    .unwrap();
    let mut data_old: Vec<index_data::Version> = ron::de::from_str(
        &std::fs::read_to_string("out/index_ron/solana-archiver-lib-old@1.1.13.ron").unwrap(),
    )
    .unwrap();

    let mut max_iter = 1000;
    'data: loop {
        if max_iter % 9 == 0 {
            println!("{max_iter}: cut crate");
        } else if max_iter % 2 == 0 {
            println!("{max_iter}: cut deps #{}", (max_iter / 2) % 5);
        } else {
            println!("{max_iter}: cut fet #{}", (max_iter / 2) % 5);
        }
        let used: HashSet<_> = data_old
            .iter()
            .filter_map(|v| {
                if max_iter % 9 == 0 {
                    return Some(v);
                }
                let out = if max_iter % 2 == 0 {
                    v.clone().without_a_dep((max_iter / 2) % 5)?
                } else {
                    v.clone().without_a_dep((max_iter / 2) % 5)?
                };
                TryInto::<Summary>::try_into(&out).ok()?;
                Some(v)
            })
            .map(|v| (v.name, v.vers))
            .filter(|(n, _)| !n.starts_with(name))
            .collect();
        let used: Vec<_> = used.into_iter().collect();
        if used.is_empty() {
            max_iter -= 1;
            continue;
        }
        dbg!(data.len(), data_old.len(), used.len());
        let out = (0..used.len())
            .into_par_iter()
            .by_exponential_blocks()
            .map(|idx_to_remove| {
                let root = new_bucket(name, (&ver).into(), true);
                let root_old = new_bucket(name_old, (&ver).into(), true);

                let fillter = |d: &index_data::Version| {
                    if used[..=idx_to_remove].iter().any(|(n, v)| {
                        (d.name == *n || (d.name.starts_with(name) && n.starts_with(name)))
                            && d.vers == *v
                    }) {
                        if max_iter % 9 == 0 {
                            return None;
                        }
                        let out = if max_iter % 2 == 0 {
                            d.clone().without_a_dep((max_iter / 2) % 5).unwrap()
                        } else {
                            d.clone().without_a_dep((max_iter / 2) % 5).unwrap()
                        };
                        TryInto::<Summary>::try_into(&out).unwrap();
                        Some(out)
                    } else {
                        Some(d.clone())
                    }
                };
                let crates = read_test_file(data.iter().filter_map(fillter));
                let crates_old = read_test_file(data_old.iter().filter_map(fillter));

                let mut dp = Index::new(&crates);
                let mut dp_old = Index::new(&crates_old);

                let start_time = std::time::Instant::now();
                _ = check(&mut dp, root.clone(), &ver);
                let time = start_time.elapsed().as_secs_f32();

                let start_time = std::time::Instant::now();
                _ = check(&mut dp_old, root_old.clone(), &ver);
                let time_old = start_time.elapsed().as_secs_f32();

                let data_new = dp.make_index_ron_data();
                let data_old_new = dp_old.make_index_ron_data();

                let len: f32 = data_new
                    .iter()
                    .map(|v| (1 + v.features_raw.len() + v.deps.len()) as f32)
                    .sum();
                let len_old: f32 = data_old_new
                    .iter()
                    .map(|v| (1 + v.features_raw.len() + v.deps.len()) as f32)
                    .sum();

                let main_slope = time / (len + 1.0);
                let delts_slope = (time_old - time) / ((len_old - len) as f32 + 1.0);

                (
                    time,
                    time_old,
                    data_new,
                    data_old_new,
                    main_slope,
                    delts_slope,
                )
            })
            .find_any(|(.., main_slope, delts_slope)| {
                main_slope > &1.7e-5 && delts_slope > &(main_slope * 8.0)
            });

        if let Some((time, time_old, data_new, data_old_new, main_slope, delts_slope)) = out {
            data = data_new;
            data_old = data_old_new;

            let file_name = format!("out/index_ron/{}@{}.ron", name, ver);
            let mut file = BufWriter::new(File::create(&file_name).unwrap());
            ron::ser::to_writer_pretty(&mut file, &data, PrettyConfig::new()).unwrap();
            file.flush().unwrap();

            let file_name = format!("out/index_ron/{}@{}.ron", name_old, ver);
            let mut file = BufWriter::new(File::create(&file_name).unwrap());
            ron::ser::to_writer_pretty(&mut file, &data_old, PrettyConfig::new()).unwrap();
            file.flush().unwrap();

            dbg!(time, time_old, main_slope, delts_slope / main_slope);
            continue 'data;
        } else {
            max_iter -= 1;
            if max_iter == 0 {
                break;
            }
        }
    }
}

#[test]
fn named_from_files_pass_tests() {
    // Switch to https://docs.rs/snapbox/latest/snapbox/harness/index.html
    let mut faild: Vec<_> = vec![];
    for case in std::fs::read_dir("out/index_ron").unwrap() {
        let case = case.unwrap().path();
        let file_name = case.file_name().unwrap().to_string_lossy();
        let (name, ver) = case_from_file_name(&file_name);
        eprintln!("Running: {name} @ {ver}");
        let start_time = std::time::Instant::now();
        let crates = crates_data_from_file(&case);
        let mut dp = Index::new(&crates);
        let root = new_bucket(name, (&ver).into(), true);
        if !check(&mut dp, root, &ver) {
            dp.make_index_ron_file();
            faild.push(file_name.to_string());
        };
        dp.make_index_ron_file();
        dp.make_pubgrub_ron_file();
        eprintln!(" in {}s", start_time.elapsed().as_secs_f32());
    }
    assert_eq!(faild.as_slice(), &Vec::<String>::new());
}

#[test]
fn named_from_files_pass_without_vers() {
    // Switch to https://docs.rs/snapbox/latest/snapbox/harness/index.html
    for case in std::fs::read_dir("out/index_ron").unwrap() {
        let case = case.unwrap().path();
        let file_name = case.file_name().unwrap().to_string_lossy();
        let (name, ver) = case_from_file_name(&file_name);
        eprintln!("Running: {name} @ {ver}");
        let root = new_bucket(name, (&ver).into(), true);
        let start_time = std::time::Instant::now();
        let data = std::fs::read_to_string(&case).unwrap();
        let mut data: Vec<index_data::Version> = ron::de::from_str(&data).unwrap();
        let mut offset = 0;
        'data: loop {
            for i in 0..data.len() {
                let len = data.len();
                let i = (i + offset) % len;
                let removed = data.swap_remove(i);
                let crates = read_test_file(data.iter().cloned());
                let mut dp = Index::new(&crates);
                if !check(&mut dp, root.clone(), &ver) {
                    data = dp.make_index_ron_data();
                    offset = i;
                    println!("Failed on removing {i} of {len}");
                    continue 'data;
                };
                if let Some(without_features) = removed.clone().without_features() {
                    data.push(without_features);
                    let crates = read_test_file(data.iter().cloned());
                    let mut dp = Index::new(&crates);
                    if !check(&mut dp, root.clone(), &ver) {
                        data = dp.make_index_ron_data();
                        offset = i;
                        println!("Failed on without_features {i} of {len}");
                        continue 'data;
                    };
                    data.pop();
                }
                for f in 0..removed.features.len() {
                    if let Some(without_features) = removed.clone().without_a_feature(f) {
                        if TryInto::<Summary>::try_into(&without_features).is_ok() {
                            data.push(without_features);
                            let crates = read_test_file(data.iter().cloned());
                            let mut dp = Index::new(&crates);
                            if !check(&mut dp, root.clone(), &ver) {
                                data = dp.make_index_ron_data();
                                offset = i;
                                println!("Failed on without_a_feature({f}) {i} of {len}");
                                continue 'data;
                            };
                            data.pop();
                        }
                    }
                }
                if let Some(without_deps) = removed.clone().without_deps() {
                    if TryInto::<Summary>::try_into(&without_deps).is_ok() {
                        data.push(without_deps);
                        let crates = read_test_file(data.iter().cloned());
                        let mut dp = Index::new(&crates);
                        if !check(&mut dp, root.clone(), &ver) {
                            data = dp.make_index_ron_data();
                            offset = i;
                            println!("Failed on without_deps {i} of {len}");
                            continue 'data;
                        };
                        data.pop();
                    }
                }
                for d in 0..removed.deps.len() {
                    if let Some(without_deps) = removed.clone().without_a_dep(d) {
                        if TryInto::<Summary>::try_into(&without_deps).is_ok() {
                            data.push(without_deps);
                            let crates = read_test_file(data.iter().cloned());
                            let mut dp = Index::new(&crates);
                            if !check(&mut dp, root.clone(), &ver) {
                                data = dp.make_index_ron_data();
                                offset = i;
                                println!("Failed on without_a_dep({d}) {i} of {len}");
                                continue 'data;
                            };
                            data.pop();
                        }
                    }
                }
            }
            break;
        }
        let crates = read_test_file(data);
        let mut dp = Index::new(&crates);
        if !check(&mut dp, root, &ver) {
            dp.make_index_ron_file();
        };

        eprintln!(" in {}s", start_time.elapsed().as_secs_f32());
    }
}

#[test]
fn all_vers_in_files_pass_tests() {
    // Switch to https://docs.rs/snapbox/latest/snapbox/harness/index.html
    let mut faild: Vec<_> = vec![];
    for case in std::fs::read_dir("out/index_ron").unwrap() {
        let case = case.unwrap().path();
        let file_name = case.file_name().unwrap().to_string_lossy();
        eprintln!("Running: {file_name}");
        let start_time = std::time::Instant::now();
        let crates = crates_data_from_file(&case);
        let mut dp = Index::new(&crates);
        for (name, vers) in &crates {
            for ver in vers.keys() {
                let root = new_bucket(&name, ver.into(), true);
                if !check(&mut dp, root, ver) {
                    dp.make_index_ron_file();
                    faild.push(format!("{file_name}:{name}@{ver}"));
                };
            }
        }
        eprintln!(" in {}s", start_time.elapsed().as_secs_f32());
    }
    assert_eq!(faild.as_slice(), &Vec::<String>::new());
}
