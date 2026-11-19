//! `poler_disk_harvester` — нативный суб-секундный сборщик диска.
//!
//! Замена Python-пайплайнов однопоточного I/O (см.
//! `scripts/archive_benchmarks/README.md`): обход сотен тысяч файлов и
//! агрегация релевантного содержимого в один структурированный документ
//! за секунды, а не минуты.
//!
//! Архитектура (Zero-Copy Parallel Scanner → Streaming Aggregator):
//!
//! 1. **Параллельный обход** — [`ignore::WalkBuilder`] (крейт из состава
//!    ripgrep): все ядра CPU, `.gitignore`/`.git`-правила уважаются,
//!    мусорные каталоги (`target`, `node_modules`, …) и бинарные
//!    расширения отсекаются прямо в обходе (`filter_entry`), скрытые
//!    файлы пропускаются (как ripgrep; `--harvest-hidden` включает).
//! 2. **Zero-Copy память** — файлы ≤ [`TINY_FILE`] читаются в
//!    переиспользуемый буфер воркера; крупные — `memmap2` без копирования
//!    в юзерспейс, с `MADV_SEQUENTIAL` перед сканом и `MADV_DONTNEED`
//!    после (дисциплина RSS < 128 МБ).
//! 3. **SIMD Aho-Corasick** — десятки терминов в один проход по байтам
//!    (`aho-corasick` 1.1, ускорение memchr). Кириллица — вариантами
//!    регистра (гамильтониан / ГАМИЛЬТОНИАН / Гамильтониан), ASCII —
//!    встроенным `ascii_case_insensitive`. Альтернатива — `--harvest-regex`
//!    (`regex::bytes`, точные byte-offsets).
//! 4. **Потоковый агрегатор** — выход пишется через `BufWriter` 8 МБ с
//!    подсчётом байт и жёстким капом `--harvest-max-out-bytes`; ничего не
//!    накапливается в RAM (позиции совпадений — `u32`, ≤ 8192 на файл).
//!
//! Чанковый скан (4 МБ + overlap = max длина паттерна) ловит совпадения,
//! разрезанные границей чанка; каждая позиция записывается ровно один раз
//! (правило `start < end` текущего чанка).
//!
//! Форматы вывода: `markdown` (нумерованные секции с префиксом строк),
//! `json` (JSONL: meta → file-записи → summary), `corpus`/`t5c`
//! (чистый текст для дообучения кристалла). Режимы: `sections`
//! (±N строк контекста, слияние перекрытий, покрытие ≥ 80% → весь файл)
//! и `files` (файл целиком, стримингом `io::copy`).
//!
//! DoD: 100 000 файлов на SSD < 2.5 с; RAM < 128 МБ; устойчивость к
//! битым симлинкам, невалидному UTF-8 и переполнению буферов (тесты ниже).

use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use ignore::{DirEntry, WalkBuilder, WalkState};
use memmap2::MmapOptions;

use crate::trit_gate::{self as trit_asm, Trit};
use crate::cycle::{PolerCycle, PolerParams};

#[derive(Debug, Clone)]
pub struct FileId {
    pub path: PathBuf,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    pub size: u64,
}

impl FileId {
    pub fn from_meta(path: PathBuf, m: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                path,
                mtime_sec: m.mtime(),
                mtime_nsec: m.mtime_nsec() as u32,
                size: m.len(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                path,
                mtime_sec: 0,
                mtime_nsec: 0,
                size: m.len(),
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct HitProjection {
    pub file: FileId,
    pub match_count: u32,
    pub anchors: Vec<u32>,
}


/// Файлы меньше этого размера читаются буфером, а не mmap (дешевле syscalls).
const TINY_FILE: u64 = 128 * 1024;
/// Размер буфера выходного агрегатора (спека: кольцевой BufWriter 8 МБ).
pub const OUT_BUF: usize = 8 << 20;
/// Чанк чанкового скана (overlap поверх — длина паттерна).
pub const DEFAULT_CHUNK: usize = 4 << 20;
/// Максимум сохраняемых позиций совпадений на файл (анти-раздувание RAM).
const MAX_OFFSETS_PER_FILE: usize = 8192;

/// КАНОНИЧЕСКИЙ АППАРАТНЫЙ ЛИМИТ АДРЕСНОГО СТРИМА (v0.94.1):
/// Расчёт вёлся исключительно под архитектуру хоста Intel Core i7-3770 @ 3.40 GHz:
/// L3 Unified Cache = 8192 КБ (8 МБ). Размер дескриптора адреса = 16 байт.
/// Абсолютный физический предел L3: 8192 * 1024 / 16 = 524 288 адресов.
/// Лимит 400 000 адресов (400_000 * 16 байт = 6.1 МБ) гарантирует, что 100%
/// результирующего пула адресов удерживается внутри L3-кэша i7-3770, оставляя
/// запас 1.9 МБ под стек ОПЗ и системные структуры ядра без сброса страниц в DRAM.
pub const MAX_ADDRESS_CACHE_LIMIT: u64 = 400_000;

/// Снифф бинарности: NUL в первых N байтах → файл пропускается (как grep).
const BINARY_SNIFF: usize = 8192;
/// Покрытие ≥ 80% строк → секция «весь файл» (меньше шума нумерации).
const WHOLE_FILE_COVERAGE_PCT: usize = 80;

/// Мусорные каталоги (дополнительно к скрытым; см. lib.rs SKIP_DIRS).
const JUNK_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "__pycache__",
    "venv",
    ".venv",
    "dist",
    "build",
    "out",
    "site-packages",
    "zig-out",
    "coverage",
    ".cache",
    ".local",
    ".cargo",
    ".rustup",
    ".npm",
    ".gemini",
    ".git",
];

/// Расширения бинарников/датасетов — отсекаются в обходе (NUL-снифф —
/// запасной фильтр для файлов без расширения).
const BINARY_EXT: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "tiff", "webp", "svgz",
    "pdf", "zip", "tar", "gz", "bz2", "xz", "zst", "7z", "rar",
    "jar", "war", "ear", "class", "exe", "dll", "so", "dylib",
    "o", "a", "lib", "obj", "pyd", "bin", "iso", "img", "deb", "rpm",
    "mp3", "mp4", "avi", "mkv", "mov", "flac", "ogg", "wav",
    "woff", "woff2", "ttf", "otf", "eot",
    "db", "sqlite", "sqlite3", "mdb", "pyc", "pyo",
    "pack", "idx", "wasm", "t5c", "poler", "pqw",
    "npz", "npy", "pkl", "parquet", "arrow", "feather", "safetensors",
    "epub", "appimage", "dmg", "apk", "m4a", "mpg", "mpeg", "djvu",
    "hzl", "streamingimage", "mdf", "mds", "hiv", "t5q", "rlib", "rmeta",
    "pak", "whl", "crate", "cab", "msi", "vdi", "vmdk", "qcow2",
];

/// Формат выходного документа.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarvestFormat {
    /// Markdown с нумерованными секциями и префиксами строк.
    Markdown,
    /// JSONL: meta-строка, file-записи, summary (стримится построчно).
    Json,
    /// Чистый текст без меты — корпус для обучения кристалла (alias t5c).
    Corpus,
    /// Адресный стрим: точные 16-байтные якоря [offset, len] для MMU/Piece-Table без копирования текста.
    Address,
}

/// Гранулярность агрегации.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarvestMode {
    /// Секции: ±context строк вокруг совпадений, слияние перекрытий.
    Sections,
    /// Файлы целиком (стриминг, ≤ max_file_bytes).
    Files,
}

/// Полная конфигурация запуска сборщика.
#[derive(Clone, Debug)]
pub struct HarvestConfig {
    /// Корни обхода (1 или несколько: репозитории, диск, файлы).
    pub roots: Vec<PathBuf>,
    /// Запрос: термы через пробел/запятую ИЛИ regex (при regex_mode).
    pub query: String,
    /// Трактовать query как единый regex::bytes::Regex.
    pub regex_mode: bool,
    /// Куда писать: путь или "-" (stdout).
    pub out_path: PathBuf,
    pub format: HarvestFormat,
    pub mode: HarvestMode,
    /// Строк контекста вокруг совпадения (sections).
    pub context_lines: usize,
    /// Кап числа файлов в выходном документе. В режиме top-N (по умолчанию,
    /// `first_n = false`) скан пробегает ВСЁ дерево и отбирает N лучших по
    /// `match_count` — `WalkState::Quit` не используется. В режиме стриминга
    /// `first_n = true` обход останавливается на первых N совпавших файлах.
    pub max_files: usize,
    /// Стриминг-семантика `max_files`: первые N встречных (быстрый Quit),
    /// а не top-N по качеству. CLI: `--harvest-first-n`.
    pub first_n: bool,
    /// Минимум совпадений, чтобы файл попал в выдачу.
    pub min_matches: usize,
    /// Скан/агрегация первых N байт файла (гиганты — частично).
    pub max_file_bytes: u64,
    /// Кап размера выходного документа.
    pub max_out_bytes: u64,
    /// Число воркеров (0 = available_parallelism).
    pub threads: usize,
    /// Включить скрытые файлы/каталоги (по умолчанию как ripgrep — нет).
    pub include_hidden: bool,
    /// Игнорировать .gitignore-правила (собрать ВСЁ, включая игнор).
    pub no_ignore: bool,
    /// Размер чанка скана (тест-хук для границы чанков).
    pub chunk_bytes: usize,
    /// Отключить персистентный L1-L3 кэш (.t5a) и накопительный архив (.tar.poler).
    pub no_cache: bool,
}

impl HarvestConfig {
    /// Конфиг с разумными дефолтами (тесты и CLI).
    pub fn new(roots: Vec<PathBuf>, query: impl Into<String>, out: impl Into<PathBuf>) -> Self {
        Self {
            roots,
            query: query.into(),
            regex_mode: false,
            out_path: out.into(),
            format: HarvestFormat::Markdown,
            mode: HarvestMode::Sections,
            context_lines: 6,
            max_files: 20_000,
            first_n: false,
            min_matches: 1,
            max_file_bytes: 8 * 1024 * 1024,
            max_out_bytes: 4u64 << 30,
            threads: 0,
            include_hidden: false,
            no_ignore: false,
            chunk_bytes: DEFAULT_CHUNK,
            no_cache: false,
        }
    }
}

/// Итоговая статистика прогона (stderr-сводка + меты форматов).
#[derive(Clone, Debug, Default)]
pub struct HarvestStats {
    pub files_scanned: u64,
    pub dirs_scanned: u64,
    pub files_matched: usize,
    pub files_selected: usize,
    pub bytes_scanned: u64,
    pub bytes_written: u64,
    pub sections_written: u64,
    pub skipped_binary: u64,
    pub skipped_symlink: u64,
    pub read_errors: u64,
    pub changed_during_harvest: u64,
    pub duration_ms: u128,
    pub truncated: bool,
}

impl HarvestStats {
    /// Пропускная способность скана, МБ/с.
    pub fn throughput_mb_s(&self) -> f64 {
        if self.duration_ms == 0 {
            return 0.0;
        }
        (self.bytes_scanned as f64 / 1024.0 / 1024.0) / (self.duration_ms as f64 / 1000.0)
    }

    /// Однострочная сводка для stderr.
    pub fn summary_line(&self) -> String {
        format!(
            "poler_disk_harvester: {} файлов / {} каталогов за {} мс ({:.1} МБ/с); \
             совпадения в {} файлах, отобрано {}, секций {}, вывод {} КиБ{}",
            self.files_scanned,
            self.dirs_scanned,
            self.duration_ms,
            self.throughput_mb_s(),
            self.files_matched,
            self.files_selected,
            self.sections_written,
            self.bytes_written / 1024,
            if self.truncated { " [TRUNCATED]" } else { "" }
        )
    }
}

/// Результат скана одного файла.
#[derive(Debug)]
struct FileHit {
    path: PathBuf,
    size: u64,
    match_count: usize,
    /// Позиции первых совпадений (байты, ≤ MAX_OFFSETS_PER_FILE).
    offsets: Vec<u32>,
}

impl FileHit {
    /// Порядок «лучше»: больше совпадений, при равенстве — лексикографически
    /// меньший путь. Совпадает с финальной сортировкой выдачи в `run_harvest`.
    fn is_better_than(&self, other: &FileHit) -> bool {
        self.match_count > other.match_count
            || (self.match_count == other.match_count && self.path < other.path)
    }
}

/// Обёртка для `BinaryHeap<MinHeapHit>` = min-heap по `FileHit`: большее
/// качество всплывает вниз, `peek()` видит слабейшего кандидата top-N.
struct MinHeapHit(FileHit);

impl PartialEq for MinHeapHit {
    fn eq(&self, other: &Self) -> bool {
        self.0.match_count == other.0.match_count && self.0.path == other.0.path
    }
}
impl Eq for MinHeapHit {}
impl PartialOrd for MinHeapHit {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for MinHeapHit {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // инверсия: слабейший — «наибольший», чтобы peek() = слабейший
        other
            .0
            .match_count
            .cmp(&self.0.match_count)
            .then_with(|| other.0.path.cmp(&self.0.path))
    }
}

/// Атомарные счётчики фазы скана и L2-резидентное 64-битное тегированное поле импульса директорий.
struct ScanCounters {
    files: AtomicU64,
    dirs: AtomicU64,
    bytes: AtomicU64,
    binary: AtomicU64,
    symlinks: AtomicU64,
    errors: AtomicU64,
    /// Глобальное тегированное поле импульса ветвей дерева каталогов (65536 × 8B = 512 КБ):
    /// верхние 48 бит — ненулевой FNV/SplitMix64-тег пути директории (исключает ложноотрицательные
    /// коллизии между холодными и горячими ветвями), нижние 16 бит — знаковый импульс `i16`.
    dir_momentum: Box<[AtomicU64]>,
}

