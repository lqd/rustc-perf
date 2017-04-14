// Copyright 2016 The rustc-perf Project Developers. See the COPYRIGHT
// file at the top-level directory.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::collections::{HashMap, BTreeSet};
use std::cmp::{Ordering, max};
use std::fs::{self, File};
use std::path::PathBuf;
use std::io::Read;

use chrono::Duration;
use serde_json::{self, Value};

use errors::*;
use util::index_in;
use date::{OptionalDate, Date};

const WEEKS_IN_SUMMARY: usize = 12;

/// Contains the TestRun loaded from the file system.
/// Useful to avoid passing around each individual field, instead passing the
/// entire struct.
pub struct InputData {
    pub summary_rustc: Summary,
    pub summary_benchmarks: Summary,

    /// A set containing all crate names of the bootstrap kind.
    pub crate_list: BTreeSet<String>,

    /// A set containing all phase names, across all crates.
    pub phase_list: BTreeSet<String>,

    /// A set of test names; only non-bootstrap benchmarks are included. Test
    /// names are found from the file path, everything before the `--` is
    /// included.
    pub benchmarks: BTreeSet<String>,

    /// The last date that was seen while loading files. The DateTime variant is
    /// used here since the date may or may not contain a time. Since the
    /// timezone is not important, it isn't stored, hence the Naive variant.
    pub last_date: Date,

    /// Private due to access being exposed through by_kind method.
    /// Care must be taken to sort these after modifying them.
    data_rustc: Vec<TestRun>,
    data_benchmarks: Vec<TestRun>,
}

/// Allows storing `InputData` in Iron's persistent data structure.
impl ::iron::typemap::Key for InputData {
    type Value = InputData;
}

impl InputData {
    /// Initialize `InputData from the file system.
    pub fn from_fs(repo_loc: &str) -> Result<InputData> {
        let repo_loc = PathBuf::from(repo_loc);

        let mut skipped = 0;
        let mut merged = 0;

        let mut data_rustc = Vec::new();
        let mut data_benchmarks = Vec::new();

        // Read all files from repo_loc/processed
        let mut file_count = 0;
        for entry in fs::read_dir(repo_loc.join("processed"))? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                continue;
            }
            file_count += 1;

            let filename = entry.file_name();
            let filename = filename.to_str().unwrap();
            let mut file = File::open(entry.path())?;
            let mut file_contents = String::new();
            // Skip files whose size is 0.
            if file.read_to_string(&mut file_contents)? == 0 {
                warn!("Skipping empty file: {}", filename);
                skipped += 1;
                continue;
            }

            let contents: Value = match serde_json::from_str(&file_contents) {
                Ok(json) => json,
                Err(err) => {
                    error!("Failed to parse JSON for {}: {:?}", filename, err);
                    skipped += 1;
                    continue;
                }
            };
            if contents["times"].as_array().unwrap().is_empty() {
                skipped += 1;
                continue;
            }

            let commit = contents["header"]["commit"]
                .as_str()
                .unwrap()
                .to_string();
            let date = InputData::parse_from_header(contents["header"]["date"].as_str().unwrap())
                .or_else(|_| InputData::parse_from_filename(&filename))?;

            let test_name = InputData::testname_from_filename(&filename).to_string();

            let times = contents["times"].as_array().unwrap();

            let push_to = if &test_name == "rustc" {
                &mut data_rustc
            } else {
                &mut data_benchmarks
            };

