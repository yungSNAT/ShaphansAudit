#![no_main]
//! Any bytes as a Claude and a Codex transcript, plain and compressed, through every
//! detector and both report formats.

use std::path::PathBuf;
use std::rc::Rc;

use chrono::DateTime;
use libfuzzer_sys::fuzz_target;
use shaphansaudit::cli::Command;
use shaphansaudit::detectors::build;
use shaphansaudit::programs::Programs;
use shaphansaudit::report::{self, Meta, Opts};
use shaphansaudit::scan::{Agent, scan};

thread_local! {
    static PROGRAMS: Rc<Programs> = Rc::new(Programs::from_env());
    /// A folder of its own for this process's transcripts.
    static DIR: PathBuf = {
        let dir = std::env::temp_dir().join(format!("shaphan-fuzz-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("fuzz folder");
        dir
    };
}

fuzz_target!(|data: &[u8]| {
    let dir = DIR.with(Clone::clone);
    for name in ["s.jsonl", "s.jsonl.zst"] {
        std::fs::write(dir.join(name), data).expect("transcript");
    }
    let mut dets = PROGRAMS.with(|p| build(Command::All, p));
    let roots = [(Agent::Claude, dir.clone()), (Agent::Codex, dir)];
    let since = DateTime::UNIX_EPOCH;
    let stats = scan(&roots, since, &mut dets);
    // Each root lists both files, the compressed one counted as read or skipped.
    for st in &stats {
        assert_eq!(st.files + st.compressed, 2, "{st:?}");
    }
    let opts = Opts {
        limit: 20,
        paths: true,
        home: None,
    };
    let sections: Vec<_> = dets.iter().map(|d| d.section(&opts)).collect();
    let meta = Meta {
        since,
        roots: &stats,
        skipped_roots: &[],
        home: None,
    };
    let _ = report::text(&meta, &sections);
    let json = report::json(&meta, &sections);
    assert!(
        serde_json::from_str::<serde_json::Value>(&json).is_ok(),
        "{json}"
    );
});