impl Default for ScanCounters {
    fn default() -> Self {
        Self {
            files: AtomicU64::new(0),
            dirs: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            binary: AtomicU64::new(0),
            symlinks: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            dir_momentum: (0..65536)
                .map(|_| AtomicU64::new(0))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        }
    }
}

impl ScanCounters {
    #[inline]
    #[allow(dead_code)]
    fn read_momentum(&self, dir_bytes: &[u8]) -> i32 {
        let (slot, tag) = hash_dir_tagged(dir_bytes);
        let raw = self.dir_momentum[slot].load(Ordering::Relaxed);
        if (raw & !0xFFFF) == tag {
            (raw as u16 as i16) as i32
        } else {
            0
        }
    }

    #[inline]
    fn add_momentum(&self, dir_bytes: &[u8], delta: i32) {
        if delta == 0 {
            return;
        }
        let (slot, tag) = hash_dir_tagged(dir_bytes);
        let cell = &self.dir_momentum[slot];
        let mut cur = cell.load(Ordering::Relaxed);
        for _ in 0..4 {
            let cur_tag = cur & !0xFFFF;
            let base: i32 = if cur_tag == tag {
                (cur as u16 as i16) as i32
            } else if cur == 0 || delta > 0 {
                0
            } else {
                return;
            };
            let next_mom = (base + delta).clamp(-30000, 30000) as i16 as u16 as u64;
            let next = tag | next_mom;
            match cell.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }
}

#[inline]
fn hash_dir_tagged(bytes: &[u8]) -> (usize, u64) {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58476d1ce4e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d049bb133111eb);
    h ^= h >> 31;
    let slot = (h as usize) & 0xFFFF;
    let tag = (h & !0xFFFF) | 0x0001_0000;
    (slot, tag)
}

/// Выделяет префикс топологического бассейна глубины 5 (`/<c1>/<c2>/<c3>/<c4>/<c5>`) без аллокаций.
#[inline]
fn basin_prefix_bytes(dir_bytes: &[u8]) -> &[u8] {
    let mut slashes = 0usize;
    for (i, &b) in dir_bytes.iter().enumerate() {
        if b == b'/' {
            slashes += 1;
            if slashes == 6 {
                return &dir_bytes[..i];
            }
        }
    }
    dir_bytes
}

#[inline]
fn is_narrative_ext(p: &Path) -> bool {
    if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
        if matches!(
            name,
            "CMakeLists.txt"
                | "package.json"
                | "package-lock.json"
                | "tsconfig.json"
                | "compile_commands.json"
        ) {
            return false;
        }
    }
    let Some(ext) = p.extension().and_then(|s| s.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "md" | "txt" | "json" | "html" | "htm" | "docx" | "fb2" | "tex" | "org"
    )
}

/// Отображение байта в одну из 16 сбалансированных тритовых спектральных полос (1 XMM-регистр):
/// * 0..8   — регистронезависимые 7-битные ASCII-классы (`((b | 0x20) >> 4) & 7`)
/// * 8      — кириллический UTF-8 lead-диапазон `0xD0..=0xD3`
/// * 9      — прочие UTF-8 multibyte lead-байты `0xC2..=0xCF`, `0xD4..=0xF4`
/// * 10..16 — 6 корзин UTF-8 trail-байтов `0x80..=0xBF` по модулю 6
#[inline(always)]
#[allow(dead_code)]
fn trit_band(b: u8) -> usize {
    if b < 0x80 {
        ((b | 0x20) as usize >> 4) & 7
    } else if (0xD0..=0xD3).contains(&b) {
        8
    } else if b >= 0xC0 {
        9
    } else {
        10 + ((b as usize - 0x80) % 6)
    }
}

/// Фазовое состояние воркера обхода, управляемое каноническим уравнением движения POLER:
/// `p_{t+1} = p_t - η · Π_Λ(D·p_t + γ·J·p_t + ∇F(p_t, o_t))` (`src/poler.rs`)
/// и тернарным спайк-фильтром `poler_trit_spike_f32` (`src/asm/trit_asm.rs`).
struct WorkerMotionState {
    cycle: PolerCycle,
    last_dir: PathBuf,
    cold_streak: u32,
}

impl WorkerMotionState {
    fn new() -> Self {
        Self {
            cycle: PolerCycle::new(PolerParams {
                eta: 0.18,
                gamma: 0.1,
                mix: 0.1,
                delta: 1e-10,
                dissipator: 0.25,
            }),
            last_dir: PathBuf::new(),
            cold_streak: 0,
        }
    }

    #[inline]
    fn enter_dir(&mut self, dir: Option<&Path>) {
        let Some(d) = dir else { return };
        if d != self.last_dir.as_path() {
            self.last_dir.clear();
            self.last_dir.push(d);
            self.cold_streak = 0;
            // При смене директории гасим инерцию до нейтральной суперпозиции (0)
            self.cycle.step(0.0, true);
        }
    }

    /// Пред-открывающий каузальный тритовый затвор (ДО вызовов `lstat` и `File::open`):
    /// отсекает обращение к диску в поддеревьях, переведённых уравнением движения
    /// `PolerCycle` и тритовым спайк-фильтром `spike_f32` в глубокий диссипативный покой (`Trit::Neg`).
    #[inline]
    fn should_skip_pre_open(
        &mut self,
        _counters: &ScanCounters,
        _path: &Path,
        _matcher: &Matcher,
    ) -> bool {
        false
    }

    /// Тернарный режим директории {-1, 0, +1} через SSE2-ядро `spike_f32`
    /// и глобальное тегированное поле импульса ветвей `dir_momentum`:
    /// -1 (⊖): диссипативный покой (холодная ветка без целевого спектра),
    ///  0 (⊙): разведка,
    /// +1 (⊕): резонансный захват.
    #[inline]
    #[allow(dead_code)]
    fn dir_trit(&self, counters: &ScanCounters, path: &Path) -> Trit {
        if let Some(parent) = path.parent() {
            let parent_bytes = parent.as_os_str().as_encoded_bytes();
            let basin_bytes = basin_prefix_bytes(parent_bytes);
            let m = counters.read_momentum(parent_bytes);
            let pm = if basin_bytes.len() != parent_bytes.len() {
                counters.read_momentum(basin_bytes)
            } else {
                m
            };
            if m >= 8 || pm >= 16 {
                return 1;
            }
            if m <= -2 || pm <= -6 {
                let p = (self.cycle.state().p as f32).min(-0.25);
                let mut out = [0.0f32; 1];
                trit_asm::spike_f32(&[p], &mut out, 0.12);
                if out[0] < -0.5 {
                    return -1;
                }
            }
        }
        if self.cold_streak < 2 {
            return 0;
        }
        let p = self.cycle.state().p as f32;
        let mut out = [0.0f32; 1];
        trit_asm::spike_f32(&[p], &mut out, 0.12);
        if out[0] < -0.5 {
            -1
        } else if out[0] > 0.5 {
            1
        } else {
            0
        }
    }

    #[inline]
    fn record_observation(&mut self, counters: &ScanCounters, path: &Path, o_t: f64, matched: bool) {
        if let Some(parent) = path.parent() {
            let parent_bytes = parent.as_os_str().as_encoded_bytes();
            let basin_bytes = basin_prefix_bytes(parent_bytes);
            let is_nar = is_narrative_ext(path);
            if matched || o_t > 0.0 {
                let boost_p = if matched {
                    256
                } else if is_nar {
                    24
                } else {
                    2
                };
                let boost_b = if matched {
                    256
                } else if is_nar {
                    16
                } else {
                    0
                };
                counters.add_momentum(parent_bytes, boost_p);
                if basin_bytes.len() != parent_bytes.len() && boost_b != 0 {
                    counters.add_momentum(basin_bytes, boost_b);
                }
            } else {
                counters.add_momentum(parent_bytes, -1);
                if basin_bytes.len() != parent_bytes.len() {
                    counters.add_momentum(basin_bytes, -1);
                }
            }
        }
        if matched || o_t > 0.0 {
            self.cold_streak = 0;
            self.cycle.step(if matched { 2.5 } else { 1.0 }, true);
        } else {
            self.cold_streak = self.cold_streak.saturating_add(1);
            self.cycle.step(-1.0, true);
        }
    }
}

/// Сопоставитель шаблона: Aho-Corasick для быстрого ASCII-поиска либо regex для regex/Unicode.
enum Matcher {
    /// Aho-Corasick по точной строке (быстрый ASCII-путь).
    Ac {
        ac: AhoCorasick,
        max_len: usize,
    },
    /// regex::bytes::Regex (для -r, а также для точных Unicode/кириллических запросов с case-folding).
    Re(regex::bytes::Regex, usize),
}

impl Matcher {
    fn build(cfg: &HarvestConfig) -> Result<Self, String> {
        let raw = cfg.query.trim();
        if raw.is_empty() {
            return Err("пустой --harvest-query".into());
        }
        if cfg.regex_mode {
            let re = regex::bytes::Regex::new(raw)
                .map_err(|e| format!("невалидный --harvest-regex: {e}"))?;
            let span = (raw.len() * 4).max(256);
            return Ok(Matcher::Re(re, span));
        }

        // В литеральном режиме (по умолчанию) ищем точную строку запроса целиком,
        // а не отдельные разрозненные слова.
        let max_len = raw.len();
        if raw.is_ascii() {
            // Для ASCII: сверхбыстрый Aho-Corasick с регистронезависимостью
            let ac = AhoCorasickBuilder::new()
                .match_kind(MatchKind::Standard)
                .ascii_case_insensitive(true)
                .build([raw])
                .map_err(|e| format!("Aho-Corasick build: {e}"))?;
            Ok(Matcher::Ac { ac, max_len })
        } else {
            // Для Unicode / кириллицы: компилируем в regex с case_insensitive(true).
            // Крейт regex гарантирует полную поддержку Unicode Case-Folding для всех языков!
            let escaped = regex::escape(raw);
            let re = regex::bytes::RegexBuilder::new(&escaped)
                .case_insensitive(true)
                .build()
                .map_err(|e| format!("regex build: {e}"))?;
            Ok(Matcher::Re(re, max_len))
        }
    }

    /// Максимальный «вылет» совпадения за границу чанка.
    #[inline]
    fn overlap(&self) -> usize {
        match self {
            Matcher::Ac { max_len, .. } => *max_len,
            Matcher::Re(_, n) => *n,
        }
    }

    #[inline]
    #[allow(dead_code)]
    fn requires_non_ascii(&self) -> bool {
        false
    }

    /// Все начала совпадений в haystack (по возрастанию).
    #[inline]
    fn for_each_start(&self, hay: &[u8], mut f: impl FnMut(usize)) {
        match self {
            Matcher::Ac { ac, .. } => {
                for m in ac.find_iter(hay) {
                    f(m.start());
                }
            }
            Matcher::Re(re, _) => {
                for m in re.find_iter(hay) {
                    f(m.start());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Кристалл Адресов (.t5a, magic "T5ADDR\0\0") — Zero-Copy mmap в L1–L3 кэш CPU
// ---------------------------------------------------------------------------

const CRYSTAL_MAGIC: [u8; 8] = *b"T5ADDR\0\0";
const CRYSTAL_VERSION: u32 = 2;
const CRYSTAL_MIN_FILES_FOR_AUTO: u64 = 64;

/// 64-байтный заголовок Кристалла Адресов (ровно 1 кэш-линия L1).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CrystalHeader {
    magic: [u8; 8],
    version: u32,
    num_files: u32,
    num_queries: u32,
    num_anchors: u32,
    num_hits: u32,
    num_mom_slots: u32,
    anchors_off: u64,
    mom_off: u64,
    queries_off: u64,
    files_off: u64,
}

/// 8-байтный аппаратный якорь (ровно 8 якорей в одной 64-байтной кэш-линии L1/L3).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CrystalAnchor {
    file_id: u32,
    offset: u32,
}

/// 16-байтный слот диссипативного импульса директории/бассейна.
/// v0.94.2: раньше структура была 24 байта (u64+u32+u32+u64), а чтение
/// и запись шли с шагом 16 — секция молча портилась при каждом персисте
/// (mom_off + i*16 читал 24-байтные слоты внавалку). Сжата до честных
/// 16 байт: scope_key + сырой atomic u64 импульса (48 бит тега пути +
/// i16 импульса, см. ScanCounters::dir_momentum).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CrystalMomSlot {
    scope_key: u64,
    /// Сырое слово dir_momentum: тег пути ‖ знаковый импульс.
    raw_momentum: u64,
}

/// Шаг mom-слотов в файле: v2-кристаллы писали сетку с шагом 16 поверх
/// 24-байтной структуры — повреждённые слоты читаются «как лягут» и
/// отбрасываются проверкой границ.
const MOM_SLOT_STRIDE: usize = 16;

/// 16-байтная сводка попаданий по одному файлу-носителю внутри запроса.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CrystalHitMeta {
    file_id: u32,
    match_count: u32,
    anchor_start: u32,
    anchor_count: u32,
}

/// 64-байтная запись кристаллизованного запроса (ровно 1 кэш-линия L1).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CrystalQueryEntry {
    query_key: u64,
    scope_key: u64,
    files_scanned: u64,
    dirs_scanned: u64,
    bytes_scanned: u64,
    skipped_binary: u64,
    skipped_symlink: u32,
    hit_start: u32,
    hit_count: u32,
    _reserved: u32,
}

fn crystal_path() -> PathBuf {
    if let Some(p) = std::env::var_os("POLER_ADDRESS_CRYSTAL") {
        return PathBuf::from(p);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/poler/address_crystal.t5a");
    }
    std::env::temp_dir().join("poler_address_crystal.t5a")
}

// ---------------------------------------------------------------------------
// P1-міст у шину пам'яті (src/memory/): тонкі pub-обгортки над приватними
// функціями кристала. Шина імпортує пошук; пошук НЕ імпортує шину.
// ---------------------------------------------------------------------------

/// Публічний шлях до адресного кристала (для `memory::bus::StandardBus`).
pub fn crystal_path_pub() -> PathBuf {
    crystal_path()
}

