use std::{sync::mpsc, thread::spawn, time::Instant};

use cargo::core::Summary;
use indicatif::{ParallelProgressIterator as _, ProgressBar, ProgressFinish, ProgressStyle};
use rayon::iter::{IntoParallelRefIterator as _, ParallelIterator as _};
use read_index::read_index;

use benchmark_from_crates::{index_data, process_carte_version, read_index, Index, OutPutSummery};

fn main() {
    let create_filter = |name: &str| !name.contains("solana");
    println!("!!!!!!!!!! Excluding Solana Crates !!!!!!!!!!");
    let version_filter =
        |version: &index_data::Version| !version.yanked && Summary::try_from(version).is_ok();
    println!("!!!!!!!!!! Excluding Yanked and Non Cargo Summary Versions !!!!!!!!!!");

    let index =
        crates_index::GitIndex::with_path("index", "https://github.com/rust-lang/crates.io-index")
            .unwrap();
    let (data, cargo_crates) = read_index(&index, create_filter, version_filter);

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

    let template = "PubGrub: [Time: {elapsed}, Rate: {per_sec}, Remaining: {eta}] {wide_bar} {pos:>6}/{len:6}: {percent:>3}%";
    let style = ProgressBar::new(data.values().map(|v| v.len()).sum::<usize>() as u64)
        .with_style(ProgressStyle::with_template(template).unwrap())
        .with_finish(ProgressFinish::AndLeave);

    data.par_iter()
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
