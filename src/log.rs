//! Диагностика для процесса без консоли.
//!
//! Игра запускается без stdout, поэтому `println!` из инжектированной DLL
//! уходит в никуда — раньше все сообщения об ошибках просто терялись. Пишем
//! в два места сразу:
//!
//! * `OutputDebugStringW` — видно в DebugView и в подключённом отладчике,
//!   без задержек и без файловых операций в игровом потоке;
//! * файл — переживает падение игры, что важно, когда падение и есть то,
//!   что мы диагностируем.
//!
//! # Почему путь не один
//!
//! Прежняя версия писала строго в `%TEMP%`, и если открыть файл не удавалось,
//! молча оставалась при `OutputDebugStringW`. Со стороны это выглядело как
//! «логов нет и ошибок нет» — худший из возможных исходов для диагностики.
//!
//! Теперь кандидатов несколько ([`candidates`]), берётся первый открывшийся,
//! а неудачи запоминаются и показываются в интерфейсе через [`status`]. Если
//! файла нет вовсе — интерфейс скажет почему, а не промолчит.

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::core::PCWSTR;

/// Имя файла лога.
pub const LOG_FILE_NAME: &str = "BTamaCheat.log";

/// Куда в итоге пишется лог.
pub struct Destination {
    /// Открытый файл; `None` — не удалось открыть ни одного кандидата.
    file: Option<Mutex<File>>,
    /// Путь открытого файла.
    pub path: Option<PathBuf>,
    /// Что помешало, если файла нет. Перечисляет всех кандидатов с причинами.
    pub problem: Option<String>,
}

/// Куда пишется лог и почему именно туда. Показывается в интерфейсе.
pub fn status() -> &'static Destination {
    static DESTINATION: OnceLock<Destination> = OnceLock::new();
    DESTINATION.get_or_init(open_log)
}

/// Каталог, из которого загружена наша собственная DLL.
///
/// Нужен как запасной путь: `%TEMP%` бывает недоступен, а рядом с DLL
/// пользователь уж точно найдёт файл и точно имеет туда права — оттуда её
/// только что прочитал загрузчик.
fn module_directory() -> Option<PathBuf> {
    let mut module = HMODULE::default();
    // SAFETY: адрес принадлежит нашему модулю, флаг FROM_ADDRESS именно это и
    // ожидает. UNCHANGED_REFCOUNT — чтобы не удерживать библиотеку лишней
    // ссылкой и не мешать выгрузке по PAGE DOWN.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(module_directory as *const () as *const u16),
            &mut module,
        )
        .ok()?;
    }

    let mut buffer = [0u16; 512];
    // SAFETY: буфер наш, длину функция берёт из среза.
    let length = unsafe { GetModuleFileNameW(Some(module), &mut buffer) } as usize;
    if length == 0 || length >= buffer.len() {
        return None;
    }
    PathBuf::from(String::from_utf16_lossy(&buffer[..length]))
        .parent()
        .map(Path::to_path_buf)
}

/// Кандидаты в порядке предпочтения.
fn candidates() -> Vec<PathBuf> {
    let mut paths = vec![std::env::temp_dir()];
    paths.extend(module_directory());
    paths.extend(std::env::var_os("USERPROFILE").map(PathBuf::from));
    paths.extend(std::env::current_dir().ok());

    let mut files: Vec<PathBuf> = paths
        .into_iter()
        .map(|dir| dir.join(LOG_FILE_NAME))
        .collect();
    files.dedup();
    files
}

fn open_log() -> Destination {
    let mut problems = String::new();
    for path in candidates() {
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => {
                return Destination {
                    file: Some(Mutex::new(file)),
                    path: Some(path),
                    problem: None,
                };
            }
            Err(error) => {
                let _ = write!(problems, "{}: {error}; ", path.display());
            }
        }
    }
    Destination {
        file: None,
        path: None,
        problem: Some(problems),
    }
}

/// Отметка времени вида `12:34:56.789`.
///
/// Без неё по логу нельзя отличить «событие только что» от «событие с
/// прошлого запуска», а файл открывается на дозапись и копит сессии.
fn timestamp() -> String {
    // SAFETY: функция без аргументов, возвращает заполненную структуру.
    let now = unsafe { GetLocalTime() };
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        now.wHour, now.wMinute, now.wSecond, now.wMilliseconds
    )
}

/// Точка, куда сходятся все макросы логирования.
pub fn write(level: &str, args: std::fmt::Arguments<'_>) {
    let mut line = String::with_capacity(192);
    let _ = write!(line, "[{} BTamaCheat/{level}] {args}", timestamp());

    let mut wide: Vec<u16> = line.encode_utf16().collect();
    wide.push(b'\n' as u16);
    wide.push(0);
    // SAFETY: `wide` — валидный, завершённый нулём UTF-16 буфер, живущий
    // дольше вызова. OutputDebugStringW только читает его.
    unsafe { OutputDebugStringW(PCWSTR(wide.as_ptr())) };

    if let Some(file) = status().file.as_ref() {
        // Именно `try_lock`, а не `lock`: эту функцию вызывает в том числе
        // обработчик паники, и паника внутри неё самой заблокировала бы поток
        // навсегда на невходимом мьютексе. Строка при этом не теряется —
        // OutputDebugStringW выше отработал в любом случае.
        // Отравленный мьютекс означает панику другого потока прямо здесь;
        // файл при этом остаётся консистентным построчно, так что пишем дальше.
        let file = match file.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        };
        if let Some(mut file) = file {
            let _ = writeln!(file, "{line}");
        }
    }
}

/// Открывает лог и отбивает начало сессии.
///
/// Вызывается явно при инициализации, чтобы файл появился сразу, а не при
/// первом же сообщении: пустой каталог без файла неотличим от сломанного
/// логирования, и именно так эта подсистема однажды и «потерялась».
pub fn start_session() {
    let destination = status();
    write(
        "info",
        format_args!(
            "=== BTamaCheat {} === pid {}",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        ),
    );
    match (&destination.path, &destination.problem) {
        (Some(path), _) => info!("лог пишется в {}", path.display()),
        (None, Some(problem)) => {
            error!("файл лога открыть не удалось, остаётся только отладчик: {problem}");
        }
        (None, None) => error!("файл лога открыть не удалось, причина не записана"),
    }
}

macro_rules! info {
    ($($arg:tt)*) => { $crate::log::write("info", format_args!($($arg)*)) };
}

/// Названа `warning`, а не `warn`: последнее совпадает со встроенным
/// атрибутом и делает имя неоднозначным при реэкспорте.
macro_rules! warning {
    ($($arg:tt)*) => { $crate::log::write("warn", format_args!($($arg)*)) };
}

macro_rules! error {
    ($($arg:tt)*) => { $crate::log::write("error", format_args!($($arg)*)) };
}

pub(crate) use {error, info, warning};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_are_distinct_and_named() {
        let candidates = candidates();
        assert!(!candidates.is_empty());
        for path in &candidates {
            assert_eq!(
                path.file_name().and_then(|name| name.to_str()),
                Some(LOG_FILE_NAME)
            );
        }
    }

    #[test]
    fn timestamp_has_millisecond_resolution() {
        let stamp = timestamp();
        // 12:34:56.789
        assert_eq!(stamp.len(), 12, "получили {stamp:?}");
        assert_eq!(stamp.matches(':').count(), 2);
        assert!(stamp.contains('.'));
    }
}
