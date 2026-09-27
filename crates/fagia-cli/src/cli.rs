use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "fagia",
    version,
    about = "See what is eating your disk and RAM, and reclaim it safely",
    long_about = "See what is eating your disk and RAM, and reclaim it safely.\n\n\
        Every command is read-only except `clean`, `kill`, `pause`, `resume` and `undo`, \
        and those always show what they will do before doing it.\n\n\
        Exit codes: 0 success, 1 error, 2 invalid usage, 3 finished but some paths were \
        skipped, 4 declined at the confirmation."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[command(flatten)]
    pub global: GlobalArgs,
}

#[derive(Debug, Clone, Args)]
pub struct GlobalArgs {
    /// Machine-readable JSON output (versioned schema)
    #[arg(long, global = true)]
    pub json: bool,
    /// CSV output for table commands
    #[arg(long, global = true, conflicts_with = "json")]
    pub csv: bool,
    /// Hide entries smaller than this (binary units: 100M = 100 MiB)
    #[arg(long, global = true, value_name = "SIZE")]
    pub min_size: Option<String>,
    /// Only items untouched for at least this long (e.g. 90d, 12w)
    #[arg(long, global = true, value_name = "AGE")]
    pub older: Option<String>,
    /// Filter by category or rule id (comma separated, e.g. node,rust)
    #[arg(long, global = true, value_delimiter = ',', value_name = "NAMES")]
    pub category: Vec<String>,
    /// Skip paths matching this glob (repeatable)
    #[arg(long, global = true, value_name = "GLOB")]
    pub exclude: Vec<String>,
    /// Use apparent size (file length) instead of disk blocks
    #[arg(long, global = true)]
    pub apparent: bool,
    /// Descend into other local filesystems
    #[arg(long, global = true)]
    pub cross_fs: bool,
    /// Descend into network and FUSE filesystems
    #[arg(long, global = true)]
    pub network_fs: bool,
    /// Don't store this scan in history
    #[arg(long, global = true)]
    pub no_save: bool,
    /// Skip the confirmation question (never the dry-run list)
    #[arg(short = 'y', long = "yes", global = true)]
    pub yes: bool,
    /// Show evidence and extra detail
    #[arg(short, long, global = true)]
    pub verbose: bool,
    /// Config file to use instead of the default
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Biggest folders, ranked
    Top {
        path: Option<PathBuf>,
        /// Levels below the root to rank
        #[arg(long, default_value_t = 1)]
        depth: usize,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Regenerable junk grouped by category
    Suspects {
        path: Option<PathBuf>,
        /// List every item, not only category totals
        #[arg(long)]
        list: bool,
    },
    /// Video, audio and images by size
    Media {
        path: Option<PathBuf>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Largest single files
    Big {
        path: Option<PathBuf>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Duplicate files and folders; --similar finds near-duplicates, --clean removes extra copies
    Dupes(DupesCli),
    /// What is in the trash; --empty deletes it for good
    Trash {
        /// Permanently delete what is in the trash (--older limits it)
        #[arg(long)]
        empty: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Suspects untouched past the stale threshold
    Stale { path: Option<PathBuf> },
    /// Growth since the previous saved scan
    Diff {
        path: Option<PathBuf>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Pick suspects and reclaim space (moves to trash by default)
    Clean {
        path: Option<PathBuf>,
        /// Delete permanently instead of moving to the trash
        #[arg(long)]
        permanent: bool,
        /// Allow running as root
        #[arg(long)]
        as_root: bool,
        /// Show the plan and stop
        #[arg(long)]
        dry_run: bool,
    },
    /// Restore what the last clean moved to the trash
    Undo {
        /// List recent clean runs instead of restoring
        #[arg(long)]
        list: bool,
        /// Run id to restore (default: the most recent)
        run: Option<String>,
    },
    /// RAM overview and top apps
    Mem(MemArgs),
    /// Quit an app group safely (SIGTERM, then optional SIGKILL)
    Kill(KillArgs),
    /// Pause an app group (SIGSTOP)
    Pause(SignalArgs),
    /// Resume a paused app group (SIGCONT)
    Resume(SignalArgs),
    /// Interactive full-screen interface
    Ui {
        path: Option<PathBuf>,
        /// Allow cleaning while running as root
        #[arg(long)]
        as_root: bool,
    },
    /// List, test and validate rules
    Rules {
        #[command(subcommand)]
        action: Option<RulesCmd>,
    },
    /// Show or edit the config file
    Config {
        #[command(subcommand)]
        action: Option<ConfigCmd>,
    },
    /// Print shell completions
    Completions { shell: clap_complete::Shell },
    /// Print the man page (roff)
    #[command(hide = true)]
    Man,
}

#[derive(Debug, Clone, Args)]
pub struct MemArgs {
    /// Live updating view
    #[arg(long)]
    pub live: bool,
    /// Dev-tool suspects only
    #[arg(long)]
    pub dev: bool,
    /// Long-running orphaned dev processes
    #[arg(long)]
    pub forgotten: bool,
    /// Record for this long, then report leak suspects (e.g. 1h, 15m)
    #[arg(long, value_name = "DURATION")]
    pub watch: Option<String>,
    #[arg(long, default_value_t = 15)]
    pub limit: usize,
    /// Expand each group into its processes
    #[arg(long)]
    pub expand: bool,
}

#[derive(Debug, Clone, Args)]
pub struct KillArgs {
    /// App group name (as shown by `fagia mem`) or a PID
    pub app: String,
    /// Offer SIGKILL if the app is still running after the grace period
    #[arg(long)]
    pub force: bool,
    /// Allow targeting other users' processes (needs root)
    #[arg(long)]
    pub allow_other_users: bool,
    /// Seconds to wait after SIGTERM
    #[arg(long, value_name = "SECONDS")]
    pub grace: Option<u64>,
}

#[derive(Debug, Clone, Args)]
pub struct SignalArgs {
    pub app: String,
    #[arg(long)]
    pub allow_other_users: bool,
}

#[derive(Debug, Subcommand)]
pub enum RulesCmd {
    /// List all rules (built-in and user)
    List,
    /// Show which rule matches a path and why
    Test { path: PathBuf },
    /// Check rules for errors and unsafe paths
    Validate,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// Print the config file path
    Path,
    /// Write a commented default config if none exists
    Init,
    /// Open the config in $EDITOR
    Edit,
    /// Print the effective settings
    Show,
}

#[derive(Debug, Clone, Args)]
pub struct DupesCli {
    pub path: Option<PathBuf>,
    /// Sets shown per section
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    /// How names must match (content is always compared byte for byte):
    /// name = same name ignoring "(1)", "- Copy"…; loose = also ignoring
    /// tags like "(z-lib.sk)" and punctuation; content = any name
    #[arg(long = "match", value_enum, default_value_t = MatchArg::Name, value_name = "HOW")]
    pub match_by: MatchArg,
    /// Same as --match content
    #[arg(long, hide = true)]
    pub any_name: bool,
    /// Skip the duplicate-folder check
    #[arg(long)]
    pub no_folders: bool,
    /// Also find near-duplicates (listed, never removed): text, images,
    /// media, content; all of them when given without a list
    #[arg(long, value_enum, value_delimiter = ',', num_args = 0.., value_name = "KINDS")]
    pub similar: Option<Vec<SimilarArg>>,
    /// Largest perceptual-hash distance for similar images (0-64)
    #[arg(long, default_value_t = 6, value_name = "BITS")]
    pub image_distance: u32,
    /// Smallest share of common content for similar files (0-1)
    #[arg(long, default_value_t = 0.8, value_name = "SHARE")]
    pub similarity: f64,
    /// Keep one copy of each set and move the others to the trash
    #[arg(long)]
    pub clean: bool,
    /// Replace extra copies with hard links instead (implies --clean)
    #[arg(long)]
    pub link: bool,
    /// Prefer keeping copies under this folder (repeatable)
    #[arg(long, value_name = "DIR")]
    pub keep_under: Vec<PathBuf>,
    /// Also consider copies that are not documents or media, or that are
    /// in hidden or program folders (never inside .git)
    #[arg(long)]
    pub any_type: bool,
    /// Show the plan and stop
    #[arg(long)]
    pub dry_run: bool,
    /// Allow running as root
    #[arg(long)]
    pub as_root: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum MatchArg {
    Name,
    Loose,
    Content,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SimilarArg {
    Text,
    Images,
    Media,
    Content,
}