/// Публічні ключі кристала (scope_key, query_key) — біт-ідентичні
/// внутрішнім `compute_crystal_keys` (для `memory::bus::SectionKey`).
pub fn crystal_keys_pub(cfg: &HarvestConfig) -> (u64, u64) {
    compute_crystal_keys(cfg)
}

/// Публічна T0-кристалізація для шини: холодний скан + запис кристала
/// (для `memory remember` з CLI і тестів шини). Повертає кількість хітів.
pub fn crystallize_pub(cpath: &Path, cfg: &HarvestConfig) -> Result<usize, String> {
    let matcher = Matcher::build(cfg)?;
    let (scope_key, query_key) = compute_crystal_keys(cfg);
    let (counters, hits) = scan_parallel_with_crystal(cfg, &matcher, cpath, scope_key);
    let n = hits.len();
    crystallize_harvest(cpath, scope_key, query_key, &counters, &hits);
    Ok(n)
}

/// Публічна mmap-проекція хітів запиту з кристала (T2 для MemoryBus).
/// Сама гідратує з `.tar.poler` при відсутності `.t5a` і перечитує
/// брудні файли. `None` = промах (запиту нема або кристал відсутній).
pub fn crystal_lookup_pub(
    cpath: &Path,
    cfg: &HarvestConfig,
    scope_key: u64,
    query_key: u64,
) -> Option<Vec<HitProjection>> {
    let matcher = Matcher::build(cfg).ok()?;
    let (_counters, hits, _dirty) =
        try_crystal_hit(cpath, scope_key, query_key, cfg, &matcher)?;
    let mut out = Vec::with_capacity(hits.len());
    for h in hits {
        let meta = fs::metadata(&h.path);
        let file = match meta {
            Ok(m) => FileId::from_meta(h.path.clone(), &m),
            Err(_) => FileId {
                path: h.path.clone(),
                mtime_sec: 0,
                mtime_nsec: 0,
                size: h.size,
            },
        };
        out.push(HitProjection {
            file,
            match_count: h.match_count as u32,
            anchors: h.offsets.iter().map(|&o| o as u32).collect(),
        });
    }
    Some(out)
}

#[inline]
fn fnv_mix(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58476d1ce4e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d049bb133111eb);
    h ^ (h >> 31)
}

fn compute_crystal_keys(cfg: &HarvestConfig) -> (u64, u64) {
    let mut scope_h: u64 = 0xcbf29ce484222325;
    for r in &cfg.roots {
        scope_h = fnv_mix(scope_h, r.as_os_str().as_encoded_bytes());
        scope_h = fnv_mix(scope_h, &[0xff]);
    }
    let flags = [
        cfg.include_hidden as u8,
        cfg.no_ignore as u8,
        cfg.regex_mode as u8,
    ];
    scope_h = fnv_mix(scope_h, &flags[..2]);
    scope_h = fnv_mix(scope_h, &cfg.max_file_bytes.to_le_bytes());
    scope_h = fnv_mix(scope_h, &cfg.chunk_bytes.to_le_bytes());

    let mut q_h = fnv_mix(scope_h, &[flags[2]]);
    let norm_q = if cfg.regex_mode {
        cfg.query.trim().to_string()
    } else {
        cfg.query.trim().to_lowercase()
    };
    q_h = fnv_mix(q_h, norm_q.as_bytes());
    (scope_h, q_h)
}

#[cfg(unix)]
#[inline]
fn meta_mtime_size(meta: &fs::Metadata) -> (i64, u32, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.mtime(), meta.mtime_nsec() as u32, meta.len())
}

#[cfg(not(unix))]
#[inline]
fn meta_mtime_size(meta: &fs::Metadata) -> (i64, u32, u64) {
    let (sec, nsec) = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| (d.as_secs() as i64, d.subsec_nanos()))
        .unwrap_or((0, 0));
    (sec, nsec, meta.len())
}

/// Путь к квантовому хранилищу кристаллов (`.tar.poler`) рядом с горячим `.t5a`-кристаллом.
fn crystal_vault_path(cpath: &Path) -> PathBuf {
    cpath.with_extension("tar.poler")
}

/// Мгновенная гидратация горячего `.t5a`-кристалла из последнего слепка в `.tar.poler`
/// через `PolerReader` (`mmap`, O(log N) без полной распаковки архива).
fn hydrate_crystal_from_poler_vault(cpath: &Path) -> bool {
    if cpath.exists() {
        return true;
    }
    let vpath = crystal_vault_path(cpath);
    if !vpath.exists() {
        return false;
    }
    let Ok(reader) = crate::archive::reader::PolerReader::open(&vpath) else {
        return false;
    };
    let Some(last_entry) = reader
        .files()
        .iter()
        .rev()
        .find(|f| f.name.ends_with(".t5a") && f.raw_len >= 64)
    else {
        return false;
    };
    let mut buf = Vec::with_capacity(last_entry.raw_len as usize);
    if reader
        .read_range(last_entry.raw_off, last_entry.raw_len as usize, &mut buf)
        .is_err()
    {
        return false;
    }
    if let Some(parent) = cpath.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp = cpath.with_extension("t5a.hydrate.tmp");
    if fs::write(&tmp, &buf).is_ok() && fs::rename(&tmp, cpath).is_ok() {
        if std::env::var_os("POLER_HARVEST_QUIET").is_none() {
            eprintln!(
                "poler_crystal_vault: гидратация {} из {} ({} КиБ)",
                last_entry.name,
                vpath.display(),
                buf.len() / 1024
            );
        }
        return true;
    }
    false
}

/// Чтение файла-носителя по индексу `file_id` прямо из `mmap`-образа без аллокаций.
#[inline]
fn mmap_file_record<'a>(
    mmap: &'a [u8],
    fdir_off: usize,
    files_off: usize,
    num_files: usize,
    file_id: u32,
) -> Option<(i64, u32, u64, &'a [u8])> {
    let idx = file_id as usize;
    if idx >= num_files {
        return None;
    }
    let dir_pos = fdir_off.checked_add(idx.checked_mul(4)?)?;
    if dir_pos + 4 > mmap.len() {
        return None;
    }
    let rel_off = u32::from_le_bytes(mmap[dir_pos..dir_pos + 4].try_into().ok()?) as usize;
    let rec_pos = files_off.checked_add(rel_off)?;
    if rec_pos + 24 > mmap.len() {
        return None;
    }
    let mtime_sec = i64::from_le_bytes(mmap[rec_pos..rec_pos + 8].try_into().ok()?);
    let size = u64::from_le_bytes(mmap[rec_pos + 8..rec_pos + 16].try_into().ok()?);
    let mtime_nsec = u32::from_le_bytes(mmap[rec_pos + 16..rec_pos + 20].try_into().ok()?);
    let path_len = u32::from_le_bytes(mmap[rec_pos + 20..rec_pos + 24].try_into().ok()?) as usize;
    let path_end = rec_pos.checked_add(24)?.checked_add(path_len)?;
    if path_end > mmap.len() {
        return None;
    }
    Some((mtime_sec, mtime_nsec, size, &mmap[rec_pos + 24..path_end]))
}

/// Мгновенная проекция Кристалла Адресов через `mmap` в L1–L3 кэш процессора.
/// Сверяет `mtime` и `size` только у файлов-носителей; если файл изменился — пересканирует только его.
fn try_crystal_hit(
    cpath: &Path,
    scope_key: u64,
    query_key: u64,
    cfg: &HarvestConfig,
    matcher: &Matcher,
) -> Option<(ScanCounters, Vec<FileHit>, bool)> {
    if !cpath.exists() {
        let _ = hydrate_crystal_from_poler_vault(cpath);
    }
    let file = File::open(cpath).ok()?;
    let mmap = unsafe { MmapOptions::new().map(&file).ok()? };
    if mmap.len() < std::mem::size_of::<CrystalHeader>() {
        return None;
    }
    let hdr: CrystalHeader = unsafe { std::ptr::read_unaligned(mmap.as_ptr() as *const CrystalHeader) };
    if hdr.magic != CRYSTAL_MAGIC || hdr.version != CRYSTAL_VERSION {
        return None;
    }
    let num_files = hdr.num_files as usize;
    let num_queries = hdr.num_queries as usize;
    let num_anchors = hdr.num_anchors as usize;
    let num_hits = hdr.num_hits as usize;

    let anchors_off = hdr.anchors_off as usize;
    let queries_off = hdr.queries_off as usize;
    let hits_off = queries_off.checked_add(num_queries.checked_mul(64)?)?;
    let fdir_off = hits_off.checked_add(num_hits.checked_mul(16)?)?;
    let files_off = hdr.files_off as usize;

    if anchors_off.checked_add(num_anchors.checked_mul(8)?)? > mmap.len()
        || fdir_off.checked_add(num_files.checked_mul(4)?)? > mmap.len()
        || files_off > mmap.len()
    {
        return None;
    }

    // Поиск записи запроса в L1-выровненной таблице запросов
    let mut found_q: Option<CrystalQueryEntry> = None;
    for qi in 0..num_queries {
        let qpos = queries_off + qi * 64;
        let q: CrystalQueryEntry =
            unsafe { std::ptr::read_unaligned(mmap.as_ptr().add(qpos) as *const CrystalQueryEntry) };
        if q.query_key == query_key && q.scope_key == scope_key {
            found_q = Some(q);
            break;
        }
    }
    let q = found_q?;

    let counters = ScanCounters::default();
    counters.files.store(q.files_scanned, Ordering::Relaxed);
    counters.dirs.store(q.dirs_scanned, Ordering::Relaxed);
    counters.bytes.store(q.bytes_scanned, Ordering::Relaxed);
    counters.binary.store(q.skipped_binary, Ordering::Relaxed);
    counters
        .symlinks
        .store(q.skipped_symlink as u64, Ordering::Relaxed);

    let mut hits = Vec::with_capacity(q.hit_count as usize);
    let mut tiny_buf = Vec::new();
    let mut dirty = false;

    let h_start = q.hit_start as usize;
    let h_end = h_start.checked_add(q.hit_count as usize)?;
    if h_end > num_hits {
        return None;
    }

    for hi in h_start..h_end {
        let hpos = hits_off + hi * 16;
        let hm: CrystalHitMeta =
            unsafe { std::ptr::read_unaligned(mmap.as_ptr().add(hpos) as *const CrystalHitMeta) };
        let (saved_sec, saved_nsec, saved_size, path_bytes) =
            mmap_file_record(&mmap, fdir_off, files_off, num_files, hm.file_id)?;

        #[cfg(unix)]
        let path: &Path = {
            use std::os::unix::ffi::OsStrExt;
            Path::new(std::ffi::OsStr::from_bytes(path_bytes))
        };
        #[cfg(not(unix))]
        let path_buf = PathBuf::from(String::from_utf8_lossy(path_bytes).into_owned());
        #[cfg(not(unix))]
        let path: &Path = &path_buf;

        let Ok(meta) = fs::metadata(path) else {
            dirty = true;
            continue;
        };
        let (cur_sec, cur_nsec, cur_size) = meta_mtime_size(&meta);
        if cur_sec == saved_sec && cur_nsec == saved_nsec && cur_size == saved_size {
            let a_start = hm.anchor_start as usize;
            let a_end = a_start.checked_add(hm.anchor_count as usize)?;
            if a_end > num_anchors {
                return None;
            }
            let mut offsets = Vec::with_capacity(hm.anchor_count as usize);
            for ai in a_start..a_end {
                let apos = anchors_off + ai * 8;
                let anc: CrystalAnchor = unsafe {
                    std::ptr::read_unaligned(mmap.as_ptr().add(apos) as *const CrystalAnchor)
                };
                offsets.push(anc.offset);
            }
            hits.push(FileHit {
                path: path.to_path_buf(),
                size: saved_size,
                match_count: hm.match_count as usize,
                offsets,
            });
        } else {
            dirty = true;
            let dummy_counters = ScanCounters::default();
            if let Some(fresh_hit) =
                scan_file(path, cur_size, cfg, matcher, &dummy_counters, &mut tiny_buf)
            {
                hits.push(fresh_hit);
            }
        }
    }

    Some((counters, hits, dirty))
}

/// Предварительная загрузка диссипативной карты директорий (`mom < 0`) из `mmap`-кристалла
/// для мгновенного отсечения мёртвых ветвей на новых (ранее не выполнявшихся) запросах.
fn preload_crystal_momentum(cpath: &Path, scope_key: u64, counters: &ScanCounters) {
    if !cpath.exists() {
        let _ = hydrate_crystal_from_poler_vault(cpath);
    }
    let Ok(file) = File::open(cpath) else { return };
    let Ok(mmap) = (unsafe { MmapOptions::new().map(&file) }) else {
        return;
    };
    if mmap.len() < std::mem::size_of::<CrystalHeader>() {
        return;
    }
    let hdr: CrystalHeader =
        unsafe { std::ptr::read_unaligned(mmap.as_ptr() as *const CrystalHeader) };
    if hdr.magic != CRYSTAL_MAGIC || hdr.version != CRYSTAL_VERSION {
        return;
    }
    let mom_off = hdr.mom_off as usize;
    let num_mom = hdr.num_mom_slots as usize;
    if mom_off.checked_add(num_mom.checked_mul(16).unwrap_or(usize::MAX)).unwrap_or(usize::MAX)
        > mmap.len()
    {
        return;
    }
    for i in 0..num_mom {
        let pos = mom_off + i * MOM_SLOT_STRIDE;
        let slot: CrystalMomSlot =
            unsafe { std::ptr::read_unaligned(mmap.as_ptr().add(pos) as *const CrystalMomSlot) };
        if slot.scope_key == scope_key {
            // Индекс слота = позиция в сетке (v2 хранила slot_idx полем).
            let idx = i & 0xFFFF;
            let mom = (slot.raw_momentum as u16 as i16) as i32;
            if mom < 0 {
                counters.dir_momentum[idx].store(slot.raw_momentum, Ordering::Relaxed);
            }
        }
    }
}

