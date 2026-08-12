//! Диагностика для процесса без консоли.
//!
//! Игра запускается без stdout, поэтому `println!` из инжектированной DLL
//! уходит в никуда — раньше все сообщения об ошибках просто терялись. Пишем
//! в два места сразу:
//!
//! * `OutputDebugStringW` — видно в DebugView и в подключённом отладчике,
//!   без задержек и без файловых операций в игровом потоке;
//! * файл в `%TEMP%` — переживает падение игры, что важно, когда падение
//!   и есть то, что мы диагностируем.

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::core::PCWSTR;

/// Имя файла лога внутри `%TEMP%`.
pub const LOG_FILE_NAME: &str = "BTamaCheat.log";

/// Путь к файлу лога — его показываем в интерфейсе, чтобы не искать вручную.
pub fn log_path() -> PathBuf {
    std::env::temp_dir().join(LOG_FILE_NAME)
}

/// Файл лога открывается один раз и лениво: если открыть не удалось
/// (нет прав на `%TEMP%`), остаётся только `OutputDebugStringW`.
fn log_file() -> Option<&'static Mutex<File>> {
    static FILE: OnceLock<Option<Mutex<File>>> = OnceLock::new();
    FILE.get_or_init(|| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path())
            .ok()
            .map(Mutex::new)
    })
    .as_ref()
}

/// Точка, куда сходятся все макросы логирования.
pub fn write(level: &str, args: std::fmt::Arguments<'_>) {
    let mut line = String::with_capacity(160);
    let _ = write!(line, "[BTamaCheat/{level}] {args}");

    let mut wide: Vec<u16> = line.encode_utf16().collect();
    wide.push(b'\n' as u16);
    wide.push(0);
    // SAFETY: `wide` — валидный, завершённый нулём UTF-16 буфер, живущий
    // дольше вызова. OutputDebugStringW только читает его.
    unsafe { OutputDebugStringW(PCWSTR(wide.as_ptr())) };

    if let Some(file) = log_file() {
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