            // If a run on the same commit occurs, replacing the crates in the
            // old run for this commit with the "current" run's crates.
            // TODO: Merge the two runs somehow, perhaps averaging each pass?
            if let Some(index) = push_to
                   .iter()
                   .position(|run: &TestRun| run.commit == commit) {
                let run = &mut push_to[index];

                let timings = make_times(times, test_name == "rustc");
                for (crate_name, crate_timings) in timings {
                    if run.by_crate.contains_key(&crate_name) {
                        warn!("Overwriting {} from {}, dated {:?}, commit {}",
                              crate_name,
                              filename,
                              date,
                              commit);
                    }

                    run.by_crate.insert(crate_name, crate_timings);
                }

                merged += 1;
            } else {
                push_to.push(TestRun::new(date, commit, times, &test_name));
            }
        }

        info!("{} total files", file_count);
        info!("{} skipped files", skipped);
        info!("{} merged times", merged);
        info!("{} bootstrap times", data_rustc.len());
        info!("{} benchmarks times", data_benchmarks.len());

        InputData::new(data_rustc, data_benchmarks)
    }

    pub fn new(mut data_rustc: Vec<TestRun>,
               mut data_benchmarks: Vec<TestRun>)
               -> Result<InputData> {
        let mut last_date = None;
        let mut phase_list = BTreeSet::new();
        let mut crate_list = BTreeSet::new();

        for run in data_rustc.iter().chain(&data_benchmarks) {
            if last_date.is_none() || last_date.as_ref().unwrap() < &run.date {
                last_date = Some(run.date);
            }

            for (crate_name, krate) in &run.by_crate {
                if run.kind == Kind::Rustc {
                    crate_list.insert(crate_name.to_string());
                }

                for phase_name in krate.keys() {
                    phase_list.insert(phase_name.to_string());
                }
            }
        }

        let last_date = last_date.expect("No dates found");

        data_rustc.sort();
        data_benchmarks.sort();

        let benchmarks = data_benchmarks
            .iter()
            .flat_map(|a| a.by_crate.keys())
            .cloned()
            .collect();

        // Post processing to generate the summary data.
        let summary_rustc = Summary::new(&data_rustc, last_date, &benchmarks);
        let summary_benchmarks = Summary::new(&data_benchmarks, last_date, &benchmarks);

        Ok(InputData {
               summary_rustc: summary_rustc,
               summary_benchmarks: summary_benchmarks,
               crate_list: crate_list,
               phase_list: phase_list,
               benchmarks: benchmarks,
               last_date: last_date,
               data_rustc: data_rustc,
               data_benchmarks: data_benchmarks,
           })
    }

    pub fn by_kind(&self, kind: Kind) -> &[TestRun] {
        match kind {
            Kind::Benchmarks => &self.data_benchmarks,
            Kind::Rustc => &self.data_rustc,
        }
    }

    /// Helper function to return a range of data given an optional start and end date.
    pub fn kinded_range(&self, kind: Kind, start: &OptionalDate, end: &OptionalDate) -> &[TestRun] {
        let kinded = self.by_kind(kind);
        let start = index_in(kinded, start.as_start(self.last_date));
        let end = index_in(kinded, end.as_end(self.last_date)) + 1;
        &kinded[start..end]
    }

    pub fn kinded_end_day(&self, kind: Kind, end: &OptionalDate) -> &TestRun {
        let kinded = self.by_kind(kind);
        &kinded[index_in(kinded, end.as_end(self.last_date))]
    }

    pub fn kinded_start_day(&self, kind: Kind, start: &OptionalDate) -> &TestRun {
        let kinded = self.by_kind(kind);
        &kinded[index_in(kinded, start.as_start(self.last_date))]
    }

    /// Parse date from JSON header in file contents.
    fn parse_from_header(date: &str) -> Result<Date> {
        Date::from_format(date, "%a %b %e %T %Y %z")
    }

    /// Parse date from filename.
    fn parse_from_filename(filename: &str) -> Result<Date> {
        let date_str = &filename[(filename.find("--").unwrap() + 2)..
                        filename.find(".json").unwrap()];

        match Date::from_format(date_str, "%Y-%m-%d-%H-%M-%S") {
            Ok(dt) => Ok(dt),
            Err(_) => Date::from_format(date_str, "%Y-%m-%d-%H-%M"),
        }
    }

    fn testname_from_filename(filename: &str) -> &str {
        &filename[0..filename.find("--").unwrap()]
    }
}

#[test]
fn check_header_date_parsing() {
    assert_eq!(InputData::parse_from_header("Sat Jan 2 13:58:57 2016 +0000").unwrap(),
               Date::ymd_hms(2016, 1, 2, 13, 58, 57));
    assert_eq!(InputData::parse_from_header("Mon Mar 14 08:32:45 2016 -0700").unwrap(),
               Date::ymd_hms(2016, 03, 14, 15, 32, 45));
    assert_eq!(InputData::parse_from_header("Mon Mar 14 08:32:45 2016 +0300").unwrap(),
               Date::ymd_hms(2016, 03, 14, 5, 32, 45));

    // Don't attempt to parse YYYY-MM-DD dates from the header, since more
    // accurate data will be found in the filename.
    assert!(InputData::parse_from_header("2016-05-05").is_err());
}

#[test]
fn check_filename_date_parsing() {
    assert_eq!(InputData::parse_from_filename("rustc--2016-08-06-21-59-30.json").unwrap(),
               Date::ymd_hms(2016, 8, 6, 21, 59, 30));
    assert_eq!(InputData::parse_from_filename("rustc--2016-08-06-00-00.json").unwrap(),
               Date::ymd_hms(2016, 8, 6, 0, 0, 0));
}