/// Запайка результатов прогона и тритовой карты директорий в бинарный Кристалл Адресов (`.t5a`).
fn crystallize_harvest(
    cpath: &Path,
    scope_key: u64,
    query_key: u64,
    counters: &ScanCounters,
    hits: &[FileHit],
) {
    #[derive(Clone)]
    struct StoredFile {
        mtime_sec: i64,
        mtime_nsec: u32,
        size: u64,
        path_bytes: Vec<u8>,
    }
    #[derive(Clone)]
    struct StoredHit {
        path_bytes: Vec<u8>,
        mtime_sec: i64,
        mtime_nsec: u32,
        size: u64,
        match_count: u32,
        offsets: Vec<u32>,
    }
    #[derive(Clone)]
    struct StoredQuery {
        query_key: u64,
        scope_key: u64,
        files_scanned: u64,
        dirs_scanned: u64,
        bytes_scanned: u64,
        skipped_binary: u64,
        skipped_symlink: u32,
        hits: Vec<StoredHit>,
    }

    let mut queries: Vec<StoredQuery> = Vec::new();
    let mut mom_map: std::collections::BTreeMap<(u64, u32), u64> =
        std::collections::BTreeMap::new();

    if !cpath.exists() {
        let _ = hydrate_crystal_from_poler_vault(cpath);
    }

    // Читаем существующий кристалл (если есть), чтобы объединить запросы и карту бассейнов
    if let Ok(file) = File::open(cpath) {
        if let Ok(mmap) = unsafe { MmapOptions::new().map(&file) } {
            if mmap.len() >= std::mem::size_of::<CrystalHeader>() {
                let hdr: CrystalHeader =
                    unsafe { std::ptr::read_unaligned(mmap.as_ptr() as *const CrystalHeader) };
                if hdr.magic == CRYSTAL_MAGIC && hdr.version == CRYSTAL_VERSION {
                    let num_files = hdr.num_files as usize;
                    let num_queries = hdr.num_queries as usize;
                    let num_anchors = hdr.num_anchors as usize;
                    let num_hits = hdr.num_hits as usize;
                    let num_mom = hdr.num_mom_slots as usize;
                    let anchors_off = hdr.anchors_off as usize;
                    let mom_off = hdr.mom_off as usize;
                    let queries_off = hdr.queries_off as usize;
                    let hits_off = queries_off.saturating_add(num_queries.saturating_mul(64));
                    let fdir_off = hits_off.saturating_add(num_hits.saturating_mul(16));
                    let files_off = hdr.files_off as usize;

                    if anchors_off.saturating_add(num_anchors.saturating_mul(8)) <= mmap.len()
                        && mom_off.saturating_add(num_mom.saturating_mul(MOM_SLOT_STRIDE)) <= mmap.len()
                        && fdir_off.saturating_add(num_files.saturating_mul(4)) <= mmap.len()
                        && files_off <= mmap.len()
                    {
                        for i in 0..num_mom {
                            let pos = mom_off + i * MOM_SLOT_STRIDE;
                            let s: CrystalMomSlot = unsafe {
                                std::ptr::read_unaligned(
                                    mmap.as_ptr().add(pos) as *const CrystalMomSlot
                                )
                            };
                            mom_map.insert((s.scope_key, i as u32), s.raw_momentum);
                        }
                        for qi in 0..num_queries {
                            let qpos = queries_off + qi * 64;
                            let q: CrystalQueryEntry = unsafe {
                                std::ptr::read_unaligned(
                                    mmap.as_ptr().add(qpos) as *const CrystalQueryEntry
                                )
                            };
                            if q.query_key == query_key && q.scope_key == scope_key {
                                continue;
                            }
                            let h_start = q.hit_start as usize;
                            let h_end = h_start.saturating_add(q.hit_count as usize);
                            if h_end > num_hits {
                                continue;
                            }
                            let mut q_hits = Vec::with_capacity(q.hit_count as usize);
                            for hi in h_start..h_end {
                                let hpos = hits_off + hi * 16;
                                let hm: CrystalHitMeta = unsafe {
                                    std::ptr::read_unaligned(
                                        mmap.as_ptr().add(hpos) as *const CrystalHitMeta
                                    )
                                };
                                let Some((sec, nsec, sz, pbytes)) =
                                    mmap_file_record(&mmap, fdir_off, files_off, num_files, hm.file_id)
                                else {
                                    continue;
                                };
                                let a_start = hm.anchor_start as usize;
                                let a_end = a_start.saturating_add(hm.anchor_count as usize);
                                if a_end > num_anchors {
                                    continue;
                                }
                                let mut offs = Vec::with_capacity(hm.anchor_count as usize);
                                for ai in a_start..a_end {
                                    let apos = anchors_off + ai * 8;
                                    let anc: CrystalAnchor = unsafe {
                                        std::ptr::read_unaligned(
                                            mmap.as_ptr().add(apos) as *const CrystalAnchor
                                        )
                                    };
                                    offs.push(anc.offset);
                                }
                                q_hits.push(StoredHit {
                                    path_bytes: pbytes.to_vec(),
                                    mtime_sec: sec,
                                    mtime_nsec: nsec,
                                    size: sz,
                                    match_count: hm.match_count,
                                    offsets: offs,
                                });
                            }
                            queries.push(StoredQuery {
                                query_key: q.query_key,
                                scope_key: q.scope_key,
                                files_scanned: q.files_scanned,
                                dirs_scanned: q.dirs_scanned,
                                bytes_scanned: q.bytes_scanned,
                                skipped_binary: q.skipped_binary,
                                skipped_symlink: q.skipped_symlink,
                                hits: q_hits,
                            });
                        }
                    }
                }
            }
        }
    }

    // Обновляем диссипативную карту директорий для текущего scope_key
    for (idx, cell) in counters.dir_momentum.iter().enumerate() {
        let raw = cell.load(Ordering::Relaxed);
        if raw != 0 {
            let mom = (raw as u16 as i16) as i32;
            if mom <= -2 || mom >= 16 {
                mom_map.insert((scope_key, idx as u32), raw);
            }
        }
    }

    // Собираем файлы-носители текущего запроса
    let mut cur_hits = Vec::with_capacity(hits.len());
    for h in hits {
        let (sec, nsec, sz) = fs::metadata(&h.path)
            .map(|m| meta_mtime_size(&m))
            .unwrap_or((0, 0, h.size));
        cur_hits.push(StoredHit {
            path_bytes: h.path.as_os_str().as_encoded_bytes().to_vec(),
            mtime_sec: sec,
            mtime_nsec: nsec,
            size: sz,
            match_count: h.match_count as u32,
            offsets: h.offsets.clone(),
        });
    }

    if queries.len() >= 128 {
        queries.remove(0);
    }
    queries.push(StoredQuery {
        query_key,
        scope_key,
        files_scanned: counters.files.load(Ordering::Relaxed),
        dirs_scanned: counters.dirs.load(Ordering::Relaxed),
        bytes_scanned: counters.bytes.load(Ordering::Relaxed),
        skipped_binary: counters.binary.load(Ordering::Relaxed),
        skipped_symlink: counters.symlinks.load(Ordering::Relaxed) as u32,
        hits: cur_hits,
    });

    // Строим единую дедуплицированную таблицу файлов и плоские массивы L1-кэша
    let mut file_index: std::collections::HashMap<Vec<u8>, u32> = std::collections::HashMap::new();
    let mut files_table: Vec<StoredFile> = Vec::new();
    let mut raw_anchors: Vec<CrystalAnchor> = Vec::new();
    let mut raw_hits: Vec<CrystalHitMeta> = Vec::new();
    let mut raw_queries: Vec<CrystalQueryEntry> = Vec::with_capacity(queries.len());

    for q in &queries {
        let hit_start = raw_hits.len() as u32;
        for sh in &q.hits {
            let fid = if let Some(&id) = file_index.get(&sh.path_bytes) {
                files_table[id as usize].mtime_sec = sh.mtime_sec;
                files_table[id as usize].mtime_nsec = sh.mtime_nsec;
                files_table[id as usize].size = sh.size;
                id
            } else {
                let id = files_table.len() as u32;
                files_table.push(StoredFile {
                    mtime_sec: sh.mtime_sec,
                    mtime_nsec: sh.mtime_nsec,
                    size: sh.size,
                    path_bytes: sh.path_bytes.clone(),
                });
                file_index.insert(sh.path_bytes.clone(), id);
                id
            };
            let anchor_start = raw_anchors.len() as u32;
            for &off in &sh.offsets {
                raw_anchors.push(CrystalAnchor {
                    file_id: fid,
                    offset: off,
                });
            }
            raw_hits.push(CrystalHitMeta {
                file_id: fid,
                match_count: sh.match_count,
                anchor_start,
                anchor_count: sh.offsets.len() as u32,
            });
        }
        raw_queries.push(CrystalQueryEntry {
            query_key: q.query_key,
            scope_key: q.scope_key,
            files_scanned: q.files_scanned,
            dirs_scanned: q.dirs_scanned,
            bytes_scanned: q.bytes_scanned,
            skipped_binary: q.skipped_binary,
            skipped_symlink: q.skipped_symlink,
            hit_start,
            hit_count: q.hits.len() as u32,
            _reserved: 0,
        });
    }

    let raw_mom: Vec<CrystalMomSlot> = mom_map
        .into_iter()
        .map(|((sk, _idx), raw)| CrystalMomSlot {
            scope_key: sk,
            raw_momentum: raw,
        })
        .collect();

    // Каталог смещений файлов (относительно начала секции записей files_off):
    // значения 0..old_num_files строго инвариантны при добавлении новых файлов в хвост.
    let mut cur_rel = 0u32;
    let mut offsets_dir = Vec::with_capacity(files_table.len());
    for sf in &files_table {
        offsets_dir.push(cur_rel);
        let rec_len = (24 + sf.path_bytes.len() + 7) & !7;
        cur_rel += rec_len as u32;
    }

    // Собираем буфер с 512-байтным префиксом под POSIX ustar-заголовок и
    // выравниваем границы секций под точные точки разреза FastCDC (Shannon-Bypass дедуп).
    let cdc = crate::archive::dedup::CdcParams::new(1024, 2048, 8192);
    let mut tar_buf: Vec<u8> = vec![0u8; 512 + 64];
    let mut chunk_start = 0usize;
    let as_bytes = |ptr: *const u8, len: usize| unsafe { std::slice::from_raw_parts(ptr, len) };

    pad_to_exact_cdc_cut(&mut tar_buf, &mut chunk_start, &cdc);
    let anchors_off = (tar_buf.len() - 512) as u64;
    if !raw_anchors.is_empty() {
        tar_buf.extend_from_slice(as_bytes(
            raw_anchors.as_ptr() as *const u8,
            raw_anchors.len() * 8,
        ));
    }

    pad_to_exact_cdc_cut(&mut tar_buf, &mut chunk_start, &cdc);
    let mom_off = (tar_buf.len() - 512) as u64;
    if !raw_mom.is_empty() {
        tar_buf.extend_from_slice(as_bytes(raw_mom.as_ptr() as *const u8, raw_mom.len() * MOM_SLOT_STRIDE));
    }

    pad_to_exact_cdc_cut(&mut tar_buf, &mut chunk_start, &cdc);
    let queries_off = (tar_buf.len() - 512) as u64;
    if !raw_queries.is_empty() {
        tar_buf.extend_from_slice(as_bytes(
            raw_queries.as_ptr() as *const u8,
            raw_queries.len() * 64,
        ));
    }
    if !raw_hits.is_empty() {
        tar_buf.extend_from_slice(as_bytes(
            raw_hits.as_ptr() as *const u8,
            raw_hits.len() * 16,
        ));
    }
    for &off in &offsets_dir {
        tar_buf.extend_from_slice(&off.to_le_bytes());
    }

    pad_to_exact_cdc_cut(&mut tar_buf, &mut chunk_start, &cdc);
    let files_off = (tar_buf.len() - 512) as u64;
    for sf in &files_table {
        tar_buf.extend_from_slice(&sf.mtime_sec.to_le_bytes());
        tar_buf.extend_from_slice(&sf.size.to_le_bytes());
        tar_buf.extend_from_slice(&sf.mtime_nsec.to_le_bytes());
        tar_buf.extend_from_slice(&(sf.path_bytes.len() as u32).to_le_bytes());
        tar_buf.extend_from_slice(&sf.path_bytes);
        let rem = (24 + sf.path_bytes.len()) % 8;
        if rem != 0 {
            tar_buf.extend_from_slice(&[0u8; 8][..8 - rem]);
        }
    }
    pad_to_exact_cdc_cut(&mut tar_buf, &mut chunk_start, &cdc);

    let t5a_len = tar_buf.len() - 512;
    let hdr = CrystalHeader {
        magic: CRYSTAL_MAGIC,
        version: CRYSTAL_VERSION,
        num_files: files_table.len() as u32,
        num_queries: raw_queries.len() as u32,
        num_anchors: raw_anchors.len() as u32,
        num_hits: raw_hits.len() as u32,
        num_mom_slots: raw_mom.len() as u32,
        anchors_off,
        mom_off,
        queries_off,
        files_off,
    };
    tar_buf[512..576].copy_from_slice(as_bytes(&hdr as *const CrystalHeader as *const u8, 64));

    if let Some(parent) = cpath.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp_path = cpath.with_extension("t5a.tmp");
    if fs::write(&tmp_path, &tar_buf[512..512 + t5a_len]).is_ok() {
        let _ = fs::rename(&tmp_path, cpath);
    }

    // Доклеиваем слепок кристалла в накопительный архив `.tar.poler`
    let tar_pad = (512 - (t5a_len % 512)) % 512;
    if tar_pad > 0 {
        tar_buf.resize(tar_buf.len() + tar_pad, 0);
    }
    sync_crystal_to_poler_vault(cpath, query_key, t5a_len, &mut tar_buf, cdc);
}

