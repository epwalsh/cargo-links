use std::sync::mpsc::channel;
use std::sync::Arc;

use exitfailure::ExitFailure;
use globset::{Glob, GlobSetBuilder};
use grep_matcher::{Captures, Matcher};
use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::Searcher;
use ignore::Walk;
use structopt::StructOpt;
use threadpool::ThreadPool;

mod link;
mod log;

use link::{Link, LinkStatus};
use log::Logger;

#[derive(Debug, StructOpt)]
#[structopt(
    name = "cargo-links",
    about = "Check the links in your crate's documentation.",
    raw(setting = "structopt::clap::AppSettings::ColoredHelp")
)]
struct Opt {
    /// Set the number of threads.
    #[structopt(short = "c", long = "concurrency", default_value = "10")]
    concurrency: usize,

    /// Verbose mode (-v, -vv, -vvv, etc).
    #[structopt(short = "v", long = "verbose", parse(from_occurrences))]
    verbose: usize,

    /// Don't log in color.
    #[structopt(long = "no-color")]
    no_color: bool,
}

fn main() -> Result<(), ExitFailure> {
    let opt = Opt::from_args();
    let mut logger = Logger::default(opt.verbose, !opt.no_color);
    logger.debug(&format!("{:?}", opt)[..])?;

    // This is the regular expression we use to find links.
    let matcher = RegexMatcher::new(r"\[[^\[\]]+\]\(([^\(\)]+)\)").unwrap();

    let mut searcher = Searcher::new();

    // Initialize thread pool and channel.
    let pool = ThreadPool::new(opt.concurrency);
    let (tx, rx) = channel();

    // We'll use a single HTTP client across threads.
    let http_client = Arc::new(reqwest::Client::new());

    // We iterator through all rust and markdown files not included in your .gitignore.
    let mut glob_builder = GlobSetBuilder::new();
    glob_builder.add(Glob::new("*.rs")?);
    glob_builder.add(Glob::new("*.md")?);
    let glob_set = glob_builder.build()?;
    let file_iter = Walk::new("./")
        .filter_map(Result::ok)
        .filter(|x| match x.file_type() {
            Some(file_type) => file_type.is_file(),
            None => false,
        })
        .map(|x| x.into_path())
        .filter(|p| glob_set.is_match(p));

    let mut n_links = 0;
    for path in file_iter {
        let path_str = path.to_str();
        if let None = path_str {
            // File path is not valid unicode, just skip.
            logger.warn(
                &format!(
                    "Filename is not valid unicode, skipping: {}",
                    path.display()
                )[..],
            )?;
            continue;
        }
        let path_str = path_str.unwrap();

        logger.debug(&format!("Searching {}", path.display())[..])?;

        searcher.search_path(
            &matcher,
            &path,
            UTF8(|lnum, line| {
                let mut captures = matcher.new_captures().unwrap();
                matcher.captures_iter(line.as_bytes(), &mut captures, |c| {
                    n_links += 1;
                    let m = c.get(1).unwrap();
                    let raw = line[m].to_string();

                    let mut link = Link::new(String::from(path_str), lnum as usize, raw);

                    let tx = tx.clone();
                    let http_client = http_client.clone();
                    pool.execute(move || {
                        link.verify(http_client);
                        tx.send(link).unwrap();
                    });

                    true
                })?;

                Ok(true)
            }),
        )?;
    }

    let mut n_bad_links = 0;
    for link in rx.iter().take(n_links) {
        match link.status.as_ref().unwrap() {
            LinkStatus::Reachable => {
                logger.info(&format!("✓ {}", link)[..])?;
            }
            LinkStatus::Questionable(reason) => {
                logger.warn(&format!("✓ {} ({})", link, reason)[..])?
            }
            LinkStatus::Unreachable(reason) => {
                n_bad_links += 1;
                match reason {
                    Some(s) => logger.error(&format!("✗ {} ({})", link, s)[..])?,
                    None => logger.error(&format!("✗ {}", link)[..])?,
                };
            }
        };
    }

    if n_bad_links > 0 {
        logger.error(&format!("Found {} bad links", n_bad_links)[..])?;
        std::process::exit(1);
    }

    Ok(())
}
