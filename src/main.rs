use std::{sync::mpsc, thread::spawn, time::Instant};

use benchmark_from_crates::{
    index_data, process_crate_version, read_index::read_index, Index, Mode, OutputSummary,
};
use clap::Parser;
use indicatif::{ParallelProgressIterator as _, ProgressBar, ProgressFinish, ProgressStyle};
use rayon::iter::{IntoParallelRefIterator as _, ParallelIterator as _};

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
struct Args {
    /// Dont filter out core elements of the Solana ecosystem
    #[clap(long)]
    with_solana: bool,

    #[arg(long, short, value_enum, default_value_t = Mode::All)]
    mode: Mode,

    /// Sets the number of threads to be used in the rayon threadpool.
    #[clap(long, short, default_value_t = 0)]
    threads: usize,
}

fn main() {
    let args = Args::parse();
    rayon::ThreadPoolBuilder::new()
        .num_threads(args.threads)
        .build_global()
        .unwrap();

    println!(
        "Running in mode {:?} on {} rayon threads.",
        &args.mode,
        rayon::current_num_threads()
    );
    let create_filter = if args.with_solana {
        |_name: &str| true
    } else {
        println!("!!!!!!!!!! Excluding Solana Crates !!!!!!!!!!");
        |name: &str| !name.contains("solana")
    };
    let version_filter = |version: &index_data::Version| !version.yanked;
    println!("!!!!!!!!!! Excluding Yanked !!!!!!!!!!");

    let index =
        crates_index::GitIndex::with_path("index", "https://github.com/rust-lang/crates.io-index")
            .unwrap();
    let data = read_index(&index, create_filter, version_filter);

    let (tx, rx) = mpsc::channel::<OutputSummary>();

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
            process_crate_version(&mut Index::new(&data), crt, ver.clone(), args.mode)
        })
        .for_each(move |csv_line| {
            let _ = tx.send(csv_line);
        });

    let (pub_cpu_time, cargo_cpu_time, cargo_pub_lock_cpu_time, pub_cargo_lock_cpu_time, wall_time) =
        file_handle.join().unwrap();
    println!("!!!!!!!!!! Timings !!!!!!!!!!");
    let p = |n: &str, t: f32| {
        if t > 0.0 {
            println!("{n:>20} time: {:>8.2}s == {:>6.2}min", t, t / 60.0)
        } else {
            println!("{n:>20} time: skipped")
        }
    };
    p("Pub CPU", pub_cpu_time);
    p("Cargo CPU", cargo_cpu_time);
    p("Cargo check lock CPU", cargo_pub_lock_cpu_time);
    p("Pub check lock CPU", pub_cargo_lock_cpu_time);
    p("Wall", wall_time);
}