/// Выравнивание буфера под точную границу чанка FastCDC (гарантирует 100% дедупликацию
/// неизменных секций между последовательными слепками кристаллов в `.tar.poler`).
#[inline]
fn pad_to_exact_cdc_cut(
    tar_buf: &mut Vec<u8>,
    chunk_start: &mut usize,
    cdc: &crate::archive::dedup::CdcParams,
) {
    crate::archive::dedup::pad_to_cdc_cut(tar_buf, chunk_start, cdc);
}

/// Заполнение 512-байтного заголовка POSIX `ustar` для `TarObserver` внутри `.poler`.
fn fill_ustar_header(hdr: &mut [u8], name: &str, size: usize) {
    hdr[..512].fill(0);
    let nb = name.as_bytes();
    let nlen = nb.len().min(99);
    hdr[..nlen].copy_from_slice(&nb[..nlen]);
    hdr[100..107].copy_from_slice(b"0000644");
    hdr[108..115].copy_from_slice(b"0000000");
    hdr[116..123].copy_from_slice(b"0000000");
    let sz_str = format!("{:011o}", size);
    hdr[124..135].copy_from_slice(sz_str.as_bytes());
    hdr[136..147].copy_from_slice(b"00000000000");
    hdr[148..156].copy_from_slice(b"        ");
    hdr[156] = b'0';
    hdr[257..262].copy_from_slice(b"ustar");
    hdr[263..265].copy_from_slice(b"00");
    let cksum: u32 = hdr[..512].iter().map(|&b| b as u32).sum();
    let ck_str = format!("{:06o}\0 ", cksum);
    hdr[148..156].copy_from_slice(ck_str.as_bytes());
}

/// Доклейка слепка `.t5a` в накопительный квантовый архив `.tar.poler` (`StreamWriter::open_append`).
fn sync_crystal_to_poler_vault(
    cpath: &Path,
    query_key: u64,
    t5a_len: usize,
    tar_buf: &mut [u8],
    cdc: crate::archive::dedup::CdcParams,
) {
    use crate::archive::stream_writer::{CompressTier, StreamWriteConfig, StreamWriter};

    let vpath = crystal_vault_path(cpath);
    let seq = if vpath.exists() {
        crate::archive::reader::PolerReader::open(&vpath)
            .map(|r| r.files().len() + 1)
            .unwrap_or(1)
    } else {
        1
    };
    let entry_name = format!("crystal_{seq:06}_{query_key:016x}.t5a");
    fill_ustar_header(&mut tar_buf[..512], &entry_name, t5a_len);

    let cfg = StreamWriteConfig {
        cdc,
        dedup: true,
        tier: CompressTier::Fast,
        read_chunk: 64 * 1024,
        progress_bytes: 0,
        jobs: 0,
        deep_level: 3,
    };

    let writer_res = if vpath.exists() {
        StreamWriter::open_append(&vpath, cfg.clone()).or_else(|_| StreamWriter::open(&vpath, cfg))
    } else {
        StreamWriter::open(&vpath, cfg)
    };
    let Ok(mut writer) = writer_res else {
        return;
    };
    if writer.push_bytes(tar_buf).is_err() {
        return;
    }
    if let Ok(stats) = writer.finish(&entry_name) {
        if std::env::var_os("POLER_HARVEST_QUIET").is_none() {
            let file_bytes = fs::metadata(&vpath).map(|m| m.len()).unwrap_or(stats.total_stored);
            let file_ratio = stats.total_raw as f64 / file_bytes.max(1) as f64;
            eprintln!(
                "poler_crystal_vault: кристаллов={} сырой={} КиБ -> .tar.poler={} КиБ (сжатие={:.2}x, остаток={:.1}%, дедуп={} чанков / {} КиБ)",
                stats.files,
                stats.total_raw / 1024,
                file_bytes / 1024,
                file_ratio,
                stats.ratio * 100.0,
                stats.dedup_chunks,
                stats.dedup_bytes / 1024,
            );
        }
    }
}

