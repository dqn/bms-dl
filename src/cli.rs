use clap::Parser;

/// BMS difficulty table downloader
#[derive(Parser)]
#[command(version, about)]
pub struct Args {
    /// BMS table URL (e.g. https://stellabms.xyz/sl/table.html)
    pub table_url: String,

    /// Output directory
    #[arg(short, long, default_value = ".")]
    pub output: String,

    /// Number of concurrent downloads
    #[arg(short, long, default_value_t = 8)]
    pub jobs: usize,

    /// Skip downloading diffs
    #[arg(long)]
    pub no_diff: bool,

    /// Filter by level (e.g. "0", "5")
    #[arg(long)]
    pub level: Option<String>,

    /// Skip entries that already exist in the output directory
    #[arg(long)]
    pub skip_existing: bool,
}
