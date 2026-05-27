use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use tracing::debug;
use tracing_subscriber::EnvFilter;
use winland_core::{Manageability, WindowInfo};

#[derive(Debug, Parser)]
#[command(name = "winland")]
#[command(about = "Windows-native tiling window manager experiments")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List discovered desktop windows and explain Winland's conservative filter.
    Windows(WindowsArgs),
}

#[derive(Debug, Args)]
struct WindowsArgs {
    /// Show only windows that Winland would currently consider manageable.
    #[arg(long)]
    manageable_only: bool,
}

fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();

    match cli.command {
        Command::Windows(args) => list_windows(args),
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .init();
}

fn list_windows(args: WindowsArgs) -> Result<()> {
    let windows = winland_win32::enumerate_windows()?;
    debug!(total = windows.len(), "received windows from win32 backend");

    let listed: Vec<_> = windows
        .iter()
        .filter(|window| !args.manageable_only || window.is_manageable())
        .collect();

    print_table(&listed);
    Ok(())
}

fn print_table(windows: &[&WindowInfo]) {
    if windows.is_empty() {
        println!("No windows found.");
        return;
    }

    let rows: Vec<Vec<String>> = windows
        .iter()
        .map(|window| {
            vec![
                window.handle.to_string(),
                manageability_label(window).to_owned(),
                manageability_reason(window).to_owned(),
                truncate(&window.title, 48),
                truncate(&window.class_name, 28),
                window.process_id.to_string(),
                truncate(window.executable_path.as_deref().unwrap_or("-"), 56),
                window.rect.to_string(),
                format!("0x{:08X}", window.styles.style),
                format!("0x{:08X}", window.styles.extended_style),
                yes_no(window.is_visible).to_owned(),
                yes_no(window.is_minimized).to_owned(),
                yes_no(window.is_dwm_cloaked).to_owned(),
                yes_no(window.has_owner).to_owned(),
                yes_no(window.is_tool_window).to_owned(),
            ]
        })
        .collect();

    let headers = [
        "HWND",
        "Status",
        "Reason",
        "Title",
        "Class",
        "PID",
        "Executable Path",
        "Rect",
        "Style",
        "ExStyle",
        "Visible",
        "Minimized",
        "Cloaked",
        "Owner",
        "Tool",
    ];
    let widths = column_widths(&headers, &rows);

    print_row(&headers, &widths);
    print_separator(&widths);
    for row in rows {
        print_row(&row, &widths);
    }
}

fn column_widths(headers: &[&str], rows: &[Vec<String>]) -> Vec<usize> {
    let mut widths: Vec<_> = headers.iter().map(|header| header.len()).collect();

    for row in rows {
        for (index, value) in row.iter().enumerate() {
            widths[index] = widths[index].max(value.len());
        }
    }

    widths
}

fn print_row<T: AsRef<str>>(values: &[T], widths: &[usize]) {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            print!("  ");
        }

        print!("{:<width$}", value.as_ref(), width = widths[index]);
    }

    println!();
}

fn print_separator(widths: &[usize]) {
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            print!("  ");
        }

        print!("{}", "-".repeat(*width));
    }

    println!();
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn manageability_label(window: &WindowInfo) -> &'static str {
    if window.is_manageable() {
        "manage"
    } else {
        "skip"
    }
}

fn manageability_reason(window: &WindowInfo) -> &'static str {
    match window.manageable_reason() {
        Manageability::Manageable => "-",
        Manageability::Unmanageable(reason) => reason,
    }
}