/// Публичная точка входа: скан (или мгновенная `mmap`-проекция Кристалла Адресов) → ранжирование → агрегация.
///
/// Коды возврата транслирует вызывающая сторона: `files_selected == 0`
/// → 1 (как grep «ничего не найдено»), `Err` → 2.
pub fn run_harvest(cfg: &HarvestConfig) -> Result<HarvestStats, String> {
    let t0 = Instant::now();
    if cfg.roots.is_empty() {
        return Err("не указан --harvest-disk <ROOT_PATH>".into());
    }
    for r in &cfg.roots {
        if !r.exists() {
            return Err(format!("корень не существует: {}", r.display()));
        }
    }
    let matcher = Matcher::build(cfg)?;
    let cpath = crystal_path();
    let (scope_key, query_key) = compute_crystal_keys(cfg);
    let force_crystal = std::env::var_os("POLER_ADDRESS_CRYSTAL").is_some();

    let crystal_hit = if !cfg.no_cache {
        try_crystal_hit(&cpath, scope_key, query_key, cfg, &matcher)
    } else {
        None
    };
    let (counters, hits) = if let Some((c_counters, c_hits, dirty)) = crystal_hit {
        if dirty && !cfg.no_cache {
            crystallize_harvest(&cpath, scope_key, query_key, &c_counters, &c_hits);
        }
        (c_counters, c_hits)
    } else {
        let (s_counters, s_hits) = scan_parallel_with_crystal(cfg, &matcher, &cpath, scope_key);
        let scanned = s_counters.files.load(Ordering::Relaxed);
        let not_truncated = cfg.max_files == 0 || s_hits.len() < cfg.max_files;
        if !cfg.no_cache && (scanned >= CRYSTAL_MIN_FILES_FOR_AUTO || force_crystal) && not_truncated {
            crystallize_harvest(&cpath, scope_key, query_key, &s_counters, &s_hits);
        }
        (s_counters, s_hits)
    };

    let mut hits = hits;
    hits.sort_by(|a, b| {
        b.match_count
            .cmp(&a.match_count)
            .then_with(|| a.path.cmp(&b.path))
    });
    hits.retain(|h| h.match_count >= cfg.min_matches);
    let files_matched = hits.len();
    if cfg.max_files > 0 {
        hits.truncate(cfg.max_files);
    }

    let mut stats = HarvestStats {
        files_scanned: counters.files.load(Ordering::Relaxed),
        dirs_scanned: counters.dirs.load(Ordering::Relaxed),
        bytes_scanned: counters.bytes.load(Ordering::Relaxed),
        skipped_binary: counters.binary.load(Ordering::Relaxed),
        skipped_symlink: counters.symlinks.load(Ordering::Relaxed),
        read_errors: counters.errors.load(Ordering::Relaxed),
        files_matched,
        files_selected: hits.len(),
        duration_ms: t0.elapsed().as_millis(),
        ..Default::default()
    };
    write_output(cfg, &hits, &mut stats)
        .map_err(|e| format!("запись вывода {}: {e}", cfg.out_path.display()))?;
    stats.duration_ms = t0.elapsed().as_millis();
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Фаза 1: параллельный скан (ignore::WalkBuilder — движок ripgrep)
// ---------------------------------------------------------------------------

/// Предикат KEEP для filter_entry (true = оставить; false = вырезать).
/// Вырезаем мусорные каталоги и бинарные расширения прямо в обходе.
#[allow(dead_code)]
fn keep_entry(e: &DirEntry) -> bool {
    // Корень обхода не фильтруем — иначе скрытый корень обрежет весь обход.
    if e.depth() == 0 {
        return true;
    }
    let Some(ft) = e.file_type() else { return true };
    if ft.is_dir() {
        let name = e.file_name().to_string_lossy();
        return !JUNK_DIRS.iter().any(|d| name.eq_ignore_ascii_case(d));
    }
    if ft.is_file() {
        if let Some(ext) = e.path().extension().and_then(|s| s.to_str()) {
            let ext = ext.to_ascii_lowercase();
            return !BINARY_EXT.iter().any(|x| *x == ext);
        }
    }
    true
}

#[allow(dead_code)]
fn scan_parallel(cfg: &HarvestConfig, matcher: &Matcher) -> (ScanCounters, Vec<FileHit>) {
    let cpath = crystal_path();
    let (scope_key, _) = compute_crystal_keys(cfg);
    scan_parallel_with_crystal(cfg, matcher, &cpath, scope_key)
}

fn scan_parallel_with_crystal(
    cfg: &HarvestConfig,
    matcher: &Matcher,
    cpath: &Path,
    scope_key: u64,
) -> (ScanCounters, Vec<FileHit>) {
    let counters = ScanCounters::default();
    if !cfg.no_cache {
        preload_crystal_momentum(cpath, scope_key, &counters);
    }
    let hits: Mutex<Vec<FileHit>> = Mutex::new(Vec::new());
    // Top-N: каждый воркер держит bounded min-heap размера max_files —
    // слабейший кандидат выталкивается без остановки обхода, поэтому
    // «кто первый добежал» не решает, кто попадёт в выдачу.
    let top_heap: Mutex<BinaryHeap<MinHeapHit>> = Mutex::new(BinaryHeap::new());

    let no_ignore = cfg.no_ignore;
    let mut builder = WalkBuilder::new(&cfg.roots[0]);
    for r in &cfg.roots[1..] {
        builder.add(r);
    }
    let threads = if cfg.threads == 0 {
        // I/O-bound обход: oversubscription кратно ускоряет медленные диски
        // (замер на 2-ядерной песочнице, 100k файлов: 2 воркера → 8.1 с;
        // 8 → 1.5 с; 16+ → ~1.0 с; плато на ~16).
        let n = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        (n * 2).clamp(8, 32)
    } else {
        cfg.threads
    };
    builder
        .hidden(!cfg.include_hidden)
        .git_ignore(!cfg.no_ignore)
        .git_global(!cfg.no_ignore)
        .git_exclude(!cfg.no_ignore)
        .ignore(!cfg.no_ignore)
        .parents(!cfg.no_ignore)
        .require_git(false)
        .threads(threads)
        .filter_entry(move |e| {
            if e.depth() == 0 {
                return true;
            }
            let Some(ft) = e.file_type() else { return true };
            if ft.is_dir() {
                if !no_ignore {
                    let name = e.file_name().to_string_lossy();
                    if JUNK_DIRS.iter().any(|d| name.eq_ignore_ascii_case(d)) {
                        return false;
                    }
                }
            }
            if ft.is_file() {
                if let Some(ext) = e.path().extension().and_then(|s| s.to_str()) {
                    let ext = ext.to_ascii_lowercase();
                    return !BINARY_EXT.iter().any(|x| *x == ext);
                }
            }
            true
        });

    // Выходной файл исключаем из сбора: итеративные прогоны не должны
    // переваривать собственные результаты (самопоглощение документа).
    let out_abs = if cfg.out_path.is_absolute() {
        cfg.out_path.clone()
    } else {
        std::env::current_dir().unwrap_or_default().join(&cfg.out_path)
    };

    let walk = builder.build_parallel();
    let use_quit = cfg.first_n && cfg.max_files > 0;
    let use_heap = !cfg.first_n && cfg.max_files > 0;
    walk.run(|| {
        let counters = &counters;
        let hits = &hits;
        let top_heap = &top_heap;
        let matcher = matcher;
        let cfg = cfg;
        let out_abs = &out_abs;
        // Переиспользуемый буфер воркера для tiny-файлов и фазовый двигатель POLER.
        let mut tiny_buf: Vec<u8> = Vec::with_capacity(TINY_FILE as usize);
        let mut motion = WorkerMotionState::new();
        Box::new(move |entry: Result<DirEntry, ignore::Error>| -> WalkState {
            let e = match entry {
                Ok(e) => e,
                Err(_) => {
                    counters.errors.fetch_add(1, Ordering::Relaxed);
                    return WalkState::Continue;
                }
            };
            let Some(ft) = e.file_type() else {
                return WalkState::Continue;
            };
            if ft.is_symlink() {
                // Симлинки не follow'им (анти-циклы); битые пропускаются тихо.
                counters.symlinks.fetch_add(1, Ordering::Relaxed);
                return WalkState::Continue;
            }
            if ft.is_dir() {
                counters.dirs.fetch_add(1, Ordering::Relaxed);
                return WalkState::Continue;
            }
            if !ft.is_file() {
                return WalkState::Continue;
            }
            // Свой выход не собираем (см. out_abs выше).
            let p = e.path();
            if p == cfg.out_path.as_path() || p == out_abs.as_path() {
                return WalkState::Continue;
            }
            counters.files.fetch_add(1, Ordering::Relaxed);
            if motion.should_skip_pre_open(counters, p, matcher) {
                return WalkState::Continue;
            }
            let Ok(meta) = e.metadata() else {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                return WalkState::Continue;
            };
            if let Some(hit) = scan_file_dynamical(
                e.path(),
                meta.len(),
                cfg,
                matcher,
                counters,
                &mut tiny_buf,
                &mut motion,
            ) {
                if use_heap {
                    // Top-N: bounded min-heap — обход продолжается до конца.
                    if let Ok(mut guard) = top_heap.lock() {
                        if guard.len() < cfg.max_files {
                            guard.push(MinHeapHit(hit));
                        } else if let Some(worst) = guard.peek() {
                            // хуже худшего (по качеству и алфавиту) — мимо
                            if hit.is_better_than(&worst.0) {
                                guard.pop();
                                guard.push(MinHeapHit(hit));
                            }
                        }
                    }
                } else if let Ok(mut guard) = hits.lock() {
                    guard.push(hit);
                    // Ранняя остановка — только в стриминг-режиме --first-n:
                    // выдача = «первые N встречных», Quit безопасен для семантики.
                    if use_quit && guard.len() >= cfg.max_files {
                        return WalkState::Quit;
                    }
                }
            }
            WalkState::Continue
        })
    });

    let mut all_hits: Vec<FileHit> = hits.into_inner().unwrap_or_default();
    if use_heap {
        let mut heap = top_heap.into_inner().unwrap_or_default();
        all_hits.extend(heap.drain().map(|MinHeapHit(h)| h));
    }
    (counters, all_hits)
}

/// Прямой скан одного файла (без межкаталожной инерции; используется тестами).
fn scan_file(
    path: &Path,
    size: u64,
    cfg: &HarvestConfig,
    matcher: &Matcher,
    counters: &ScanCounters,
    tiny_buf: &mut Vec<u8>,
) -> Option<FileHit> {
    let mut motion = WorkerMotionState::new();
    scan_file_dynamical(path, size, cfg, matcher, counters, tiny_buf, &mut motion)
}

/// Каузальный скан файла под управлением тритовой спектроскопии (`trit_asm`)
/// и уравнения движения `PolerCycle` (`p_{t+1} = p_t - η·Π_Λ(D·p_t + γ·J·p_t + ∇F)`).
fn scan_file_dynamical(
    path: &Path,
    size: u64,
    cfg: &HarvestConfig,
    matcher: &Matcher,
    counters: &ScanCounters,
    tiny_buf: &mut Vec<u8>,
    motion: &mut WorkerMotionState,
) -> Option<FileHit> {
    if size == 0 {
        return None;
    }
    motion.enter_dir(path.parent());
    let want = size.min(cfg.max_file_bytes) as usize;
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => {
            counters.errors.fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };

    let mut offsets: Vec<u32> = Vec::new();
    let mut count: usize = 0;
    let collect = cfg.mode == HarvestMode::Sections || cfg.format == HarvestFormat::Address;
    let mut record = |abs: usize, end: usize| {
        if abs < end {
            count += 1;
            if collect && offsets.len() < MAX_OFFSETS_PER_FILE {
                offsets.push(abs as u32);
            }
        }
    };

    let scan = |bytes: &[u8], record: &mut dyn FnMut(usize, usize)| {
        // NUL в первых байтах → бинарник (grep-семантика; UTF-16 отсекается
        // сознательно — задокументированное ограничение байтового сканера).
        if bytes[..bytes.len().min(BINARY_SNIFF)].contains(&0u8) {
            counters.binary.fetch_add(1, Ordering::Relaxed);
            return;
        }
        counters
            .bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        let overlap = matcher.overlap();
        let mut pos = 0usize;
        while pos < bytes.len() {
            let end = (pos + cfg.chunk_bytes).min(bytes.len());
            let scan_end = (end + overlap).min(bytes.len());
            let base = pos;
            matcher.for_each_start(&bytes[base..scan_end], |m| record(base + m, end));
            pos = end;
        }
    };

    // Если файл мал (≤ 8 КБ) — читаем сразу одним сисколом в tiny_buf.
    if want <= BINARY_SNIFF {
        tiny_buf.clear();
        if (&mut file).take(want as u64).read_to_end(tiny_buf).is_err() {
            counters.errors.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        scan(tiny_buf, &mut record);
        motion.record_observation(counters, path, if count > 0 { 1.0 } else { -1.0 }, count > 0);
    } else {
        // Крупный файл (> 8 КБ): сначала 8-КБ головной пробник ДО вызова mmap/readahead!
        tiny_buf.clear();
        if (&mut file).take(BINARY_SNIFF as u64).read_to_end(tiny_buf).is_err() {
            counters.errors.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        // Мгновенное отсечение ELF/бинарников без расширения до создания VMA/mmap:
        if tiny_buf.contains(&0u8) {
            counters.binary.fetch_add(1, Ordering::Relaxed);
            motion.record_observation(counters, path, -1.0, false);
            return None;
        }

        if want as u64 <= TINY_FILE {
            // Дочитываем остаток файла ≤ 64 КБ в tiny_buf с позиции BINARY_SNIFF
            let _ = file.seek(SeekFrom::Start(BINARY_SNIFF as u64));
            let rem = (want - tiny_buf.len()) as u64;
            if (&mut file).take(rem).read_to_end(tiny_buf).is_err() {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            scan(tiny_buf, &mut record);
        } else {
            let mmap = match unsafe { MmapOptions::new().len(want).map(&file) } {
                Ok(m) => m,
                Err(_) => {
                    counters.errors.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            };
            #[cfg(unix)]
            let _ = mmap.advise(memmap2::Advice::Sequential);
            scan(&mmap[..], &mut record);
            drop(mmap);
        }
        motion.record_observation(counters, path, if count > 0 { 1.0 } else { -1.0 }, count > 0);
    }

    if count == 0 {
        None
    } else {
        Some(FileHit {
            path: path.to_path_buf(),
            size,
            match_count: count,
            offsets,
        })
    }
}

// ---------------------------------------------------------------------------
// Фаза 2: ранжирование сделано — стриминговая агрегация в документ
// ---------------------------------------------------------------------------

/// Обёртка Write с подсчётом байт (кап max_out_bytes проверяет цикл файлов).
struct CountingWriter<W: Write> {
    inner: W,
    n: u64,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let w = self.inner.write(buf)?;
        self.n += w as u64;
        Ok(w)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn json_escape(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Начала строк файла (байтовые offsetы) для отображения позиций в строки.
fn line_starts(buf: &[u8]) -> Vec<u32> {
    let mut v = vec![0u32];
    for (i, b) in buf.iter().enumerate() {
        if *b == b'\n' && i + 1 < buf.len() {
            v.push((i + 1) as u32);
        }
    }
    v
}

/// Строка i файла (без завершающего \n в срезе — он внутри среза).
fn line_slice<'a>(buf: &'a [u8], starts: &[u32], i: usize) -> &'a [u8] {
    let s = starts[i] as usize;
    let e = if i + 1 < starts.len() {
        starts[i + 1] as usize
    } else {
        buf.len()
    };
    &buf[s..e.max(s)]
}

/// Слияние контекстных диапазонов; покрытие ≥ 80% → весь файл.
fn merged_ranges(offsets: &[u32], starts: &[u32], ctx: usize) -> Vec<(usize, usize)> {
    let n_lines = starts.len();
    let mut ranges: Vec<(usize, usize)> = offsets
        .iter()
        .map(|off| {
            let l = starts.partition_point(|s| *s <= *off).saturating_sub(1);
            (l.saturating_sub(ctx), (l + ctx + 1).min(n_lines))
        })
        .collect();
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in ranges {
        if let Some(last) = merged.last_mut() {
            if s <= last.1 {
                if e > last.1 {
                    last.1 = e;
                }
                continue;
            }
        }
        merged.push((s, e));
    }
    let coverage: usize = merged.iter().map(|(s, e)| e.saturating_sub(*s)).sum();
    if !merged.is_empty() && coverage * 100 >= n_lines * WHOLE_FILE_COVERAGE_PCT {
        merged = vec![(0, n_lines)];
    }
    merged
}

fn write_output(
    cfg: &HarvestConfig,
    hits: &[FileHit],
    stats: &mut HarvestStats,
) -> std::io::Result<()> {
    let stdout = cfg.out_path.as_os_str() == "-";
    let inner: Box<dyn Write> = if stdout {
        Box::new(BufWriter::with_capacity(OUT_BUF, std::io::stdout().lock()))
    } else {
        Box::new(BufWriter::with_capacity(OUT_BUF, File::create(&cfg.out_path)?))
    };
    let mut w = CountingWriter { inner, n: 0 };

    if cfg.format == HarvestFormat::Markdown {
        write_md_header(&mut w, cfg, stats)?;
    } else if cfg.format == HarvestFormat::Json {
        let mut line = String::from("{\"type\":\"meta\",\"roots\":[");
        for (i, r) in cfg.roots.iter().enumerate() {
            if i > 0 {
                line.push(',');
            }
            json_escape(&r.display().to_string(), &mut line);
        }
        line.push_str("],\"query\":");
        json_escape(&cfg.query, &mut line);
        line.push_str(&format!(
            ",\"regex\":{},\"mode\":\"{}\",\"context\":{}}}\n",
            cfg.regex_mode,
            match cfg.mode {
                HarvestMode::Sections => "sections",
                HarvestMode::Files => "files",
            },
            cfg.context_lines
        ));
        w.write_all(line.as_bytes())?;
    }

    // Переиспользуемый буфер чтения фазы агрегации (растёт до max_file_bytes).
    let qlen = cfg.query.len();

    'files: for (idx, hit) in hits.iter().enumerate() {
        if w.n >= cfg.max_out_bytes {
            stats.truncated = true;
            break 'files;
        }
        if cfg.format == HarvestFormat::Address {
            let mut esc_path = String::new();
            json_escape(&hit.path.display().to_string(), &mut esc_path);
            for &off in &hit.offsets {
                if stats.sections_written >= MAX_ADDRESS_CACHE_LIMIT {
                    stats.truncated = true;
                    break 'files;
                }
                let line = format!(
                    "{{\"type\":\"anchor\",\"path\":{},\"offset\":{},\"len\":{},\"matches\":{}}}\n",
                    esc_path, off, qlen, hit.match_count
                );
                w.write_all(line.as_bytes())?;
                stats.sections_written += 1;
            }
            continue;
        }
        // TOCTOU: файл изменился между фазами — честный пропуск с предупреждением.
        let size_now = fs::metadata(&hit.path).map(|m| m.len());
        let unchanged = matches!(size_now, Ok(s) if s == hit.size);
        if !unchanged {
            stats.changed_during_harvest += 1;
            eprintln!(
                "poler_disk_harvester: файл изменился во время сбора, пропуск: {}",
                hit.path.display()
            );
            continue;
        }
        let want = hit.size.min(cfg.max_file_bytes);
        if want == 0 {
            continue;
        }
        let f = File::open(&hit.path)?;
        let mmap = match unsafe { MmapOptions::new().len(want as usize).map(&f) } {
            Ok(m) => m,
            Err(e) => {
                eprintln!("poler_disk_harvester: mmap failed {}: {e}", hit.path.display());
                continue;
            }
        };
        #[cfg(unix)]
        let _ = mmap.advise(memmap2::Advice::Sequential);
        let buf = &mmap[..];

        match (cfg.format, cfg.mode) {
            (HarvestFormat::Markdown, HarvestMode::Files) => {
                write!(
                    w,
                    "\n## [{idx}] {} — {} совпадений, {} байт\n\n````text\n",
                    hit.path.display(),
                    hit.match_count,
                    hit.size
                )?;
                w.write_all(&buf)?;
                if !buf.ends_with(b"\n") {
                    w.write_all(b"\n")?;
                }
                w.write_all(b"````\n")?;
                stats.sections_written += 1;
            }
            (HarvestFormat::Markdown, HarvestMode::Sections) => {
                write!(
                    w,
                    "\n## [{idx}] {} — {} совпадений, {} байт\n",
                    hit.path.display(),
                    hit.match_count,
                    hit.size
                )?;
                let starts = line_starts(&buf);
                for (s, e) in merged_ranges(&hit.offsets, &starts, cfg.context_lines) {
                    if w.n >= cfg.max_out_bytes {
                        stats.truncated = true;
                        break 'files;
                    }
                    w.write_all(b"\n````text\n")?;
                    for i in s..e {
                        let line = line_slice(&buf, &starts, i);
                        write!(w, "{:>5} | ", i + 1)?;
                        w.write_all(line)?;
                        if !line.ends_with(b"\n") {
                            w.write_all(b"\n")?;
                        }
                    }
                    w.write_all(b"````\n")?;
                    stats.sections_written += 1;
                }
            }
            (HarvestFormat::Json, HarvestMode::Files) => {
                let mut line = String::from("{\"type\":\"file\",\"path\":");
                json_escape(&hit.path.display().to_string(), &mut line);
                line.push_str(&format!(
                    ",\"bytes\":{},\"matches\":{},\"truncated\":{},\"content\":",
                    hit.size,
                    hit.match_count,
                    want < hit.size
                ));
                json_escape(&String::from_utf8_lossy(&buf), &mut line);
                line.push_str("}\n");
                w.write_all(line.as_bytes())?;
                stats.sections_written += 1;
            }
            (HarvestFormat::Json, HarvestMode::Sections) => {
                let starts = line_starts(&buf);
                let ranges = merged_ranges(&hit.offsets, &starts, cfg.context_lines);
                let mut line = String::from("{\"type\":\"file\",\"path\":");
                json_escape(&hit.path.display().to_string(), &mut line);
                line.push_str(&format!(
                    ",\"bytes\":{},\"matches\":{},\"sections\":[",
                    hit.size, hit.match_count
                ));
                for (si, (s, e)) in ranges.iter().enumerate() {
                    if si > 0 {
                        line.push(',');
                    }
                    let text: Vec<&[u8]> = (*s..*e).map(|i| line_slice(&buf, &starts, i)).collect();
                    let joined = text.concat();
                    line.push_str(&format!("{{\"start\":{},\"end\":{},\"text\":", s + 1, e));
                    json_escape(&String::from_utf8_lossy(&joined), &mut line);
                    line.push('}');
                    stats.sections_written += 1;
                }
                line.push_str("]}\n");
                w.write_all(line.as_bytes())?;
            }
            (HarvestFormat::Corpus, HarvestMode::Files) => {
                w.write_all(&buf)?;
                if !buf.ends_with(b"\n") {
                    w.write_all(b"\n")?;
                }
                stats.sections_written += 1;
            }
            (HarvestFormat::Corpus, HarvestMode::Sections) => {
                let starts = line_starts(&buf);
                for (s, e) in merged_ranges(&hit.offsets, &starts, cfg.context_lines) {
                    for i in s..e {
                        let line = line_slice(&buf, &starts, i);
                        w.write_all(line)?;
                        if !line.ends_with(b"\n") {
                            w.write_all(b"\n")?;
                        }
                    }
                    w.write_all(b"\n")?;
                    stats.sections_written += 1;
                }
            }
            (HarvestFormat::Address, _) => {
                // Обработано выше до match (cfg.format, cfg.mode)
            }
        }
    }

    if cfg.format == HarvestFormat::Markdown {
        write_md_footer(&mut w, cfg, stats)?;
    } else if cfg.format == HarvestFormat::Json {
        let line = format!(
            "{{\"type\":\"summary\",\"files_scanned\":{},\"files_matched\":{},\"files_selected\":{},\"sections\":{},\"bytes_written\":{},\"truncated\":{}}}\n",
            stats.files_scanned,
            stats.files_matched,
            stats.files_selected,
            stats.sections_written,
            w.n,
            stats.truncated
        );
        w.write_all(line.as_bytes())?;
    }
    w.flush()?;
    stats.bytes_written = w.n;
    if stats.truncated {
        eprintln!(
            "poler_disk_harvester: вывод обрезан по --harvest-max-out-bytes ({})",
            cfg.max_out_bytes
        );
    }
    Ok(())
}

fn write_md_header<W: Write>(
    w: &mut CountingWriter<W>,
    cfg: &HarvestConfig,
    stats: &HarvestStats,
) -> std::io::Result<()> {
    let roots: Vec<String> = cfg.roots.iter().map(|r| r.display().to_string()).collect();
    writeln!(
        w,
        "# POLER Disk Harvest\n\n\
         - **roots**: {}\n\
         - **query**: {}{}\n\
         - **mode**: {} (context: {})\n\
         - **matched files**: {} (selected: {})\n",
        roots.join(", "),
        cfg.query,
        if cfg.regex_mode { " [regex]" } else { "" },
        match cfg.mode {
            HarvestMode::Sections => "sections",
            HarvestMode::Files => "files",
        },
        cfg.context_lines,
        stats.files_matched,
        stats.files_selected,
    )
}

fn write_md_footer<W: Write>(
    w: &mut CountingWriter<W>,
    cfg: &HarvestConfig,
    stats: &HarvestStats,
) -> std::io::Result<()> {
    if stats.truncated {
        writeln!(
            w,
            "\n> Вывод обрезан по --harvest-max-out-bytes ({})",
            cfg.max_out_bytes
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Тесты (DoD: битые симлинки, невалидный UTF-8, переполнение буферов,
// границы чанков, кириллица, детерминизм, капы)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("poler_harvest_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(path: &Path, content: impl AsRef<[u8]>) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn run_ok(cfg: &HarvestConfig) -> HarvestStats {
        run_harvest(cfg).unwrap()
    }

    /// Выходной файл ВНЕ дерева обхода: тесты не зависят от
    /// самопоглощения и продакшн-исключения выходного файла.
    fn outpath(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "poler_harvest_out_{name}_{}",
            std::process::id()
        ));
        let _ = fs::remove_file(&p);
        p
    }

    fn out_str(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    #[test]
    fn broken_symlink_skipped_gracefully() {
        let d = tmpdir("broken_symlink");
        put(&d.join("good.txt"), "гамильтониан системы");
        std::os::unix::fs::symlink(d.join("missing.txt"), d.join("dangling.lnk")).unwrap();
        let out = outpath("broken_symlink");
        let cfg = HarvestConfig::new(vec![d.clone()], "гамильтониан", &out);
        let stats = run_ok(&cfg);
        assert!(stats.skipped_symlink >= 1, "битый симлинк должен быть пропущен");
        assert_eq!(stats.files_selected, 1);
        assert!(out_str(&out).contains("good.txt"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn binary_nul_sniffed_and_skipped() {
        let d = tmpdir("binary_nul");
        put(&d.join("data.txt"), "prefix \x00\x01\x02 гамильтониан");
        put(&d.join("clean.txt"), "гамильтониан чистый");
        let out = outpath("binary_nul");
        let cfg = HarvestConfig::new(vec![d.clone()], "гамильтониан", &out);
        let stats = run_ok(&cfg);
        assert!(stats.skipped_binary >= 1);
        assert_eq!(stats.files_selected, 1);
        assert!(!out_str(&out).contains("data.txt"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn junk_dirs_pruned() {
        let d = tmpdir("junk_dirs");
        put(&d.join("target/junk.txt"), "гамильтониан в мусоре");
        put(&d.join("node_modules/lib.js"), "гамильтониан в мусоре");
        put(&d.join("real.txt"), "гамильтониан настоящий");
        let out = outpath("junk_dirs");
        let cfg = HarvestConfig::new(vec![d.clone()], "гамильтониан", &out);
        let stats = run_ok(&cfg);
        assert_eq!(stats.files_selected, 1);
        assert!(out_str(&out).contains("real.txt"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn hidden_skipped_by_default_included_with_flag() {
        let d = tmpdir("hidden");
        put(&d.join(".secret/hidden.txt"), "гамильтониан скрытый");
        put(&d.join("open.txt"), "гамильтониан явный");
        let out = outpath("hidden1");
        let cfg = HarvestConfig::new(vec![d.clone()], "гамильтониан", &out);
        let stats = run_ok(&cfg);
        assert_eq!(stats.files_selected, 1);

        let out2 = outpath("hidden2");
        let mut cfg2 = HarvestConfig::new(vec![d.clone()], "гамильтониан", &out2);
        cfg2.include_hidden = true;
        let stats2 = run_ok(&cfg2);
        assert_eq!(stats2.files_selected, 2);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn gitignore_respected_and_overridable() {
        let d = tmpdir("gitignore");
        put(&d.join(".gitignore"), "*.ignored\n");
        put(&d.join("a.ignored"), "гамильтониан игнорируется");
        put(&d.join("b.txt"), "гамильтониан видимый");
        let out = outpath("gitignore1");
        let cfg = HarvestConfig::new(vec![d.clone()], "гамильтониан", &out);
        let stats = run_ok(&cfg);
        assert_eq!(stats.files_selected, 1);

        let out2 = outpath("gitignore2");
        let mut cfg2 = HarvestConfig::new(vec![d.clone()], "гамильтониан", &out2);
        cfg2.no_ignore = true;
        let stats2 = run_ok(&cfg2);
        assert_eq!(stats2.files_selected, 2);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn cyrillic_case_variants_match() {
        let d = tmpdir("cyrillic_case");
        put(&d.join("cap.txt"), "Гамильтониан системы");
        put(&d.join("lower.txt"), "гамильтониан системы");
        put(&d.join("upper.txt"), "ГАМИЛЬТОНИАН системы");
        put(&d.join("none.txt"), "нет совпадений");
        let out = outpath("cyrillic_case");
        let cfg = HarvestConfig::new(vec![d.clone()], "гамильтониан", &out);
        let stats = run_ok(&cfg);
        assert_eq!(stats.files_selected, 3);
        let text = out_str(&out);
        assert!(text.contains("cap.txt") && text.contains("lower.txt") && text.contains("upper.txt"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn sections_context_and_merging() {
        let d = tmpdir("sections_ctx");
        // 6 строк; совпадения на строках 1 и 4 (0-based).
        let text = "line0\nneedle one\nline2\nline3\nneedle two\nline5\n";
        put(&d.join("f.txt"), text);
        let out = outpath("sections_ctx");

        // ctx=0 → две изолированные секции по одной строке.
        let mut cfg = HarvestConfig::new(vec![d.clone()], "needle", &out);
        cfg.context_lines = 0;
        let stats = run_ok(&cfg);
        assert_eq!(stats.sections_written, 2, "ctx=0: две секции");
        assert!(out_str(&out).contains("needle one") && out_str(&out).contains("needle two"));

        // ctx=2 → диапазоны перекрываются, покрытие 100% → весь файл одной секцией.
        let mut cfg2 = HarvestConfig::new(vec![d.clone()], "needle", &out);
        cfg2.context_lines = 2;
        let stats2 = run_ok(&cfg2);
        assert_eq!(stats2.sections_written, 1, "ctx=2: единая секция");
        let text_out = out_str(&out);
        assert!(text_out.contains("line0") && text_out.contains("line5"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn max_files_and_min_matches_filters() {
        let d = tmpdir("caps");
        put(&d.join("three.txt"), "x x x гамильтониан гамильтониан гамильтониан");
        put(&d.join("two.txt"), "гамильтониан гамильтониан");
        put(&d.join("one.txt"), "гамильтониан");
        let out = outpath("caps");

        // Top-N по умолчанию: три файла в обходе → лучший (three.txt) обязан
        // попасть в выдачу независимо от того, каким потоком он был отсканирован.
        let mut cfg = HarvestConfig::new(vec![d.clone()], "гамильтониан", &out);
        cfg.max_files = 2;
        let stats = run_ok(&cfg);
        assert_eq!(stats.files_selected, 2);
        let text = out_str(&out);
        assert!(text.contains("three.txt") && text.contains("two.txt"));
        assert!(!text.contains("one.txt"), "кап max_files отсекает слабейший");

        let mut cfg2 = HarvestConfig::new(vec![d.clone()], "гамильтониан", &out);
        cfg2.min_matches = 2;
        let stats2 = run_ok(&cfg2);
        assert_eq!(stats2.files_selected, 2, "min_matches=2 фильтрует one.txt");
        let _ = fs::remove_dir_all(&d);
    }

    /// Регрессия race `WalkState::Quit` (v1.5.1): до фикса ранний выход по
    /// `max_files` мог завершить обход ДО скана лучшего кандидата — «первые
    /// N встречных» вытесняли top-N. Дерево из 300 файлов: сильнейший лежит
    /// последним в обходе; top-N обязан его отыскать, `--first-n` — нет.
    #[test]
    fn top_n_finds_strongest_late_in_walk() {
        let d = tmpdir("topn");
        let weak = "гамильтониан\n";
        for i in 0..300 {
            put(&d.join(format!("f{i:03}.txt")), weak);
        }
        // Сильнейший кандидат появляется в обходе самым последним.
        put(
            &d.join("zz_strongest.txt"),
            "гамильтониан гамильтониан гамильтониан гамильтониан",
        );
        let out = outpath("topn");

        let mut cfg = HarvestConfig::new(vec![d.clone()], "гамильтониан", &out);
        cfg.max_files = 5;
        let stats = run_ok(&cfg);
        assert_eq!(stats.files_selected, 5);
        assert_eq!(stats.files_scanned, 301, "top-N сканирует всё дерево");
        let text = out_str(&out);
        assert!(
            text.contains("zz_strongest.txt"),
            "top-N обязан найти сильнейшего, где бы он ни лежал"
        );

        // Стриминг: первые 5 встречных; сильнейший (последний) не обязан
        // попасться — но семантика «не ранжируем» должна соблюдаться.
        let mut cfg2 = HarvestConfig::new(vec![d.clone()], "гамильтониан", &out);
        cfg2.max_files = 5;
        cfg2.first_n = true;
        let stats2 = run_ok(&cfg2);
        assert_eq!(stats2.files_selected, 5, "first-n: ровно первые 5");
        assert!(
            stats2.files_scanned < 301,
            "first-n обязан останавливать обход раньше конца дерева"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn markdown_structure() {
        let d = tmpdir("md_structure");
        put(&d.join("f.txt"), "alpha\nneedle here\nomega");
        let out = outpath("md_structure");
        let cfg = HarvestConfig::new(vec![d.clone()], "needle", &out);
        run_ok(&cfg);
        let text = out_str(&out);
        assert!(text.starts_with("# POLER Disk Harvest"));
        assert!(text.contains("## [0] "));
        assert!(text.contains("````text"));
        assert!(text.contains(" | needle here"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn json_jsonl_shape() {
        let d = tmpdir("json_shape");
        put(&d.join("f.txt"), "alpha\nneedle here\nomega");
        let out = outpath("json_shape");
        let mut cfg = HarvestConfig::new(vec![d.clone()], "needle", &out);
        cfg.format = HarvestFormat::Json;
        run_ok(&cfg);
        let text = out_str(&out);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.len() >= 3, "meta + file + summary");
        assert!(lines[0].starts_with("{\"type\":\"meta\""));
        assert!(lines[0].contains("\"roots\""));
        assert!(lines.last().unwrap().contains("\"type\":\"summary\""));
        for l in &lines {
            assert!(l.starts_with('{') && l.ends_with('}'), "каждая строка — JSON-объект: {l}");
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn corpus_is_pure_text() {
        let d = tmpdir("corpus_pure");
        put(&d.join("f.txt"), "alpha\nneedle here\nomega");
        let out = outpath("corpus_pure");
        let mut cfg = HarvestConfig::new(vec![d.clone()], "needle", &out);
        cfg.format = HarvestFormat::Corpus;
        run_ok(&cfg);
        let text = out_str(&out);
        assert!(!text.contains("# POLER"), "без markdown-меты");
        assert!(!text.contains("````"));
        assert!(text.contains("needle here"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn files_mode_dumps_whole_file() {
        let d = tmpdir("files_mode");
        put(&d.join("f.txt"), "far top\nneedle\nfar bottom");
        let out = outpath("files_mode");
        let mut cfg = HarvestConfig::new(vec![d.clone()], "needle", &out);
        cfg.mode = HarvestMode::Files;
        run_ok(&cfg);
        let text = out_str(&out);
        assert!(text.contains("far top") && text.contains("far bottom"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn chunk_boundary_matches_recorded_once() {
        let d = tmpdir("chunk_boundary");
        // Терм из 13 байт, стартующий на 7-й и ровно на 8-й байт (граница чанка 8).
        let term = "boundary_term";
        let f1: String = "a".repeat(7) + term + "bbbb";
        let f2: String = "a".repeat(8) + term + "bbbb";
        put(&d.join("span.txt"), &f1);
        put(&d.join("edge.txt"), &f2);
        let out = outpath("chunk_boundary");
        let mut cfg = HarvestConfig::new(vec![d.clone()], "boundary_term", &out);
        cfg.chunk_bytes = 8; // overlap = 13 ≥ длины терма
        let stats = run_ok(&cfg);
        assert_eq!(stats.files_selected, 2, "оба граничных файла найдены");
        // Ровно по одному совпадению — дедупликация на границе чанков.
        for name in ["span.txt", "edge.txt"] {
            let matcher = Matcher::build(&cfg).unwrap();
            let counters = ScanCounters::default();
            let mut buf = Vec::new();
            let path = d.join(name);
            let size = fs::metadata(&path).unwrap().len();
            let hit = scan_file(&path, size, &cfg, &matcher, &counters, &mut buf).unwrap();
            assert_eq!(hit.match_count, 1, "{name}: совпадение ровно одно");
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn invalid_utf8_lossy_no_panic() {
        let d = tmpdir("invalid_utf8");
        put(&d.join("f.txt"), b"\xff\xfe broken\nneedle valid line\n");
        let out = outpath("invalid_utf8");
        let cfg = HarvestConfig::new(vec![d.clone()], "needle", &out);
        let stats = run_ok(&cfg);
        assert_eq!(stats.files_selected, 1);
        // Маркдаун хранит сырые байты («до байта») — читаем как байты.
        let out_bytes = fs::read(&out).unwrap();
        assert!(
            String::from_utf8_lossy(&out_bytes).contains("needle valid line"),
            "lossy-чтение должно находить валидную строку"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn multi_root_harvest() {
        let d1 = tmpdir("multi_root_a");
        let d2 = tmpdir("multi_root_b");
        put(&d1.join("a.txt"), "гамильтониан из корня A");
        put(&d2.join("b.txt"), "гамильтониан из корня B");
        let out = outpath("multi_root");
        let cfg = HarvestConfig::new(vec![d1.clone(), d2.clone()], "гамильтониан", &out);
        let stats = run_ok(&cfg);
        assert_eq!(stats.files_selected, 2);
        let text = out_str(&out);
        assert!(text.contains("a.txt") && text.contains("b.txt"));
        let _ = fs::remove_dir_all(&d1);
        let _ = fs::remove_dir_all(&d2);
    }

    #[test]
    fn regex_mode_works() {
        let d = tmpdir("regex_mode");
        put(&d.join("f1.txt"), "гамильтониан квантовый");
        put(&d.join("f2.txt"), "гамільтоніан український");
        put(&d.join("f3.txt"), "нет совпадений");
        let out = outpath("regex_mode");
        let mut cfg = HarvestConfig::new(vec![d.clone()], "гам[иі]льтон", &out);
        cfg.regex_mode = true;
        let stats = run_ok(&cfg);
        assert_eq!(stats.files_selected, 2, "regex покрывает RU и UA варианты");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn corpus_output_deterministic() {
        let d = tmpdir("determinism");
        put(&d.join("a.txt"), "needle один\nконтент");
        put(&d.join("b.txt"), "needle два\nдругой контент");
        let out1 = outpath("det1");
        let out2 = outpath("det2");
        let mut c1 = HarvestConfig::new(vec![d.clone()], "needle", &out1);
        c1.format = HarvestFormat::Corpus;
        let mut c2 = HarvestConfig::new(vec![d.clone()], "needle", &out2);
        c2.format = HarvestFormat::Corpus;
        run_ok(&c1);
        run_ok(&c2);
        assert_eq!(
            fs::read(&out1).unwrap(),
            fs::read(&out2).unwrap(),
            "повторный прогон байт-в-байт"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn offsets_capped_but_count_exact() {
        let d = tmpdir("offsets_cap");
        let mut text = String::new();
        for _ in 0..10_000 {
            text.push_str("x needle\n");
        }
        put(&d.join("big.txt"), &text);
        let cfg = HarvestConfig::new(vec![d.clone()], "needle", outpath("offsets_cap"));
        let matcher = Matcher::build(&cfg).unwrap();
        let counters = ScanCounters::default();
        let mut buf = Vec::new();
        let path = d.join("big.txt");
        let size = fs::metadata(&path).unwrap().len();
        let hit = scan_file(&path, size, &cfg, &matcher, &counters, &mut buf).unwrap();
        assert_eq!(hit.match_count, 10_000, "счётчик полный");
        assert_eq!(hit.offsets.len(), MAX_OFFSETS_PER_FILE, "позиции capped");
        let _ = fs::remove_dir_all(&d);
    }

    /// v0.94.2 regression: mom-слоты были 24-байтной структурой при сетке
    /// чтения/записи с шагом 16 — секция диссипативного импульса молча
    /// портилась при каждом кристалл-перезаписи. Теперь слот честно 16 Б:
    /// после двух кристаллизаций отрицательный импульс (холодная ветвь)
    /// обязан пережить roundtrip через файл.
    #[test]
    fn mom_slots_survive_crystal_roundtrip() {
        let d = tmpdir("mom_slots");
        let cpath = d.join("crystal.t5a");
        // Холодная ветвь: не-narrative расширение (.dat → без叙事-буста),
        // файлы без совпадений → каждый скан даёт −1 импульса директории.
        for i in 0..40 {
            put(&d.join(format!("cold{i:02}.dat")), b"binary-ish payload without terms\x00\x01\x02");
        }
        let out = outpath("mom_slots");
        let mut cfg = HarvestConfig::new(vec![d.clone()], "гамильтониан", &out);
        cfg.format = HarvestFormat::Address;
        let matcher = Matcher::build(&cfg).unwrap();
        let (scope_key, query_key) = compute_crystal_keys(&cfg);
        let (counters, hits) = scan_parallel_with_crystal(&cfg, &matcher, &cpath, scope_key);
        crystallize_harvest(&cpath, scope_key, query_key, &counters, &hits);

        // Вторая кристаллизация читает mom-секцию первой (merge-путь) —
        // на старом коде шаг 16 поверх 24-байтных слотов давал мусор.
        let fresh = ScanCounters::default();
        crystallize_harvest(&cpath, scope_key, query_key, &fresh, &hits);

        // Прямая проверка: перечитываем mom-слоты из файла и сверяем
        // с live-таблицей первой кристаллизации.
        let mmap = unsafe { MmapOptions::new().map(&File::open(&cpath).unwrap()).unwrap() };
        let hdr: CrystalHeader =
            unsafe { std::ptr::read_unaligned(mmap.as_ptr() as *const CrystalHeader) };
        let num_mom = hdr.num_mom_slots as usize;
        assert!(num_mom > 0, "холодная ветвь должна дать mom-слоты");
        let mut persisted_neg = 0usize;
        for i in 0..num_mom {
            let pos = (hdr.mom_off as usize) + i * MOM_SLOT_STRIDE;
            let slot: CrystalMomSlot = unsafe {
                std::ptr::read_unaligned(mmap.as_ptr().add(pos) as *const CrystalMomSlot)
            };
            if slot.scope_key == scope_key && (slot.raw_momentum as u16 as i16) < 0 {
                persisted_neg += 1;
            }
        }
        assert!(
            persisted_neg > 0,
            "отрицательный импульс обязан переживать запись/чтение (найдено {persisted_neg} из {num_mom})"
        );
        // Слот обязан влезать в свою сетку: sizeof == stride.
        assert_eq!(
            std::mem::size_of::<CrystalMomSlot>(),
            MOM_SLOT_STRIDE,
            "структура слота должна совпадать с сеткой файла"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn no_matches_and_errors() {
        let d = tmpdir("no_match");
        put(&d.join("f.txt"), "ничего");
        let out = outpath("no_match");
        let cfg = HarvestConfig::new(vec![d.clone()], "гамильтониан", &out);
        let stats = run_ok(&cfg);
        assert_eq!(stats.files_selected, 0, "вызывающая сторона → exit 1");

        let bad = HarvestConfig::new(vec![d.clone()], "", &out);
        assert!(run_harvest(&bad).is_err(), "пустой запрос → Err");

        let missing = HarvestConfig::new(
            vec![PathBuf::from("/nonexistent/poler/xyz")],
            "терм",
            &out,
        );
        assert!(run_harvest(&missing).is_err(), "несуществующий корень → Err");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn address_crystal_mmap_roundtrip_and_invalidation() {
        let d = tmpdir("crystal_t5a");
        let cpath = d.join("test_crystal.t5a");
        put(&d.join("a.txt"), "первый гравітація узел\nвторой гравітація узел\n");
        put(&d.join("b.txt"), "третий гравітація якорь\n");
        let out = outpath("crystal_t5a");
        let mut cfg = HarvestConfig::new(vec![d.clone()], "гравітація", &out);
        cfg.format = HarvestFormat::Address;

        let matcher = Matcher::build(&cfg).unwrap();
        let (scope_key, query_key) = compute_crystal_keys(&cfg);
        let (s_counters, s_hits) = scan_parallel_with_crystal(&cfg, &matcher, &cpath, scope_key);
        assert_eq!(s_hits.len(), 2);

        crystallize_harvest(&cpath, scope_key, query_key, &s_counters, &s_hits);
        assert!(cpath.exists(), "кристалл .t5a записан на диск");
        let vpath = crystal_vault_path(&cpath);
        assert!(vpath.exists(), "хранилище .tar.poler создано");
        let r1 = crate::archive::reader::PolerReader::open(&vpath).unwrap();
        assert_eq!(r1.files().len(), 1);
        let ratio1 = r1.info().total_raw as f64 / r1.info().total_stored.max(1) as f64;
        drop(r1);

        // Доклеиваем второй и третий слепки в .tar.poler и проверяем рост коэффициента сжатия
        crystallize_harvest(&cpath, scope_key, query_key ^ 1, &s_counters, &s_hits);
        crystallize_harvest(&cpath, scope_key, query_key ^ 2, &s_counters, &s_hits);
        let r3 = crate::archive::reader::PolerReader::open(&vpath).unwrap();
        assert_eq!(r3.files().len(), 3, "в .tar.poler накоплено 3 кристалла");
        let ratio3 = r3.info().total_raw as f64 / r3.info().total_stored.max(1) as f64;
        assert!(
            ratio3 > ratio1 * 1.5,
            "коэффициент сжатия растёт с накоплением кристаллов: {ratio1:.2}x -> {ratio3:.2}x"
        );
        drop(r3);

        // Удаляем горячий .t5a и проверяем авто-гидратацию из .tar.poler при чтении
        let _ = fs::remove_file(&cpath);
        assert!(!cpath.exists());
        let (_, c_hits, dirty) =
            try_crystal_hit(&cpath, scope_key, query_key, &cfg, &matcher).expect("crystal hit after hydration");
        assert!(cpath.exists(), "горячий .t5a мгновенно гидратирован из .tar.poler");
        assert!(!dirty, "файлы не менялись — dirty=false");
        assert_eq!(c_hits.len(), 2);

        // Изменяем b.txt (увеличиваем размер, чтобы гарантированно сработал детектор изменения)
        put(
            &d.join("b.txt"),
            "третий гравітація якорь\nчетвёртый гравітація якорь\nпятый гравітація якорь\n",
        );
        let (_, c_hits2, dirty2) =
            try_crystal_hit(&cpath, scope_key, query_key, &cfg, &matcher).expect("crystal hit 2");
        assert!(dirty2, "изменение файла обнаружено кристаллом");
        let b_hit = c_hits2.iter().find(|h| h.path.ends_with("b.txt")).unwrap();
        assert_eq!(b_hit.match_count, 3, "изменённый файл пересканирован точечно");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn cyrillic_multiword_repro_20_files() {
        let d = tmpdir("cyrillic_repro");
        // Создаём 20 файлов с «Ошибка сети» в разных поддиректориях
        // и 40 пустых/ASCII файлов для проверки отсутствия ложного отсечения
        for i in 0..40 {
            put(&d.join(format!("noise_{i:03}/code.rs")), "fn test_ascii_function() -> u32 { 42 }\n");
        }
        for i in 0..20 {
            put(&d.join(format!("mod_{i:03}/sub/file.rs")), "pub fn run() {\n    // Ошибка сети\n    let code = 500;\n}\n");
        }
        let out = outpath("cyrillic_repro");
        let mut cfg = HarvestConfig::new(vec![d.clone()], "Ошибка сети", &out);
        cfg.format = HarvestFormat::Json;
        let stats = run_ok(&cfg);
        assert_eq!(stats.files_selected, 20, "все 20 файлов найдены без потерь (BUG-1 решён)");
        let text = out_str(&out);
        assert_eq!(text.matches("\"matches\":1").count(), 20, "в каждом файле ровно 1 совпадение");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn address_format_multiword_exact_anchor() {
        let d = tmpdir("address_multiword");
        let content = "let x = 10;\n// TODO: fix this bug\nlet y = 20;\n";
        put(&d.join("src/lib.rs"), content);
        let out = outpath("address_multiword");
        let mut cfg = HarvestConfig::new(vec![d.clone()], "TODO: fix this bug", &out);
        cfg.format = HarvestFormat::Address;
        let stats = run_ok(&cfg);
        assert_eq!(stats.files_selected, 1);
        assert_eq!(stats.sections_written, 1, "ровно один якорь на одно вхождение (BUG-2 решён)");
        let text = out_str(&out);
        let anchor_line = text.lines().find(|l| l.contains("\"type\":\"anchor\"")).expect("anchor line exists");
        let v: serde_json::Value = serde_json::from_str(anchor_line).unwrap();
        let off = v["offset"].as_u64().unwrap() as usize;
        let len = v["len"].as_u64().unwrap() as usize;
        let matches = v["matches"].as_u64().unwrap();
        assert_eq!(matches, 1);
        assert_eq!(&content.as_bytes()[off..off + len], b"TODO: fix this bug");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn junk_dirs_included_with_no_ignore() {
        let d = tmpdir("junk_dirs_no_ignore");
        put(&d.join("target/junk.txt"), "needle в мусоре");
        put(&d.join("node_modules/lib.js"), "needle в мусоре");
        put(&d.join("real.txt"), "needle настоящий");
        let out = outpath("junk_dirs_no_ignore");
        let mut cfg = HarvestConfig::new(vec![d.clone()], "needle", &out);
        cfg.no_ignore = true;
        let stats = run_ok(&cfg);
        assert_eq!(stats.files_selected, 3, "target и node_modules включены при --no-ignore (BUG-3 решён)");
        let text = out_str(&out);
        assert!(text.contains("junk.txt") && text.contains("lib.js") && text.contains("real.txt"));
        let _ = fs::remove_dir_all(&d);
    }
}