#[test]
fn check_testname_extraction() {
    assert_eq!(InputData::testname_from_filename("rustc--2016-08-06-21-59-30.json"),
               "rustc");
    assert_eq!(InputData::testname_from_filename("rust-encoding.0.2.32--2016-08-06-21-59-30.json"),
               "rust-encoding.0.2.32");
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    #[serde(rename="benchmarks")]
    Benchmarks,
    #[serde(rename="rustc")]
    Rustc,
}

#[test]
fn serialize_kind() {
    assert_eq!(serde_json::to_string(&Kind::Benchmarks).unwrap(),
               r#""benchmarks""#);
    assert_eq!(serde_json::from_str::<Kind>(r#""benchmarks""#).unwrap(),
               Kind::Benchmarks);
    assert_eq!(serde_json::to_string(&Kind::Rustc).unwrap(), r#""rustc""#);
    assert_eq!(serde_json::from_str::<Kind>(r#""rustc""#).unwrap(),
               Kind::Rustc);
}

/// The data loaded for a single date, and all associated crates.
#[derive(Debug)]
pub struct TestRun {
    pub date: Date,
    pub commit: String,
    pub kind: Kind,

    /// Map of crate names to a map of phases to timings per phase.
    pub by_crate: HashMap<String, HashMap<String, Timing>>,
}

impl PartialEq for TestRun {
    fn eq(&self, other: &TestRun) -> bool {
        self.commit == other.commit && self.date == other.date
    }
}

impl Eq for TestRun {}

impl PartialOrd for TestRun {
    fn partial_cmp(&self, other: &TestRun) -> Option<Ordering> {
        Some(<TestRun as Ord>::cmp(self, other))
    }
}

impl Ord for TestRun {
    fn cmp(&self, other: &TestRun) -> Ordering {
        self.date.cmp(&other.date)
    }
}

impl TestRun {
    fn new(date: Date, commit: String, times: &[Value], test_name: &str) -> TestRun {
        let is_rustc = test_name == "rustc";
        TestRun {
            date: date,
            commit: commit,
            kind: if is_rustc {
                Kind::Rustc
            } else {
                Kind::Benchmarks
            },
            by_crate: make_times(times, is_rustc),
        }
    }
}

/// Contains a single timing, associated with a phase (though the phase name
/// is not included in the timing).
#[derive(Debug, PartialEq, Copy, Clone, Serialize, Deserialize)]
pub struct Timing {
    pub percent: f64,
    pub time: f64,
    pub rss: Option<u64>,
}

impl Timing {
    fn new() -> Timing {
        Timing {
            percent: 0.0,
            time: 0.0,
            rss: Some(0),
        }
    }
}

/// Run through the timings for a date and construct the `by_crate` field of TestRun.
fn make_times(timings: &[Value], is_rustc: bool) -> HashMap<String, HashMap<String, Timing>> {
    let mut by_crate = HashMap::new();
    let mut totals = HashMap::new();

    for timing in timings {
        let mut times = HashMap::new();
        for (phase_name, phase) in timing["times"].as_object().unwrap() {
            times.insert(phase_name.to_string(),
                         Timing {
                             percent: phase["percent"].as_f64().unwrap(),
                             time: phase["time"].as_f64().unwrap(),
                             rss: timing
                                 .get("rss")
                                 .and_then(|rss| rss.get(phase_name))
                                 .and_then(|phase| phase.as_u64()),
                         });
        }

        let mut mem_values = Vec::new();
        if let Some(obj) = timing.get("rss").and_then(|rss| rss.as_object()) {
            for (_, value) in obj {
                mem_values.push(value.as_u64().unwrap());
            }
        }

        times.insert("total".into(),
                     Timing {
                         percent: 100.0,
                         time: timing["total"].as_f64().unwrap(),
                         rss: Some(mem_values.into_iter().max().unwrap_or(0)),
                     });

        for phase in times.keys() {
            let mut entry = totals
                .entry(phase.to_string())
                .or_insert_with(Timing::new);
            entry.time += times[phase].time;
            entry.rss = max(times[phase].rss, entry.rss);
        }

        by_crate.insert(timing["crate"].as_str().unwrap().to_string(), times);
    }

    if is_rustc {
        by_crate.insert("total".into(), totals);
    }
    // TODO: calculate percentages
    by_crate
}

#[derive(Debug)]
pub struct MedianRun {
    pub date: Date,

    /// Maps crate names to a map of phases to each phase's percent change
    /// from the previous date.
    pub by_crate: HashMap<String, HashMap<String, f64>>,
}

impl MedianRun {
    /// To summarize given an index, take the last 3 available runs and
    /// iterate over the phases, returning the median time for each phase
    fn new(data: &[TestRun], start_idx: usize, date: Date) -> MedianRun {
        let mut runs = Vec::new();

        for idx in 0..3 {
            if let Some(idx) = start_idx.checked_sub(idx) {
                if let Some(ref run) = data.get(idx) {
                    runs.push(&run.by_crate);
                }
            }
        }

        let mut by_crate_phase: HashMap<String, HashMap<String, Vec<f64>>> = HashMap::new();

        for run in runs {
            for (krate_name, krate) in run {
                let mut by_phase = by_crate_phase
                    .entry(krate_name.to_string())
                    .or_insert(HashMap::new());
                for (phase_name, phase) in krate {
                    by_phase
                        .entry(phase_name.to_string())
                        .or_insert(Vec::new())
                        .push(phase.time);
                }
            }
        }

        let mut median = MedianRun {
            date: date,
            by_crate: HashMap::new(),
        };

        for (crate_name, krate) in by_crate_phase {
            let mut crate_medians = HashMap::new();
            for (phase_name, phase) in krate {
                // Find the median value.
                let mut values = phase.clone();
                assert!(!values.is_empty(), "At least one value");
                values.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let median = values[values.len() / 2];
                crate_medians.insert(phase_name, median);
            }

            median.by_crate.insert(crate_name, crate_medians);
        }

        median
    }
}

pub struct Summary {
    pub total: MedianRun,
    pub summary: Vec<MedianRun>,
}

impl Summary {
    // Compute summary data. For each week, we find the last 3 weeks, and use
    // the median timing as the basis of the current week's summary.
    fn new(data: &[TestRun], last_date: Date, benchmarks: &BTreeSet<String>) -> Summary {
        // 12 week long mapping of crate names to by-phase percent changes with
        // the previous week.
        let mut weeks: Vec<MedianRun> = Vec::new();

        for i in 0..WEEKS_IN_SUMMARY {
            let start = last_date.start_of_week() - Duration::weeks(i as i64);
            let end = start + Duration::weeks(1);

            let mut start_idx = index_in(data, start);
            let end_idx = index_in(data, end);

            if start_idx == end_idx {
                assert!(start_idx > 0, "Handling dawn of data not yet implemented");
                start_idx -= 1;
            }

            weeks.push(Summary::compare_weeks(&MedianRun::new(data, start_idx, start),
                                              &MedianRun::new(data, end_idx, end)));
        }

        assert_eq!(weeks.len(), 12);

        let totals = {
            let start = last_date.start_of_week() - Duration::weeks(13);
            let end = last_date + Duration::weeks(1);

            let start_idx = index_in(data, start);
            let end_idx = index_in(data, end);

            Summary::compare_weeks(&MedianRun::new(data, start_idx, start),
                                   &MedianRun::new(data, end_idx, end))
        };

        for week in &mut weeks {
            for crate_name in benchmarks {
                if !week.by_crate.contains_key(crate_name) {
                    week.by_crate
                        .insert(crate_name.to_string(), HashMap::new());
                }
            }
        }

        Summary {
            total: totals,
            summary: weeks,
        }
    }

    /// Compute the percent change for a given number from the week N-1 to
    /// week N, where week N-1 is the previous and week N is the current
    /// week.
    fn get_percent_change(previous: f64, current: f64) -> f64 {
        ((current - previous) / previous) * 100.0
    }

    fn compare_weeks(week0: &MedianRun, week1: &MedianRun) -> MedianRun {
        let mut current_week = HashMap::new();
        for crate_name in week0.by_crate.keys() {
            if !week1.by_crate.contains_key(crate_name) {
                continue;
            }

            let mut current_crate = HashMap::new();
            for phase in week0.by_crate[crate_name].keys() {
                if !week1.by_crate[crate_name].contains_key(phase) {
                    continue;
                }

                // TODO: Some form of warning is a good idea?
                // If this is triggered for both the 0th overall week and
                // the last week, then that phase won't be recorded in
                // totals no matter what happens in between.
                if week0.by_crate[crate_name][phase] > 0.0 &&
                   week1.by_crate[crate_name][phase] > 0.0 {
                    current_crate.insert(phase.to_string(),
                                         Summary::get_percent_change(week0.by_crate[crate_name]
                                                                         [phase],
                                                                     week1.by_crate[crate_name]
                                                                         [phase]));
                }
            }
            current_week.insert(crate_name.to_string(), current_crate);
        }
        MedianRun {
            date: week1.date,
            by_crate: current_week,
        }
    }
}
