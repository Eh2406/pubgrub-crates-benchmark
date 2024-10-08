use std::{sync::mpsc, thread::spawn, time::Instant};

use benchmark_from_crates::{
    index_data, process_crate_version, read_index::read_index, Index, Mode, OutputSummary,
};
use clap::Parser;
use rayon::iter::{IntoParallelRefIterator as _, ParallelIterator as _};
use std::time::Duration;

use ratatui::{
    buffer::Buffer,
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    layout::{Alignment, Constraint, Layout, Rect},
    widgets::{block::Title, Block, Gauge, Padding, Widget},
};

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

    let crate_vertions = data.values().map(|v| v.len() as u64).sum::<u64>();

    let file_handle =
        spawn(move || {
            let mut out_file = csv::Writer::from_path("out.csv").unwrap();
            // "Rate: {per_sec}, Remaining: {eta}";

            #[derive(Debug, Clone, Copy)]
            struct App {
                progress: u64,
                crate_vertions: u64,
                start: Instant,
                pub_cpu_time: f32,
                cargo_cpu_time: f32,
                cargo_pub_lock_cpu_time: f32,
                pub_cargo_lock_cpu_time: f32,
            }

            impl Widget for &App {
                fn render(self, area: Rect, buf: &mut Buffer) {
                    let layout = Layout::vertical([Constraint::Length(4), Constraint::Min(1)]);
                    let [progress_area, gauge_area] = layout.areas(area);
                    let progress = self.progress;
                    let crate_vertions = self.crate_vertions;

                    let percent = self.progress as f64 / crate_vertions as f64;

                    Gauge::default()
                        .block(Block::new().title(
                            Title::from("Percentage Processed").alignment(Alignment::Center),
                        ))
                        .ratio(percent)
                        .label(format!("{percent:.1}% = {progress}/{crate_vertions}"))
                        .use_unicode(true)
                        .render(progress_area, buf);

                    let layout = Layout::horizontal([Constraint::Length(55), Constraint::Min(1)]);
                    let [name_area, gauge_area] = layout.areas(gauge_area);

                    let layout = Layout::vertical([Constraint::Ratio(1, 5); 5]);
                    let [pub_area, cargo_area, cargo_lock_area, pub_lock_area, wall_area] =
                        layout.areas(name_area);

                    let mut p = |n: &str, t: f32, area: Rect, all_time: f32| {
                        Gauge::default()
                            .block(Block::new().padding(Padding::proportional(1)))
                            .ratio(if all_time > 0.0 {
                                t as f64 / all_time as f64
                            } else {
                                0.0
                            })
                            .label(if t > 0.0 {
                                format!("{n} time: {:>8.2}s == {:>6.2}min", t, t / 60.0)
                            } else {
                                format!("{n} time: skipped")
                            })
                            .use_unicode(true)
                            .render(area, buf);
                    };

                    let all_cpu_time = [
                        self.pub_cpu_time,
                        self.cargo_cpu_time,
                        self.cargo_pub_lock_cpu_time,
                        self.pub_cargo_lock_cpu_time,
                    ]
                    .into_iter()
                    .max_by(f32::total_cmp)
                    .unwrap();

                    p("Pub CPU", self.pub_cpu_time, pub_area, all_cpu_time);
                    p("Cargo CPU", self.cargo_cpu_time, cargo_area, all_cpu_time);
                    p(
                        "Cargo check lock CPU",
                        self.cargo_pub_lock_cpu_time,
                        cargo_lock_area,
                        all_cpu_time,
                    );
                    p(
                        "Pub check lock CPU",
                        self.pub_cargo_lock_cpu_time,
                        pub_lock_area,
                        all_cpu_time,
                    );
                    p("Wall", self.start.elapsed().as_secs_f32(), wall_area, 0.0);
                }
            }

            let mut terminal = ratatui::init();
            let mut app = App {
                progress: 0,
                crate_vertions,
                start: Instant::now(),
                pub_cpu_time: 0.0,
                cargo_cpu_time: 0.0,
                cargo_pub_lock_cpu_time: 0.0,
                pub_cargo_lock_cpu_time: 0.0,
            };
            'out: loop {
                terminal
                    .draw(|frame| frame.render_widget(&app, frame.area()))
                    .unwrap();
                if event::poll(Duration::from_secs_f32(0.05)).unwrap() {
                    if let Event::Key(key) = event::read().unwrap() {
                        if key.kind == KeyEventKind::Press {
                            if let KeyCode::Char('q') | KeyCode::Esc = key.code {
                                break;
                            }
                        }
                    }
                }
                loop {
                    match rx.try_recv() {
                        Ok(row) => {
                            app.progress += 1;
                            app.pub_cpu_time += row.time;
                            app.cargo_cpu_time += row.cargo_time;
                            app.cargo_pub_lock_cpu_time += row.cargo_check_pub_lock_time;
                            app.pub_cargo_lock_cpu_time += row.pub_check_cargo_lock_time;
                            out_file.serialize(row).unwrap();
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => break 'out,
                    }
                }
            }
            ratatui::restore();
            out_file.flush().unwrap();
        });

    data.par_iter()
        .flat_map(|(c, v)| v.par_iter().map(|(v, _)| (c.clone(), v)))
        .map(|(crt, ver)| {
            process_crate_version(&mut Index::new(&data), crt, ver.clone(), args.mode)
        })
        .for_each(move |csv_line| {
            let _ = tx.send(csv_line);
        });

    file_handle.join().unwrap();
}
