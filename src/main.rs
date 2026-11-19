//! CLI для автономного сборщика и сканера диска poler-harvest

use std::path::PathBuf;
use std::process::ExitCode;
use clap::{Parser, ValueEnum};
use poler_harvest::{
    run_harvest, HarvestConfig, HarvestFormat, HarvestMode,
};

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatArg {
    Address,
    Json,
    Markdown,
    Corpus,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeArg {
    Sections,
    Files,
}

#[derive(Parser, Debug)]
#[command(
    name = "poler-harvest",
    author = "Kotokvit / POLER Engineering Core",
    version = "0.96.1",
    about = "Sovereign Ultra-Fast Filesystem Harvester & L1-L3 Address Crystal Scanner (successor to find + ripgrep)",
    after_help = "Использование:\n  poler-harvest <ROOT_DIR> <QUERY> [FORMAT]\n  poler-harvest [OPTIONS] -d <ROOT> -q <QUERY>\n\nФорматы: address (L3 binary stream), json, markdown, corpus"
)]
struct Cli {
    /// Корневой каталог для сканирования (позиционный аргумент 1)
    #[arg(value_name = "ROOT")]
    root: Option<PathBuf>,

    /// Поисковый запрос (позиционный аргумент 2)
    #[arg(value_name = "QUERY")]
    query: Option<String>,

    /// Формат вывода: address (по умолчанию), json, markdown, corpus (позиционный 3)
    #[arg(value_name = "FORMAT")]
    format_pos: Option<String>,

    /// Явный поисковый запрос (--query / -q)
    #[arg(short = 'q', long = "query")]
    query_flag: Option<String>,

    /// Явные корни сканирования (--disk / -d)
    #[arg(short = 'd', long = "disk")]
    roots_flag: Vec<PathBuf>,

    /// Формат вывода через флаг (-f / --format)
    #[arg(short = 'f', long = "format", value_enum)]
    format_flag: Option<FormatArg>,

    /// Режим выдачи: sections (с контекстом) или files
    #[arg(short = 'm', long = "mode", value_enum, default_value = "sections")]
    mode: ModeArg,

    /// Поиск по регулярному выражению
    #[arg(short = 'r', long = "regex")]
    regex: bool,

    /// Число строк контекста вокруг совпадения
    #[arg(short = 'c', long = "context", default_value_t = 0)]
    context: usize,

    /// Файл вывода (по умолчанию '-' — stdout)
    #[arg(short = 'o', long = "out")]
    out: Option<PathBuf>,

    /// Сканировать скрытые файлы и каталоги
    #[arg(short = 'H', long = "hidden")]
    hidden: bool,

    /// Игнорировать правила .gitignore / .ignore
    #[arg(long = "no-ignore")]
    no_ignore: bool,

    /// Отключить чтение и запись персистентного кэша (.t5a / .tar.poler)
    #[arg(long = "no-cache")]
    no_cache: bool,

    /// Число рабочих потоков (0 — авто по ядрам CPU)
    #[arg(short = 'j', long = "threads", default_value_t = 0)]
    threads: usize,

    /// Максимум отобранных файлов (по умолчанию 20 000, 0 — без лимита)
    #[arg(long = "max-files", default_value_t = 20_000)]
    max_files: usize,

    /// First-N режим: останов после max_files (без ранжирования)
    #[arg(long = "first-n")]
    first_n: bool,

    /// Минимум совпадений в файле для отбора
    #[arg(long = "min-matches", default_value_t = 1)]
    min_matches: usize,

    /// Максимальный размер файла для скана (байт)
    #[arg(long = "max-file-bytes", default_value_t = 500 * 1024 * 1024)]
    max_file_bytes: u64,

    /// Защитный кап вывода байт (анти-раздувание RAM/диска)
    #[arg(long = "max-out-bytes", default_value_t = 256 * 1024 * 1024)]
    max_out_bytes: u64,
}

fn parse_format(pos: Option<&str>, flag: Option<FormatArg>) -> HarvestFormat {
    if let Some(f) = flag {
        return match f {
            FormatArg::Address => HarvestFormat::Address,
            FormatArg::Json => HarvestFormat::Json,
            FormatArg::Markdown => HarvestFormat::Markdown,
            FormatArg::Corpus => HarvestFormat::Corpus,
        };
    }
    if let Some(p) = pos {
        match p.to_lowercase().as_str() {
            "json" => HarvestFormat::Json,
            "markdown" | "md" => HarvestFormat::Markdown,
            "corpus" => HarvestFormat::Corpus,
            _ => HarvestFormat::Address,
        }
    } else {
        HarvestFormat::Address
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Разрешение корней: флаги --disk имеют приоритет, иначе позиционный root
    let mut roots = cli.roots_flag;
    if roots.is_empty() {
        if let Some(r) = cli.root {
            roots.push(r);
        }
    }

    // Разрешение запроса: флаг --query имеет приоритет, иначе позиционный query
    let query = cli.query_flag.or(cli.query);

    let Some(query) = query else {
        eprintln!("poler-harvest: ошибка: не указан поисковый запрос (QUERY)");
        eprintln!("Использование: poler-harvest <ROOT_DIR> <QUERY> [FORMAT]");
        return ExitCode::from(2);
    };

    if roots.is_empty() {
        eprintln!("poler-harvest: ошибка: не указан корневой каталог (ROOT_DIR)");
        eprintln!("Использование: poler-harvest <ROOT_DIR> <QUERY> [FORMAT]");
        return ExitCode::from(2);
    }

    let format = parse_format(cli.format_pos.as_deref(), cli.format_flag);
    let mode = match cli.mode {
        ModeArg::Sections => HarvestMode::Sections,
        ModeArg::Files => HarvestMode::Files,
    };

    let cfg = HarvestConfig {
        roots,
        query,
        regex_mode: cli.regex,
        out_path: cli.out.unwrap_or_else(|| PathBuf::from("-")),
        format,
        mode,
        context_lines: cli.context,
        max_files: cli.max_files,
        first_n: cli.first_n,
        min_matches: cli.min_matches,
        max_file_bytes: cli.max_file_bytes,
        max_out_bytes: cli.max_out_bytes,
        threads: cli.threads,
        include_hidden: cli.hidden,
        no_ignore: cli.no_ignore,
        chunk_bytes: 4 << 20, // 4 MB default chunk
        no_cache: cli.no_cache,
    };

    match run_harvest(&cfg) {
        Ok(stats) => {
            eprintln!("{}", stats.summary_line());
            if stats.read_errors > 0 {
                eprintln!("poler-harvest: ошибок чтения: {}", stats.read_errors);
            }
            if stats.files_selected == 0 {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("poler-harvest: ошибка: {e}");
            ExitCode::from(2)
        }
    }
}
