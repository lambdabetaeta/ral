//! The command line: a folder, and a sentence about what to do in it.
//!
//! Synod's user is a secretary, not a programmer, so this surface is
//! deliberately tiny — the folder, the job, and the two knobs an IT
//! department may need to set.  Everything exarch offers a developer
//! (capability bases, attenuation files, reasoning effort, output
//! formats) is absent on purpose: a flag nobody in the audience can
//! judge is worse than no flag at all.
//!
//! The job itself is resolved by [`exarch::cli::load_seed`], which synod
//! shares rather than re-spells.

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    about = "Synod — an assistant that does office work in one folder you choose",
    long_about = None
)]
pub struct Cli {
    /// The folder to work in. Synod reads and changes the files in this
    /// folder and nowhere else on your computer.
    pub folder: PathBuf,

    /// What you would like done, in your own words. Put it in quotes, for
    /// example: synod ~/Invoices "file every invoice under the month it
    /// was sent".
    #[arg(
        value_name = "JOB",
        num_args = 0..,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        conflicts_with = "job_file",
    )]
    pub job: Vec<String>,

    /// Take the job from a written file instead of typing it here. Use
    /// this when the instructions are long.
    #[arg(long = "job-file", value_name = "FILE")]
    pub job_file: Option<PathBuf>,

    /// Which model does the work. Your IT department normally chooses
    /// this for you; leave it out unless you have been told what to put.
    #[arg(long, value_name = "NAME")]
    pub model: Option<String>,

    /// Which company's models to use — anthropic, openai, and so on.
    /// Leave it out to use whichever account is set up on this computer.
    #[arg(long, value_name = "NAME")]
    pub provider: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_is_required() {
        Cli::try_parse_from(["synod"]).expect_err("synod cannot run without a folder");
    }

    #[test]
    fn the_folder_comes_first_and_the_rest_is_the_job() {
        let cli = Cli::try_parse_from(["synod", "/Users/x/Invoices", "sort", "these", "please"])
            .expect("a folder and a sentence is the ordinary invocation");

        assert_eq!(cli.folder, PathBuf::from("/Users/x/Invoices"));
        assert_eq!(cli.job, ["sort", "these", "please"]);
    }

    /// A job is prose, and prose contains dashes.  A sentence beginning
    /// with one must stay a sentence rather than become an unknown flag.
    #[test]
    fn a_job_may_open_with_a_dash() {
        let cli = Cli::try_parse_from(["synod", "/w", "- rename the scans"])
            .expect("a leading dash is words, not a flag");

        assert_eq!(cli.job, ["- rename the scans"]);
    }

    #[test]
    fn a_written_job_excludes_a_typed_one() {
        Cli::try_parse_from(["synod", "/w", "--job-file", "brief.txt", "and also this"])
            .expect_err("the job comes from one place or the other, never both");

        let cli = Cli::try_parse_from(["synod", "/w", "--job-file", "brief.txt"])
            .expect("a written job alone is fine");
        assert_eq!(cli.job_file, Some(PathBuf::from("brief.txt")));
        assert!(cli.job.is_empty());
    }

    /// The job resolves through exarch's shared seed loader; synod simply
    /// has no flag form of its own to offer it.
    #[test]
    fn typed_words_become_the_job() {
        let cli = Cli::try_parse_from(["synod", "/w", "draft the letter", "keep it short"])
            .expect("parses");
        let job = exarch::cli::load_seed(None, cli.job_file, cli.job)
            .expect("nothing to read from disk")
            .expect("the job is present");

        assert_eq!(job, "draft the letter\nkeep it short");
    }

    #[test]
    fn no_words_at_all_is_no_job() {
        let cli = Cli::try_parse_from(["synod", "/w"]).expect("parses");
        let job = exarch::cli::load_seed(None, cli.job_file, cli.job).expect("nothing to read");

        assert!(job.is_none(), "an empty command line asks for nothing");
    }
}
